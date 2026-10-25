//! Per-message jitter, perturbation B (`docs/m5-design.md` §7.2).
//!
//! A jittered link delivers each message `j` cycles later than its latency says, with
//! `0 ≤ j ≤ max_cycles`. `j` is derived from the seed, the directed link, and the
//! message's ordinal on it, so it does not depend on the order in which links are used.
//! A link stays FIFO: a delivery is never earlier than the link's previous one.
//!
//! A minimized setting (`docs/m5-design.md` §16) also lists the only messages it may
//! delay; every other draw is 0.

use std::sync::Arc;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::time::Tick;

/// BLAKE3 `derive_key` context for jitter draws. Part of the contract: changing it
/// changes every perturbed run.
pub const JITTER_CONTEXT: &str = "SystemScope 2026-10 jitter v1";

/// A set of links, by declaration index (bit `i` is link `i`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LinkSet(pub u128);

impl LinkSet {
    /// The set holding exactly `links`, each below 128.
    pub fn of(links: &[u32]) -> LinkSet {
        LinkSet(links.iter().fold(0, |set, &i| set | 1u128 << i))
    }

    /// Whether link `index` is in the set.
    pub fn contains(self, index: u32) -> bool {
        index < 128 && self.0 >> index & 1 == 1
    }

    /// The links in the set, in ascending order.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (0..128).filter(move |&i| self.contains(i))
    }
}

/// One message, by directed link and ordinal: `(link, dir, ordinal)`.
pub type MessageId = (u32, u8, u64);

/// Perturbation B's setting: part of the session configuration.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Jitter {
    /// Seeds every draw.
    pub seed: u64,
    /// The largest extra delay, in cycles of the link's own clock domain. At least 1.
    pub max_cycles: u64,
    /// The jittered links. Nonempty, and each must have a `Cycles` latency.
    pub links: LinkSet,
    /// The only messages that may be delayed, ascending and each on a jittered link, or
    /// `None` for every message. A message outside the list draws 0.
    pub only: Option<Arc<[MessageId]>>,
}

impl Jitter {
    /// Appends the canonical encoding of the seed, `max_cycles`, and the links. The list
    /// of messages, if any, is encoded by [`Jitter::encode_only`].
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.seed);
        e.u64(self.max_cycles);
        e.u128(self.links.0);
    }

    /// Reads a setting without a list of messages.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Jitter, DecodeError> {
        Ok(Jitter {
            seed: d.u64()?,
            max_cycles: d.u64()?,
            links: LinkSet(d.u128()?),
            only: None,
        })
    }

    /// Appends the list of messages, which must be present: its length, then each
    /// message's link, direction, and ordinal.
    pub fn encode_only(&self, e: &mut Encoder) {
        let only = self.only.as_deref().expect("a list of messages");
        e.len(only.len());
        for &(link, dir, ordinal) in only {
            e.u32(link);
            e.u8(dir);
            e.u64(ordinal);
        }
    }

    /// Reads a list of messages into `self`, refusing one out of order.
    pub fn decode_only(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        let mut only: Vec<MessageId> = Vec::new();
        for _ in 0..d.len()? {
            let id = (d.u32()?, d.u8()?, d.u64()?);
            if only.last().is_some_and(|&last| last >= id) {
                return Err(DecodeError::InvalidTag {
                    what: "message order",
                    tag: 0,
                });
            }
            only.push(id);
        }
        self.only = Some(only.into());
        Ok(())
    }

    /// The extra cycles of message `ordinal` sent on `link` from its `a` end (`dir` 0) or
    /// its `b` end (`dir` 1).
    pub fn draw(&self, link: u32, dir: u8, ordinal: u64) -> u64 {
        if let Some(only) = &self.only
            && only.binary_search(&(link, dir, ordinal)).is_err()
        {
            return 0;
        }
        let mut material = [0u8; 21];
        material[..8].copy_from_slice(&self.seed.to_le_bytes());
        material[8..12].copy_from_slice(&link.to_le_bytes());
        material[12] = dir;
        material[13..].copy_from_slice(&ordinal.to_le_bytes());
        let out = blake3::derive_key(JITTER_CONTEXT, &material);
        let word = u64::from_le_bytes(out[..8].try_into().expect("8 bytes"));
        // `max_cycles` is `u64::MAX` at most, when every word is in range.
        match self.max_cycles.checked_add(1) {
            Some(span) => word % span,
            None => word,
        }
    }
}

/// One direction of a jittered link: what makes the next draw and keeps it FIFO.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lane {
    /// Messages sent so far. Restored as found, so sending past `u64::MAX` is an error.
    pub sent: u64,
    /// The latest delivery tick, or 0 before the first.
    pub last: Tick,
}

/// Perturbation B's runtime state: its setting and one lane per direction of each
/// jittered link, in link order, `a` to `b` first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitterState {
    /// The setting.
    pub jitter: Jitter,
    /// `(link, lanes)` for each jittered link.
    pub lanes: Vec<(u32, [Lane; 2])>,
}

impl JitterState {
    /// Fresh lanes for `jitter`.
    pub fn new(jitter: Jitter) -> JitterState {
        JitterState {
            lanes: jitter
                .links
                .iter()
                .map(|i| (i, [Lane::default(); 2]))
                .collect(),
            jitter,
        }
    }

    /// The lane for messages sent on `link` from end `dir`, if the link is jittered.
    pub fn lane(&mut self, link: u32, dir: u8) -> Option<&mut Lane> {
        self.lanes
            .iter_mut()
            .find(|(i, _)| *i == link)
            .map(|(_, lanes)| &mut lanes[usize::from(dir)])
    }

    /// Appends the canonical encoding: the setting, then every lane. The list of
    /// messages, if any, is not part of it.
    pub fn encode(&self, e: &mut Encoder) {
        self.jitter.encode(e);
        self.encode_lanes(e);
    }

    /// Appends every lane.
    pub fn encode_lanes(&self, e: &mut Encoder) {
        e.len(self.lanes.len());
        for (_, lanes) in &self.lanes {
            for lane in lanes {
                e.u64(lane.sent);
                e.u64(lane.last.0);
            }
        }
    }

    /// Reads a setting and its lanes, unchecked: see [`JitterState::with_lanes`].
    pub fn decode(d: &mut Decoder<'_>) -> Result<(Jitter, Vec<[Lane; 2]>), DecodeError> {
        let jitter = Jitter::decode(d)?;
        Ok((jitter, JitterState::decode_lanes(d)?))
    }

    /// Reads lanes, unchecked.
    pub fn decode_lanes(d: &mut Decoder<'_>) -> Result<Vec<[Lane; 2]>, DecodeError> {
        let mut lanes = Vec::new();
        for _ in 0..d.len()? {
            let mut lane = || -> Result<Lane, DecodeError> {
                Ok(Lane {
                    sent: d.u64()?,
                    last: Tick(d.u64()?),
                })
            };
            lanes.push([lane()?, lane()?]);
        }
        Ok(lanes)
    }

    /// The state of `jitter` with `lanes`, or `None` unless there is exactly one pair of
    /// lanes per jittered link.
    pub fn with_lanes(jitter: Jitter, lanes: Vec<[Lane; 2]>) -> Option<JitterState> {
        let mut state = JitterState::new(jitter);
        if lanes.len() != state.lanes.len() {
            return None;
        }
        for ((_, slot), found) in state.lanes.iter_mut().zip(lanes) {
            *slot = found;
        }
        Some(state)
    }
}
