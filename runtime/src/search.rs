//! The search tree of `docs/m6-design.md` §9.2 and §9.8: the children of a scripted run,
//! each its script with one deviation more at a choice point after its parent's last.
//! Every script within the bounds is a node exactly once, since its parent is the script
//! without its deviation of latest branch point.
//!
//! A child is a duplicate when its delay or shift takes the tick the next smaller one
//! gives (§9.2). The runtime classifies it when it applies the deviation
//! ([`ScriptState::duplicates`](crate::script::ScriptState::duplicates)), so a duplicate
//! costs a fork up to its branch point, not a run. Driving the tree, with the stop rule,
//! the order, and the comparison, is the verification library's.

use crate::choice::{BranchPoint, Choice, ChoiceError, Recording, choice_points};
use crate::jitter::LinkSet;
use crate::script::Script;

/// The bounds of a search over one session (§9.1), apart from the budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// `k`: the most deviations in one script.
    pub deviations: usize,
    /// `D`: the most cycles per delay.
    pub delay: u64,
    /// `E`: the most cycles per shift, either way.
    pub shift: u64,
    /// The links whose messages may be delayed.
    pub scope: LinkSet,
    /// The components whose wakes may be shifted.
    pub asynchronous: Vec<u32>,
}

/// A node of the search tree other than its root, the empty script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    /// The parent's script with one deviation more.
    pub script: Script,
    /// The new deviation's branch point, after every branch point of the parent's.
    pub branch: BranchPoint,
}

/// The children of `script`, whose latest branch point is `last` (`None` for the empty
/// script), from `rec`, its run recorded to its stop rule: none once it has `k`
/// deviations. In branch-point order, and within a choice point, picks by event, delays by
/// `d`, and shifts earlier first, each way by `|s|`.
pub fn children(
    script: &Script,
    last: Option<BranchPoint>,
    rec: &Recording,
    bounds: &Bounds,
) -> Result<Vec<Child>, ChoiceError> {
    if script.deviations() >= bounds.deviations {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for point in choice_points(rec, bounds.scope, &bounds.asynchronous, last)? {
        let branch = point.branch;
        match point.choice {
            Choice::Pick { default, ready } => {
                for event in ready {
                    let mut child = script.clone();
                    match child.picks.iter_mut().find(|(d, _)| *d == default) {
                        Some((_, events)) => events.push(event),
                        None => {
                            child.picks.push((default, vec![event]));
                            child.picks.sort();
                        }
                    }
                    out.push(Child {
                        script: child,
                        branch,
                    });
                }
            }
            Choice::Delay(id) => {
                for d in 1..=bounds.delay {
                    let mut child = script.clone();
                    child.delays.push((id, d));
                    child.delays.sort();
                    out.push(Child {
                        script: child,
                        branch,
                    });
                }
            }
            Choice::Shift(id) => {
                let e = i64::try_from(bounds.shift).unwrap_or(i64::MAX);
                for s in (1..=e).map(|m| -m).chain(1..=e) {
                    let mut child = script.clone();
                    child.shifts.push((id, s));
                    child.shifts.sort();
                    out.push(Child {
                        script: child,
                        branch,
                    });
                }
            }
        }
    }
    Ok(out)
}
