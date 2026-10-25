//! A scripted run's choice points (`docs/m6-design.md` §8): its tie groups with more
//! than one admissible order, its messages on scoped links, and the wakes of its
//! asynchronous components, each with its branch point.
//!
//! A [`Recording`] is a run as the choice points need it: every dispatched event, with
//! the counters of the events scheduled while it was dispatched. Which event scheduled
//! which gives a group's causal edges, and identities give its FIFO edges.

use std::ops::Range;

use crate::jitter::{LinkSet, MessageId};
use crate::runtime::{Dispatched, Runtime, RuntimeError};
use crate::script::{EventId, WakeId};
use crate::ties;

/// The largest group whose admissible orders [`Group::count`] counts: its table has
/// `2^n` entries.
pub const MAX_COUNTED: usize = 20;

/// One dispatched event and what its handler scheduled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The event.
    pub event: Dispatched,
    /// The counters of the events scheduled while it was dispatched.
    pub scheduled: Range<u64>,
}

/// A scripted run, recorded from `init` on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recording {
    /// The counters of the events `init` scheduled.
    pub init: Range<u64>,
    /// Every dispatched event, in order.
    pub steps: Vec<Step>,
}

impl Recording {
    /// Starts a recording of `rt`, which must have just run `init`.
    pub fn after_init(rt: &Runtime) -> Recording {
        Recording {
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

    /// The branch position of the event with `counter`: 0 if `init` scheduled it, and
    /// `i + 1` if step `i` did. A checkpoint after `k` events precedes it if `k` is less.
    pub fn scheduled_at(&self, counter: u64) -> u64 {
        if self.init.contains(&counter) {
            return 0;
        }
        let i = self.steps.partition_point(|s| s.scheduled.end <= counter);
        debug_assert!(
            self.steps
                .get(i)
                .is_some_and(|s| s.scheduled.contains(&counter))
        );
        i as u64 + 1
    }
}

/// Where a deviation first changes its run (`docs/m6-design.md` §8.4), in their total
/// order: by position, then a delay or a shift before a group, then by identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchPoint {
    /// The scheduling's branch position, as [`Recording::scheduled_at`] gives it.
    pub at: u64,
    /// 0 for a delay or a shift, 1 for a group order.
    pub kind: u8,
    /// The delayed message, the shifted wake, or the group's least identity.
    pub id: EventId,
}

/// A tie group: the events dispatched at one `(tick, phase)`, in the run's order, with
/// the edges an admissible order keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// The events, in the run's order.
    pub events: Vec<EventId>,
    /// The position of each event's step in the recording.
    pub steps: Vec<usize>,
    /// `preds[j]`: a bit for every member that must come before member `j`.
    pub preds: Vec<u64>,
}

/// A choice point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// A tie group with more than one admissible order.
    Group(Group),
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
    /// A group has more members than an order can name (64).
    GroupTooLarge,
}

impl Group {
    /// Whether some order other than the run's own is admissible: some member has no
    /// edge from the member just before it.
    pub fn has_alternative(&self) -> bool {
        (1..self.events.len()).any(|j| self.preds[j] >> (j - 1) & 1 == 0)
    }

    /// The number of admissible orders, the run's own included, if the group has at
    /// most [`MAX_COUNTED`] members; saturating.
    pub fn count(&self) -> Option<u64> {
        let n = self.events.len();
        if n > MAX_COUNTED {
            return None;
        }
        let mut ways = vec![0u64; 1 << n];
        ways[0] = 1;
        for mask in 0..(1usize << n) {
            let w = ways[mask];
            if w == 0 {
                continue;
            }
            for j in 0..n {
                if mask >> j & 1 == 0 && self.preds[j] & !(mask as u64) == 0 {
                    let next = mask | 1 << j;
                    ways[next] = ways[next].saturating_add(w);
                }
            }
        }
        Some(ways[(1 << n) - 1])
    }

    /// Every admissible order other than the run's own, in ascending order of their
    /// member positions.
    pub fn orders(&self) -> Orders<'_> {
        Orders {
            group: self,
            current: (0..self.events.len()).collect(),
        }
    }

    /// The group order an order of member positions gives: its events in that order.
    pub fn order(&self, positions: &[usize]) -> Vec<EventId> {
        positions.iter().map(|&j| self.events[j]).collect()
    }
}

/// The admissible orders of a group after its run's own, as member positions.
pub struct Orders<'a> {
    group: &'a Group,
    current: Vec<usize>,
}

impl Iterator for Orders<'_> {
    type Item = Vec<usize>;

    fn next(&mut self) -> Option<Vec<usize>> {
        let preds = &self.group.preds;
        let n = self.current.len();
        // The rightmost position whose member can be replaced by a larger admissible
        // one; the rest is then filled with the least admissible members.
        for i in (0..n).rev() {
            let placed: u64 = self.current[..i].iter().fold(0, |m, &j| m | 1 << j);
            let ready = |j: usize, placed: u64| placed >> j & 1 == 0 && preds[j] & !placed == 0;
            let Some(next) = (self.current[i] + 1..n).find(|&j| ready(j, placed)) else {
                continue;
            };
            self.current.truncate(i);
            self.current.push(next);
            let mut placed = placed | 1 << next;
            while self.current.len() < n {
                let j = (0..n)
                    .find(|&j| ready(j, placed))
                    .expect("a DAG has a source");
                self.current.push(j);
                placed |= 1 << j;
            }
            return Some(self.current.clone());
        }
        None
    }
}

/// The recording's choice points (`docs/m6-design.md` §8.1) whose branch points are
/// after `after`, in branch-point order: its tie groups with more than one admissible
/// order, its messages on the links of `scope`, and the wakes of the components in
/// `asynchronous`.
pub fn choice_points(
    rec: &Recording,
    scope: LinkSet,
    asynchronous: &[u32],
    after: Option<BranchPoint>,
) -> Result<Vec<ChoicePoint>, ChoiceError> {
    let steps = &rec.steps;
    if steps.iter().any(|s| s.event.id.is_none()) {
        return Err(ChoiceError::Unscripted);
    }
    let mut out = Vec::new();
    let at = |i: usize| rec.scheduled_at(ties::counter(steps[i].event.key.sequence));
    let mut start = 0;
    while start < steps.len() {
        let key = steps[start].event.key;
        let end = start
            + steps[start..]
                .iter()
                .take_while(|s| s.event.key.tick == key.tick && s.event.key.phase == key.phase)
                .count();
        for (i, step) in steps.iter().enumerate().take(end).skip(start) {
            let Some(id) = step.event.id else { continue };
            let choice = match id {
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
                    at: at(i),
                    kind: 0,
                    id,
                },
                choice,
            });
        }
        if end - start > 1 {
            let group = group(rec, start..end)?;
            if group.has_alternative() {
                out.push(ChoicePoint {
                    branch: BranchPoint {
                        at: (start..end).map(at).min().expect("a group has members"),
                        kind: 1,
                        id: *group.events.iter().min().expect("a group has members"),
                    },
                    choice: Choice::Group(group),
                });
            }
        }
        start = end;
    }
    // A group is found once its last member is dispatched, so sort by branch point.
    out.sort_by_key(|c| c.branch);
    out.retain(|c| after.is_none_or(|a| c.branch > a));
    Ok(out)
}

/// The group of the steps in `range`, with its causal and FIFO edges.
fn group(rec: &Recording, range: Range<usize>) -> Result<Group, ChoiceError> {
    let n = range.len();
    if n > 64 {
        return Err(ChoiceError::GroupTooLarge);
    }
    let steps = &rec.steps[range.clone()];
    let events: Vec<EventId> = steps
        .iter()
        .map(|s| s.event.id.ok_or(ChoiceError::Unscripted))
        .collect::<Result<_, _>>()?;
    let mut preds = vec![0u64; n];
    for (j, s) in steps.iter().enumerate() {
        let counter = ties::counter(s.event.key.sequence);
        for (i, earlier) in steps.iter().enumerate() {
            let causal = earlier.scheduled.contains(&counter);
            let fifo = match (events[i], events[j]) {
                (
                    EventId::Msg {
                        link: a,
                        dir: x,
                        ordinal: p,
                    },
                    EventId::Msg {
                        link: b,
                        dir: y,
                        ordinal: q,
                    },
                ) => (a, x) == (b, y) && p < q,
                _ => false,
            };
            if causal || fifo {
                preds[j] |= 1 << i;
            }
        }
    }
    Ok(Group {
        events,
        steps: range.collect(),
        preds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wake(ordinal: u64) -> EventId {
        EventId::Wake {
            component: 0,
            ordinal,
        }
    }

    fn group(preds: Vec<u64>) -> Group {
        let n = preds.len();
        Group {
            events: (0..n as u64).map(wake).collect(),
            steps: (0..n).collect(),
            preds,
        }
    }

    /// Every permutation that keeps `preds`, by brute force.
    fn brute(preds: &[u64]) -> Vec<Vec<usize>> {
        fn go(preds: &[u64], prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
            let n = preds.len();
            if prefix.len() == n {
                out.push(prefix.clone());
                return;
            }
            for j in 0..n {
                let placed: u64 = prefix.iter().fold(0, |m, &i| m | 1 << i);
                if placed >> j & 1 == 0 && preds[j] & !placed == 0 {
                    prefix.push(j);
                    go(preds, prefix, out);
                    prefix.pop();
                }
            }
        }
        let mut out = Vec::new();
        go(preds, &mut Vec::new(), &mut out);
        out
    }

    #[test]
    fn orders_are_every_linear_extension_but_the_runs_own_in_order() {
        for preds in [
            vec![0, 0, 0],
            vec![0, 0b1, 0],
            vec![0, 0b1, 0b10],
            vec![0, 0, 0b11, 0],
            vec![0, 0b1, 0, 0b100, 0],
        ] {
            let g = group(preds.clone());
            let all = brute(&preds);
            assert_eq!(g.count(), Some(all.len() as u64), "{preds:?}");
            let others: Vec<Vec<usize>> = g.orders().collect();
            assert_eq!(others, all[1..], "{preds:?}");
            assert_eq!(g.has_alternative(), all.len() > 1, "{preds:?}");
        }
        assert_eq!(group(vec![0; 6]).count(), Some(720));
        assert_eq!(group(vec![0; MAX_COUNTED + 1]).count(), None);
    }
}
