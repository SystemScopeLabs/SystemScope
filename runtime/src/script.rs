//! Scripted schedules (`docs/m6-design.md` §7).
//!
//! A script lists deviations from the reference ordering: delays of single messages, and
//! orders of tie groups. It names events by [`EventId`], what scheduled them, never by
//! `sequence`, which a deviation changes for every later event. A scripted session counts
//! every directed link's messages and every component's wakes to give each event its
//! identity.
//!
//! A group order gives each event it lists a rank, its position in the list. The
//! scheduler then raises the rank to its floor (causal) and to the rank of the previous
//! message of its lane in the same `(tick, phase)` (FIFO), so every dispatched order is
//! admissible, whatever events a reordering adds or removes.

use std::collections::BTreeMap;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::event::{EventKey, Phase};
use systemscope_contracts::time::Tick;

use crate::jitter::MessageId;
use crate::ties::RANK_BITS;

/// An event's identity in a scripted session (`docs/m6-design.md` §7.1): the `n`-th
/// message accepted for one direction of a link, or the `n`-th wake a component
/// scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventId {
    /// Message `ordinal` sent on `link` from its `a` end (`dir` 0) or its `b` end (1).
    Msg {
        /// The link's declaration index.
        link: u32,
        /// The sending end.
        dir: u8,
        /// Messages sent on that lane before it.
        ordinal: u64,
    },
    /// Wake `ordinal` scheduled by `component`, including those from its `Init`.
    Wake {
        /// The component's id.
        component: u32,
        /// Wakes it scheduled before this one.
        ordinal: u64,
    },
}

impl EventId {
    /// Appends the canonical encoding: tag 0 and the message, or tag 1 and the wake.
    pub fn encode(&self, e: &mut Encoder) {
        match *self {
            EventId::Msg { link, dir, ordinal } => {
                e.u8(0);
                e.u32(link);
                e.u8(dir);
                e.u64(ordinal);
            }
            EventId::Wake { component, ordinal } => {
                e.u8(1);
                e.u32(component);
                e.u64(ordinal);
            }
        }
    }

    /// Reads one identity.
    pub fn decode(d: &mut Decoder<'_>) -> Result<EventId, DecodeError> {
        match d.u8()? {
            0 => Ok(EventId::Msg {
                link: d.u32()?,
                dir: d.u8()?,
                ordinal: d.u64()?,
            }),
            1 => Ok(EventId::Wake {
                component: d.u32()?,
                ordinal: d.u64()?,
            }),
            tag => Err(DecodeError::InvalidTag {
                what: "event identity",
                tag,
            }),
        }
    }
}

/// A script: part of the session configuration. The empty script still makes a scripted
/// session, which counts identities and writes the scripted snapshot format.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Script {
    /// `(message, cycles)`: the message is delivered `cycles` later, at least 1, on a
    /// `Cycles` link. Ascending by message.
    pub delays: Vec<(MessageId, u64)>,
    /// Group orders, each its events in the order chosen, at least two. An event's rank
    /// is its position. Ascending by each group's least identity; no identity twice.
    pub groups: Vec<Vec<EventId>>,
}

/// Why a script is not canonical.
fn invalid(what: &'static str) -> DecodeError {
    DecodeError::InvalidTag { what, tag: 0 }
}

impl Script {
    /// Appends the delays: their count, then each message and its cycles.
    pub fn encode_delays(&self, e: &mut Encoder) {
        e.len(self.delays.len());
        for &((link, dir, ordinal), cycles) in &self.delays {
            e.u32(link);
            e.u8(dir);
            e.u64(ordinal);
            e.u64(cycles);
        }
    }

    /// Appends the group orders: their count, then each group's length and identities.
    pub fn encode_groups(&self, e: &mut Encoder) {
        e.len(self.groups.len());
        for group in &self.groups {
            e.len(group.len());
            for id in group {
                id.encode(e);
            }
        }
    }

    /// Reads delays into `self`.
    pub fn decode_delays(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        self.delays.clear();
        for _ in 0..d.len()? {
            let id = (d.u32()?, d.u8()?, d.u64()?);
            self.delays.push((id, d.u64()?));
        }
        Ok(())
    }

    /// Reads group orders into `self`.
    pub fn decode_groups(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        self.groups.clear();
        for _ in 0..d.len()? {
            let mut group = Vec::new();
            for _ in 0..d.len()? {
                group.push(EventId::decode(d)?);
            }
            self.groups.push(group);
        }
        Ok(())
    }

    /// Appends the canonical encoding: the delays, then the group orders.
    pub fn encode(&self, e: &mut Encoder) {
        self.encode_delays(e);
        self.encode_groups(e);
    }

    /// Reads one script, refusing one that is not canonical.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Script, DecodeError> {
        let mut script = Script::default();
        script.decode_delays(d)?;
        script.decode_groups(d)?;
        script.check().map_err(invalid)?;
        Ok(script)
    }

    /// Checks what a script means without a topology: delays ascending and at least 1,
    /// groups of two or more, ascending by least identity, no identity twice, and every
    /// rank below `2^RANK_BITS`.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.delays.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err("the delays are not ascending");
        }
        if self.delays.iter().any(|&(_, cycles)| cycles == 0) {
            return Err("a delay is 0 cycles");
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut least = None;
        for group in &self.groups {
            if group.len() < 2 {
                return Err("a group order lists fewer than two events");
            }
            if group.len() as u64 > 1 << RANK_BITS {
                return Err("a group order lists too many events");
            }
            if group.iter().any(|id| !seen.insert(*id)) {
                return Err("an event is listed twice");
            }
            let min = group.iter().min().copied();
            if least >= min {
                return Err("the group orders are not ascending");
            }
            least = min;
        }
        Ok(())
    }

    /// Every identity the script names, with its rank if it is in a group order.
    fn ranks(&self) -> BTreeMap<EventId, u64> {
        self.groups
            .iter()
            .flat_map(|g| (0u64..).zip(g).map(|(rank, id)| (*id, rank)))
            .collect()
    }
}

/// One direction of a link in a scripted session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lane {
    /// Messages accepted so far.
    pub sent: u64,
    /// The key of the latest one, or `None` before the first.
    pub last: Option<EventKey>,
}

impl Lane {
    /// The latest delivery tick, or 0 before the first.
    pub fn last_tick(&self) -> Tick {
        self.last.map_or(Tick::ZERO, |k| k.tick)
    }

    /// The rank a message to `(tick, phase)` must reach to stay behind the previous one.
    pub fn fifo_rank(&self, tick: Tick, phase: Phase) -> u64 {
        match self.last {
            Some(k) if k.tick == tick && k.phase == phase => crate::ties::rank(k.sequence),
            _ => 0,
        }
    }
}

/// A scripted session's runtime state: the script and every identity count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptState {
    /// The script.
    pub script: Script,
    delays: BTreeMap<MessageId, u64>,
    ranks: BTreeMap<EventId, u64>,
    /// Two lanes per link, by declaration index, `a` to `b` first.
    pub lanes: Vec<[Lane; 2]>,
    /// Wakes scheduled so far, by component id.
    pub wakes: Vec<u64>,
}

impl ScriptState {
    /// Fresh counts for `script` on a topology of `links` links and `components`
    /// components.
    pub fn new(script: Script, links: usize, components: usize) -> ScriptState {
        ScriptState::with_counts(
            script,
            vec![[Lane::default(); 2]; links],
            vec![0; components],
        )
    }

    /// The state of `script` with the given counts.
    pub fn with_counts(script: Script, lanes: Vec<[Lane; 2]>, wakes: Vec<u64>) -> ScriptState {
        ScriptState {
            delays: script.delays.iter().copied().collect(),
            ranks: script.ranks(),
            script,
            lanes,
            wakes,
        }
    }

    /// The extra cycles of `message`, or 0.
    pub fn delay(&self, message: MessageId) -> u64 {
        self.delays.get(&message).copied().unwrap_or(0)
    }

    /// The script rank of `id`, or 0 if no group order lists it.
    pub fn rank(&self, id: EventId) -> u64 {
        self.ranks.get(&id).copied().unwrap_or(0)
    }

    /// Whether `id` was already scheduled, by the counts.
    pub fn scheduled(&self, id: EventId) -> bool {
        match id {
            EventId::Msg { link, dir, ordinal } => self
                .lanes
                .get(link as usize)
                .and_then(|l| l.get(usize::from(dir)))
                .is_some_and(|l| ordinal < l.sent),
            EventId::Wake { component, ordinal } => self
                .wakes
                .get(component as usize)
                .is_some_and(|&n| ordinal < n),
        }
    }

    /// The script's entries that name no event the run has scheduled so far: vacuous if
    /// the run has ended (`docs/m6-design.md` §7.1).
    pub fn unscheduled(&self) -> Vec<EventId> {
        let delays = self
            .script
            .delays
            .iter()
            .map(|&((link, dir, ordinal), _)| EventId::Msg { link, dir, ordinal });
        let ranked = self.script.groups.iter().flatten().copied();
        let mut out: Vec<EventId> = delays
            .chain(ranked)
            .filter(|&id| !self.scheduled(id))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Appends the counts: every lane, then every wake count.
    pub fn encode_counts(&self, e: &mut Encoder) {
        e.len(self.lanes.len());
        for lanes in &self.lanes {
            for lane in lanes {
                e.u64(lane.sent);
                match lane.last {
                    None => e.u8(0),
                    Some(key) => {
                        e.u8(1);
                        key.encode(e);
                    }
                }
            }
        }
        e.len(self.wakes.len());
        for &n in &self.wakes {
            e.u64(n);
        }
    }

    /// Reads counts, unchecked.
    pub fn decode_counts(d: &mut Decoder<'_>) -> Result<(Vec<[Lane; 2]>, Vec<u64>), DecodeError> {
        let mut lanes = Vec::new();
        for _ in 0..d.len()? {
            let mut lane = || -> Result<Lane, DecodeError> {
                let sent = d.u64()?;
                let last = match d.u8()? {
                    0 => None,
                    1 => Some(EventKey::decode(d)?),
                    tag => {
                        return Err(DecodeError::InvalidTag {
                            what: "option",
                            tag,
                        });
                    }
                };
                Ok(Lane { sent, last })
            };
            lanes.push([lane()?, lane()?]);
        }
        let mut wakes = Vec::new();
        for _ in 0..d.len()? {
            wakes.push(d.u64()?);
        }
        Ok((lanes, wakes))
    }

    /// Whether `child` extends this state's script with entries none of whose events
    /// were scheduled yet: what a fork needs (`docs/m6-design.md` §9.3).
    pub fn admits_fork(&self, child: &Script) -> bool {
        let parent = &self.script;
        let delays_kept = parent.delays.iter().all(|d| child.delays.contains(d));
        let groups_kept = parent.groups.iter().all(|g| child.groups.contains(g));
        let new_delays = child
            .delays
            .iter()
            .filter(|d| !parent.delays.contains(d))
            .map(|&((link, dir, ordinal), _)| EventId::Msg { link, dir, ordinal });
        let new_ranked = child
            .groups
            .iter()
            .filter(|g| !parent.groups.contains(g))
            .flatten()
            .copied();
        delays_kept && groups_kept && !new_delays.chain(new_ranked).any(|id| self.scheduled(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(link: u32, dir: u8, ordinal: u64) -> EventId {
        EventId::Msg { link, dir, ordinal }
    }

    fn wake(component: u32, ordinal: u64) -> EventId {
        EventId::Wake { component, ordinal }
    }

    fn sample() -> Script {
        Script {
            delays: vec![((1, 0, 3), 2), ((4, 1, 0), 7)],
            groups: vec![
                vec![wake(2, 5), msg(0, 1, 9)],
                vec![msg(3, 0, 1), wake(0, 0), msg(3, 0, 0)],
            ],
        }
    }

    #[test]
    fn a_script_round_trips_and_ranks_by_position() {
        for script in [Script::default(), sample()] {
            let mut e = Encoder::new();
            script.encode(&mut e);
            let bytes = e.into_bytes();
            let mut d = Decoder::new(&bytes);
            assert_eq!(Script::decode(&mut d), Ok(script.clone()));
            assert!(d.finish().is_ok());
        }
        let state = ScriptState::new(sample(), 5, 3);
        assert_eq!(state.rank(msg(0, 1, 9)), 1);
        assert_eq!(state.rank(msg(3, 0, 0)), 2);
        assert_eq!(state.rank(msg(9, 0, 0)), 0);
        assert_eq!(state.delay((4, 1, 0)), 7);
        assert_eq!(state.delay((4, 1, 1)), 0);
    }

    #[test]
    fn a_script_that_is_not_canonical_is_refused() {
        let refused = |mut s: Script, mutate: fn(&mut Script)| {
            mutate(&mut s);
            s.check().unwrap_err()
        };
        assert_eq!(
            refused(sample(), |s| s.delays.swap(0, 1)),
            "the delays are not ascending"
        );
        assert_eq!(
            refused(sample(), |s| s.delays[0].1 = 0),
            "a delay is 0 cycles"
        );
        assert_eq!(
            refused(sample(), |s| s.groups[0].truncate(1)),
            "a group order lists fewer than two events"
        );
        assert_eq!(
            refused(sample(), |s| s.groups[1][0] = wake(2, 5)),
            "an event is listed twice"
        );
        assert_eq!(
            refused(sample(), |s| s.groups.swap(0, 1)),
            "the group orders are not ascending"
        );
    }

    #[test]
    fn counts_decide_what_was_scheduled_and_what_a_fork_admits() {
        let mut state = ScriptState::new(Script::default(), 5, 3);
        state.lanes[1][0].sent = 4;
        state.wakes[2] = 6;
        assert!(state.scheduled(msg(1, 0, 3)));
        assert!(!state.scheduled(msg(1, 0, 4)));
        assert!(!state.scheduled(msg(1, 1, 0)));
        assert!(state.scheduled(wake(2, 5)));
        assert!(!state.scheduled(msg(9, 0, 0)));
        // The sample delays (1, 0, 3), already sent: no fork may add it.
        assert!(!state.admits_fork(&sample()));
        let later = Script {
            delays: vec![((1, 0, 4), 1)],
            groups: vec![vec![wake(2, 6), msg(1, 1, 0)]],
        };
        assert!(state.admits_fork(&later));
        let forked =
            ScriptState::with_counts(later.clone(), state.lanes.clone(), state.wakes.clone());
        // A fork keeps the parent's entries.
        assert!(!forked.admits_fork(&Script::default()));
        assert_eq!(
            forked.unscheduled(),
            vec![msg(1, 0, 4), msg(1, 1, 0), wake(2, 6)]
        );
    }
}
