//! A scripted run's choice points (`docs/m6-design.md` §8): its dispatches with a ready
//! event other than the default, its messages on scoped links, and the wakes of its
//! asynchronous components, each with its branch point.
//!
//! A [`Recording`] is a run as the choice points need it: every dispatched event, with
//! the counters of the events scheduled while it was dispatched. Which events were
//! pending at a dispatch follows from the counters, and identities give the FIFO lanes.
//!
//! A recording may begin at a checkpoint of the search (§9.4), which is never inside a
//! `(tick, phase)`: its choice points after a branch point past the checkpoint are those
//! of the whole run, since every event scheduled before the checkpoint has a branch
//! position at or before it.

use std::ops::Range;

use crate::jitter::{LinkSet, MessageId};
use crate::runtime::{Dispatched, Runtime, RuntimeError};
use crate::script::{EventId, WakeId};

/// One dispatched event and what its handler scheduled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The event.
    pub event: Dispatched,
    /// The counters of the events scheduled while it was dispatched.
    pub scheduled: Range<u64>,
}

/// A scripted run, recorded from `init` or from a checkpoint on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recording {
    /// The events dispatched before the recording began: 0 from `init`.
    pub start: u64,
    /// The counters of the events scheduled before the first recorded step: by `init`,
    /// and from a checkpoint, by everything before it.
    pub init: Range<u64>,
    /// Every dispatched event since, in order.
    pub steps: Vec<Step>,
}

impl Recording {
    /// Starts a recording of `rt`, which must have just run `init`.
    pub fn after_init(rt: &Runtime) -> Recording {
        Recording {
            start: 0,
            init: 0..rt.next_sequence(),
            steps: Vec::new(),
        }
    }

    /// Starts a recording of `rt`, which must have just been restored or forked from a
    /// checkpoint after `events` events, taken where no `(tick, phase)` began before it.
    pub fn after_checkpoint(rt: &Runtime, events: u64) -> Recording {
        Recording {
            start: events,
            init: 0..rt.next_sequence(),
            steps: Vec::new(),
        }
    }

    /// Dispatches `rt`'s next event and records it; `None` once the queue is empty.
    pub fn step(&mut self, rt: &mut Runtime) -> Result<Option<&Step>, RuntimeError> {
        let before = rt.next_sequence();
        let Some(event) = rt.step()? else {
            return Ok(None);
        };
        self.steps.push(Step {
            event,
            scheduled: before..rt.next_sequence(),
        });
        Ok(self.steps.last())
    }

    /// The branch position of the event with `counter`: `start + i + 1` if step `i`
    /// scheduled it, and `start` if it was scheduled before the recording began, at that
    /// position or before. A checkpoint after `k` events precedes it if `k` is less.
    pub fn scheduled_at(&self, counter: u64) -> u64 {
        if self.init.contains(&counter) {
            return self.start;
        }
        let i = self.steps.partition_point(|s| s.scheduled.end <= counter);
        debug_assert!(
            self.steps
                .get(i)
                .is_some_and(|s| s.scheduled.contains(&counter))
        );
        self.start + i as u64 + 1
    }
}

/// Where a deviation first changes its run (`docs/m6-design.md` §8.4), in their total
/// order: by position, then a pick before a delay or a shift, then by identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchPoint {
    /// The branch position: [`Recording::scheduled_at`] for a scheduling, and
    /// `start + i + 1` for the dispatch of step `i`.
    pub at: u64,
    /// 0 for a pick, 1 for a delay or a shift.
    pub kind: u8,
    /// The pick's default event, the delayed message, or the shifted wake.
    pub id: EventId,
}

/// A choice point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// A dispatch whose default event is `default`, with the other events ready at it,
    /// ascending: each may be picked in its place.
    Pick {
        /// The event the dispatch would dispatch.
        default: EventId,
        /// The other ready events.
        ready: Vec<EventId>,
    },
    /// A message on a scoped link, to be delayed `1..=D` cycles.
    Delay(MessageId),
    /// A wake of an asynchronous component, to be shifted `±1..=E` cycles.
    Shift(WakeId),
}

/// A choice point and its branch point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoicePoint {
    /// Where it branches.
    pub branch: BranchPoint,
    /// What it varies.
    pub choice: Choice,
}

/// Why a recording has no choice points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceError {
    /// A dispatched event has no identity: the run was not scripted.
    Unscripted,
}

/// The recording's choice points (`docs/m6-design.md` §8.1) whose branch points are
/// after `after`, in branch-point order: its dispatches with a ready event other than the
/// default, its messages on the links of `scope`, and the wakes of the components in
/// `asynchronous`.
pub fn choice_points(
    rec: &Recording,
    scope: LinkSet,
    asynchronous: &[u32],
    after: Option<BranchPoint>,
) -> Result<Vec<ChoicePoint>, ChoiceError> {
    let steps = &rec.steps;
    let ids: Vec<EventId> = steps
        .iter()
        .map(|s| s.event.id.ok_or(ChoiceError::Unscripted))
        .collect::<Result<_, _>>()?;
    // A scripted session's sequences are its counters.
    let counter = |i: usize| steps[i].event.key.sequence;
    let mut out = Vec::new();
    let mut start = 0;
    while start < steps.len() {
        let key = steps[start].event.key;
        let end = start
            + steps[start..]
                .iter()
                .take_while(|s| s.event.key.tick == key.tick && s.event.key.phase == key.phase)
                .count();
        for i in start..end {
            // Pending at step i: the group's events from i on scheduled before it ran.
            let pending: Vec<usize> = (i..end)
                .filter(|&j| counter(j) < steps[i].scheduled.start)
                .collect();
            let default = *pending
                .iter()
                .min_by_key(|&&j| counter(j))
                .expect("the dispatched event is pending");
            let mut ready: Vec<EventId> = pending
                .iter()
                .filter(|&&j| j != default && !behind(ids[j], pending.iter().map(|&p| ids[p])))
                .map(|&j| ids[j])
                .collect();
            if !ready.is_empty() {
                ready.sort();
                out.push(ChoicePoint {
                    branch: BranchPoint {
                        at: rec.start + i as u64 + 1,
                        kind: 0,
                        id: ids[default],
                    },
                    choice: Choice::Pick {
                        default: ids[default],
                        ready,
                    },
                });
            }
            let choice = match ids[i] {
                EventId::Msg { link, dir, ordinal } if scope.contains(link) => {
                    Choice::Delay((link, dir, ordinal))
                }
                EventId::Wake { component, ordinal } if asynchronous.contains(&component) => {
                    Choice::Shift((component, ordinal))
                }
                _ => continue,
            };
            out.push(ChoicePoint {
                branch: BranchPoint {
                    at: rec.scheduled_at(counter(i)),
                    kind: 1,
                    id: ids[i],
                },
                choice,
            });
        }
        start = end;
    }
    // A delay or a shift is found at its dispatch, after its scheduling.
    out.sort_by_key(|c| c.branch);
    out.retain(|c| after.is_none_or(|a| c.branch > a));
    Ok(out)
}

/// Whether `id` is a message with an earlier message of its lane among `others`.
fn behind(id: EventId, mut others: impl Iterator<Item = EventId>) -> bool {
    let EventId::Msg { link, dir, ordinal } = id else {
        return false;
    };
    others.any(|o| {
        matches!(o, EventId::Msg { link: l, dir: d, ordinal: n } if (l, d) == (link, dir) && n < ordinal)
    })
}
