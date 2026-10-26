//! Scripted schedules (`docs/m6-design.md` §7).
//!
//! A script lists deviations from the reference ordering: delays of single messages,
//! shifts of single wakes, and picks of single dispatches. It names events by
//! [`EventId`], what scheduled them, never by `sequence`, which a deviation changes for
//! every later event. A scripted session counts every directed link's messages and every
//! component's wakes to give each event its identity.
//!
//! A pick dispatches a ready event of the next `(tick, phase)` in place of the default
//! event, the one with the least counter (§7.2). Ready means pending with no earlier
//! message of its lane pending, so every dispatched order is admissible, whatever events
//! a pick adds or removes.

use std::collections::{BTreeMap, BTreeSet};

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::event::EventKey;
use systemscope_contracts::time::Tick;

use crate::jitter::MessageId;

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

/// A wake's identity as a shift names it: `(component, ordinal)`.
pub type WakeId = (u32, u64);

/// A script: part of the session configuration. The empty script still makes a scripted
/// session, which counts identities and writes the scripted snapshot format.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Script {
    /// `(message, cycles)`: the message is delivered `cycles` later, at least 1, on a
    /// `Cycles` link. Ascending by message.
    pub delays: Vec<(MessageId, u64)>,
    /// `(wake, cycles)`: the wake is dispatched `|cycles|` cycles earlier if negative,
    /// later if positive, in the clock domain it names (`docs/m6-design.md` §7.6).
    /// Ascending by wake, never 0.
    pub shifts: Vec<(WakeId, i64)>,
    /// `(default, events)`: the events dispatched, in order, at successive dispatches
    /// while `default` is the default event, at least one. Ascending by default; no
    /// identity twice, as a default or an event.
    pub picks: Vec<(EventId, Vec<EventId>)>,
}

/// Why a script is not canonical.
fn invalid(what: &'static str) -> DecodeError {
    DecodeError::InvalidTag { what, tag: 0 }
}

impl Script {
    /// Appends the delays: their count, then each message and its cycles; then the
    /// shifts: their count, then each wake and its cycles.
    pub fn encode_delays(&self, e: &mut Encoder) {
        e.len(self.delays.len());
        for &((link, dir, ordinal), cycles) in &self.delays {
            e.u32(link);
            e.u8(dir);
            e.u64(ordinal);
            e.u64(cycles);
        }
        e.len(self.shifts.len());
        for &((component, ordinal), cycles) in &self.shifts {
            e.u32(component);
            e.u64(ordinal);
            e.i64(cycles);
        }
    }

    /// Appends the picks: their count, then each default event, and its events' count
    /// and identities.
    pub fn encode_picks(&self, e: &mut Encoder) {
        e.len(self.picks.len());
        for (default, events) in &self.picks {
            default.encode(e);
            e.len(events.len());
            for id in events {
                id.encode(e);
            }
        }
    }

    /// Reads delays and shifts into `self`.
    pub fn decode_delays(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        self.delays.clear();
        for _ in 0..d.len()? {
            let id = (d.u32()?, d.u8()?, d.u64()?);
            self.delays.push((id, d.u64()?));
        }
        self.shifts.clear();
        for _ in 0..d.len()? {
            let id = (d.u32()?, d.u64()?);
            self.shifts.push((id, d.i64()?));
        }
        Ok(())
    }

    /// Reads picks into `self`.
    pub fn decode_picks(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        self.picks.clear();
        for _ in 0..d.len()? {
            let default = EventId::decode(d)?;
            let mut events = Vec::new();
            for _ in 0..d.len()? {
                events.push(EventId::decode(d)?);
            }
            self.picks.push((default, events));
        }
        Ok(())
    }

    /// Appends the canonical encoding: the delays and shifts, then the picks.
    pub fn encode(&self, e: &mut Encoder) {
        self.encode_delays(e);
        self.encode_picks(e);
    }

    /// Reads one script, refusing one that is not canonical.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Script, DecodeError> {
        let mut script = Script::default();
        script.decode_delays(d)?;
        script.decode_picks(d)?;
        script.check().map_err(invalid)?;
        Ok(script)
    }

    /// Its deviations: each delay, each shift, and each picked event.
    pub fn deviations(&self) -> usize {
        self.delays.len() + self.shifts.len() + self.picks.iter().map(|p| p.1.len()).sum::<usize>()
    }

    /// Checks what a script means without a topology: delays ascending and at least 1,
    /// shifts ascending and neither 0 nor `i64::MIN`, and picks ascending by default, each
    /// with an event, and no identity twice among them.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.delays.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err("the delays are not ascending");
        }
        if self.delays.iter().any(|&(_, cycles)| cycles == 0) {
            return Err("a delay is 0 cycles");
        }
        if self.shifts.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err("the shifts are not ascending");
        }
        if self.shifts.iter().any(|&(_, cycles)| cycles == 0) {
            return Err("a shift is 0 cycles");
        }
        if self.shifts.iter().any(|&(_, cycles)| cycles == i64::MIN) {
            return Err("a shift has no magnitude");
        }
        if self.picks.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err("the picks are not ascending");
        }
        if self.picks.iter().any(|(_, events)| events.is_empty()) {
            return Err("a default event has no pick");
        }
        let mut seen = BTreeSet::new();
        let named = self
            .picks
            .iter()
            .flat_map(|(default, events)| std::iter::once(default).chain(events));
        for id in named {
            if !seen.insert(*id) {
                return Err("a pick names an event twice");
            }
        }
        Ok(())
    }

    /// Every identity the picks name, defaults and events.
    fn picked(&self) -> impl Iterator<Item = EventId> + '_ {
        self.picks
            .iter()
            .flat_map(|(default, events)| std::iter::once(*default).chain(events.iter().copied()))
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
}

/// A scripted session's counts: every lane, every component's wakes, and the picks
/// taken ahead of the current default event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Two lanes per link, by declaration index, `a` to `b` first.
    pub lanes: Vec<[Lane; 2]>,
    /// Wakes scheduled so far, by component id.
    pub wakes: Vec<u64>,
    /// `(default, n)`: the last dispatch picked the `n`-th event ahead of `default`,
    /// which is still pending; `None` if it dispatched its default.
    pub ahead: Option<(EventId, u64)>,
}

/// A scripted session's runtime state: the script and every identity count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptState {
    /// The script.
    pub script: Script,
    delays: BTreeMap<MessageId, u64>,
    shifts: BTreeMap<WakeId, i64>,
    picks: BTreeMap<EventId, Vec<EventId>>,
    /// Two lanes per link, by declaration index, `a` to `b` first.
    pub lanes: Vec<[Lane; 2]>,
    /// Wakes scheduled so far, by component id.
    pub wakes: Vec<u64>,
    /// As [`Counts::ahead`].
    pub ahead: Option<(EventId, u64)>,
    /// See [`ScriptState::duplicates`].
    duplicates: Vec<EventId>,
}

impl ScriptState {
    /// Fresh counts for `script` on a topology of `links` links and `components`
    /// components.
    pub fn new(script: Script, links: usize, components: usize) -> ScriptState {
        ScriptState::with_counts(
            script,
            Counts {
                lanes: vec![[Lane::default(); 2]; links],
                wakes: vec![0; components],
                ahead: None,
            },
        )
    }

    /// The state of `script` with the given counts.
    pub fn with_counts(script: Script, counts: Counts) -> ScriptState {
        ScriptState {
            delays: script.delays.iter().copied().collect(),
            shifts: script.shifts.iter().copied().collect(),
            picks: script.picks.iter().cloned().collect(),
            script,
            lanes: counts.lanes,
            wakes: counts.wakes,
            ahead: counts.ahead,
            duplicates: Vec::new(),
        }
    }

    /// The counts, apart from the script.
    pub fn counts(&self) -> Counts {
        Counts {
            lanes: self.lanes.clone(),
            wakes: self.wakes.clone(),
            ahead: self.ahead,
        }
    }

    /// The events whose delay or shift took effect as a duplicate (`docs/m6-design.md`
    /// §9.2) since this state began, in order: the event got the tick the next smaller
    /// `d` or `|s|` of the same sign gives, the parent's for 1. Not part of a snapshot,
    /// so empty after a restore or a fork.
    pub fn duplicates(&self) -> &[EventId] {
        &self.duplicates
    }

    /// Notes that `id`'s delay or shift took effect as a duplicate.
    pub(crate) fn duplicated(&mut self, id: EventId) {
        self.duplicates.push(id);
    }

    /// The extra cycles of `message`, or 0.
    pub fn delay(&self, message: MessageId) -> u64 {
        self.delays.get(&message).copied().unwrap_or(0)
    }

    /// The cycles `wake` is shifted by, or 0.
    pub fn shift(&self, wake: WakeId) -> i64 {
        self.shifts.get(&wake).copied().unwrap_or(0)
    }

    /// The event the script picks at a dispatch whose default event is `default`, if any:
    /// the entry after the picks already taken ahead of it.
    pub fn pick(&self, default: EventId) -> Option<EventId> {
        let taken = match self.ahead {
            Some((d, n)) if d == default => n,
            _ => 0,
        };
        let events = self.picks.get(&default)?;
        usize::try_from(taken)
            .ok()
            .and_then(|i| events.get(i))
            .copied()
    }

    /// Records a dispatch whose default event was `default`: `picked` if the script's
    /// pick was dispatched in its place.
    pub fn dispatched(&mut self, default: EventId, picked: bool) {
        self.ahead = if picked {
            let taken = match self.ahead {
                Some((d, n)) if d == default => n,
                _ => 0,
            };
            Some((default, taken + 1))
        } else {
            None
        };
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
        let shifts = self
            .script
            .shifts
            .iter()
            .map(|&((component, ordinal), _)| EventId::Wake { component, ordinal });
        let mut out: Vec<EventId> = delays
            .chain(shifts)
            .chain(self.script.picked())
            .filter(|&id| !self.scheduled(id))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Appends the counts: every lane, then every wake count, then the picks taken ahead
    /// of the current default event.
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
        match self.ahead {
            None => e.u8(0),
            Some((default, n)) => {
                e.u8(1);
                default.encode(e);
                e.u64(n);
            }
        }
    }

    /// Reads counts, unchecked.
    pub fn decode_counts(d: &mut Decoder<'_>) -> Result<Counts, DecodeError> {
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
        let ahead = match d.u8()? {
            0 => None,
            1 => Some((EventId::decode(d)?, d.u64()?)),
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "option",
                    tag,
                });
            }
        };
        Ok(Counts {
            lanes,
            wakes,
            ahead,
        })
    }

    /// Whether `child` extends this state's script with entries that have not taken
    /// effect yet: what a fork needs (`docs/m6-design.md` §9.3). A new delay or shift's
    /// event must not be scheduled yet, and a new pick's default must not be dispatched
    /// yet: unscheduled, or `pending`. Each of the parent's pick lists must begin the
    /// child's list for its default.
    pub fn admits_fork(&self, child: &Script, pending: impl Fn(EventId) -> bool) -> bool {
        let parent = &self.script;
        let delays_kept = parent.delays.iter().all(|d| child.delays.contains(d));
        let shifts_kept = parent.shifts.iter().all(|s| child.shifts.contains(s));
        let picks_kept = parent.picks.iter().all(|(default, events)| {
            child
                .picks
                .iter()
                .any(|(d, e)| d == default && e.starts_with(events))
        });
        let new_delays = child
            .delays
            .iter()
            .filter(|d| !parent.delays.contains(d))
            .map(|&((link, dir, ordinal), _)| EventId::Msg { link, dir, ordinal });
        let new_shifts = child
            .shifts
            .iter()
            .filter(|s| !parent.shifts.contains(s))
            .map(|&((component, ordinal), _)| EventId::Wake { component, ordinal });
        let new_picks = child
            .picks
            .iter()
            .filter(|(d, e)| !parent.picks.iter().any(|p| p.0 == *d && p.1 == *e))
            .map(|(d, _)| *d);
        delays_kept
            && shifts_kept
            && picks_kept
            && !new_delays.chain(new_shifts).any(|id| self.scheduled(id))
            && new_picks
                .into_iter()
                .all(|d| !self.scheduled(d) || pending(d))
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
            shifts: vec![((1, 2), -40), ((2, 0), 3)],
            picks: vec![
                (msg(0, 1, 9), vec![wake(2, 5)]),
                (wake(0, 0), vec![msg(3, 0, 1), msg(3, 0, 0)]),
            ],
        }
    }

    #[test]
    fn a_script_round_trips_and_picks_in_order() {
        for script in [Script::default(), sample()] {
            let mut e = Encoder::new();
            script.encode(&mut e);
            let bytes = e.into_bytes();
            let mut d = Decoder::new(&bytes);
            assert_eq!(Script::decode(&mut d), Ok(script.clone()));
            assert!(d.finish().is_ok());
        }
        let mut state = ScriptState::new(sample(), 5, 3);
        assert_eq!(state.pick(msg(0, 1, 9)), Some(wake(2, 5)));
        assert_eq!(state.pick(wake(0, 0)), Some(msg(3, 0, 1)));
        assert_eq!(state.pick(msg(3, 0, 1)), None);
        state.dispatched(wake(0, 0), true);
        assert_eq!(state.ahead, Some((wake(0, 0), 1)));
        assert_eq!(state.pick(wake(0, 0)), Some(msg(3, 0, 0)));
        // The count belongs to its default event.
        assert_eq!(state.pick(msg(0, 1, 9)), Some(wake(2, 5)));
        state.dispatched(wake(0, 0), true);
        assert_eq!(state.pick(wake(0, 0)), None);
        state.dispatched(wake(0, 0), false);
        assert_eq!(state.ahead, None);
        assert_eq!(state.delay((4, 1, 0)), 7);
        assert_eq!(state.delay((4, 1, 1)), 0);
        assert_eq!(state.shift((1, 2)), -40);
        assert_eq!(state.shift((2, 1)), 0);
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
            refused(sample(), |s| s.shifts.swap(0, 1)),
            "the shifts are not ascending"
        );
        assert_eq!(
            refused(sample(), |s| s.shifts[1].1 = 0),
            "a shift is 0 cycles"
        );
        assert_eq!(
            refused(sample(), |s| s.shifts[0].1 = i64::MIN),
            "a shift has no magnitude"
        );
        assert_eq!(
            refused(sample(), |s| s.picks.swap(0, 1)),
            "the picks are not ascending"
        );
        assert_eq!(
            refused(sample(), |s| s.picks[0].1.clear()),
            "a default event has no pick"
        );
        assert_eq!(
            refused(sample(), |s| s.picks[1].1[0] = wake(2, 5)),
            "a pick names an event twice"
        );
        assert_eq!(
            refused(sample(), |s| s.picks[1].1[1] = wake(0, 0)),
            "a pick names an event twice"
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
        let none = |_| false;
        // The sample delays (1, 0, 3), already sent: no fork may add it.
        assert!(!state.admits_fork(&sample(), none));
        let later = Script {
            delays: vec![((1, 0, 4), 1)],
            shifts: vec![((2, 7), -1)],
            picks: vec![(wake(2, 6), vec![msg(1, 1, 0)])],
        };
        // A shift of wake (2, 5), already scheduled, is refused as the delay is.
        assert!(!state.admits_fork(
            &Script {
                shifts: vec![((2, 5), 1)],
                ..Script::default()
            },
            none
        ));
        assert!(state.admits_fork(&later, none));
        // A pick ahead of wake (2, 5) needs it still pending.
        let ahead_of_5 = Script {
            picks: vec![(wake(2, 5), vec![msg(1, 1, 0)])],
            ..Script::default()
        };
        assert!(!state.admits_fork(&ahead_of_5, none));
        assert!(state.admits_fork(&ahead_of_5, |id| id == wake(2, 5)));
        let forked = ScriptState::with_counts(later.clone(), state.counts());
        // A fork keeps the parent's entries, and extends a pick list only at its end.
        assert!(!forked.admits_fork(&Script::default(), none));
        let mut longer = later.clone();
        longer.picks[0].1.push(wake(2, 8));
        assert!(forked.admits_fork(&longer, none));
        let mut before = later.clone();
        before.picks[0].1.insert(0, wake(2, 8));
        assert!(!forked.admits_fork(&before, none));
        assert_eq!(
            forked.unscheduled(),
            vec![msg(1, 0, 4), msg(1, 1, 0), wake(2, 6), wake(2, 7)]
        );
    }
}
