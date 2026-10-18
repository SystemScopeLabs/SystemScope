//! Prefix and replay-window minimization (`docs/m4-design.md` §17): the reproducer of a
//! case from the latest checkpoints that still hold the report window, accepted only
//! when it fails the same way as the whole search.

use crate::backend::Capability;
use crate::bundle::{Bundle, bundled};
use crate::case::{InputVariant, Scenario};
use crate::checkpoint::Checkpoint;
use crate::compare::{StreamResult, Verdict};
use crate::error::EngineError;
use crate::profile::Plan;
use crate::report::{Builder, searched};
use crate::result::VerificationResult;

/// A minimized reproducer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Minimized {
    /// The bundle.
    pub bundle: Bundle,
    /// Its verification result, recomputed from its own material.
    pub result: VerificationResult,
    /// The primary boundaries each side's checkpoint covers, or `None` for its initial
    /// state.
    pub origins: [Option<u64>; 2],
}

/// Whether two primary-stream results fail the same way (§17.1): both diverge on the same
/// stream, with the same kind and the same differing fields and values. The ordinal may
/// differ.
pub fn same_failure(a: &StreamResult, b: &StreamResult) -> bool {
    match (a, b) {
        (
            StreamResult::Diverged {
                stream: s,
                divergence: x,
                ..
            },
            StreamResult::Diverged {
                stream: t,
                divergence: y,
                ..
            },
        ) => s == t && x == y,
        _ => false,
    }
}

/// Whether a candidate's verdict keeps the whole search's (§17.1): a primary divergence
/// must fail the same way, and anything else must be the same verdict.
pub fn accepts(
    full: &Result<Verdict, EngineError>,
    candidate: &Result<Verdict, EngineError>,
) -> bool {
    match (full, candidate) {
        (Ok(Verdict::Compared(a)), Ok(Verdict::Compared(b))) if !a.primary.agrees() => {
            same_failure(&a.primary, &b.primary)
        }
        _ => full == candidate,
    }
}

/// The replay window's origins (§17.2): the latest checkpoint on `stream` covering at
/// most `limit` boundaries that every side in `restores` holds, for those sides, and
/// `None` for the others. Without such a checkpoint every side starts from its initial
/// state.
pub fn window_origins<'a>(
    stream: &str,
    checkpoints: [&'a [Checkpoint]; 2],
    restores: [bool; 2],
    limit: u64,
) -> [Option<&'a Checkpoint>; 2] {
    let covering = |i: usize, covered: u64| {
        checkpoints[i]
            .iter()
            .find(|c| c.mark(stream).is_some_and(|m| m.covered == covered))
    };
    let common = (0..2)
        .filter(|&i| restores[i])
        .map(|i| {
            checkpoints[i]
                .iter()
                .filter_map(|c| c.mark(stream).map(|m| m.covered))
                .filter(|&covered| covered <= limit)
                .collect::<Vec<_>>()
        })
        .reduce(|a, b| a.into_iter().filter(|c| b.contains(c)).collect());
    match common.and_then(|c| c.into_iter().max()) {
        Some(covered) => [0, 1].map(|i| covering(i, covered).filter(|_| restores[i])),
        None => [None, None],
    }
}

/// Locates `inputs[1]` against `inputs[0]`, indexing each side that can step with a
/// checkpoint every `every` primary boundaries, then writes the reproducer from each
/// restoring side's latest checkpoint covering at most the divergence minus the report
/// window (§17.2). A side that cannot restore starts from its initial state, and an
/// agreement starts from the initial states. The reproducer must fail the same way as
/// the search, or this is an [`EngineError::Internal`].
pub fn minimize(
    plan: &Plan,
    scenario: &Scenario,
    sides: [Builder<'_>; 2],
    inputs: [InputVariant; 2],
    every: u64,
) -> Result<Minimized, EngineError> {
    let (full, indexes) = searched(plan, scenario, sides, inputs.clone(), every)?;
    let primary = |r: &VerificationResult| match &r.verdict {
        Ok(Verdict::Compared(c)) => Some(c.primary.clone()),
        _ => None,
    };
    let stream = plan.profile.primary.stream.as_str();
    let w = u64::from(plan.profile.report_window);
    let restores = sides.map(|s| s.make.build().capabilities().contains(&Capability::Restore));
    let origins = match primary(&full) {
        Some(StreamResult::Diverged { ordinal, .. }) => window_origins(
            stream,
            [&indexes[0].checkpoints, &indexes[1].checkpoints],
            restores,
            ordinal.saturating_sub(w),
        ),
        _ => [None, None],
    };
    let (bundle, result) = bundled(plan, scenario, sides, inputs, origins)?;
    if !accepts(&full.verdict, &result.verdict) {
        return Err(EngineError::Internal(format!(
            "the reproducer fails differently: {:?}, not {:?}",
            result.verdict, full.verdict
        )));
    }
    Ok(Minimized {
        bundle,
        result,
        origins: origins.map(|c| c.and_then(|c| c.mark(stream).map(|m| m.covered))),
    })
}
