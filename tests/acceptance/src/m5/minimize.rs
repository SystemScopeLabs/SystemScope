//! Perturbation minimization (`docs/m5-design.md` §16): a finding's perturbation reduced
//! to the parts it needs. A candidate is kept only if a full run under it fails the way
//! the whole perturbation does, by M4's [`same_failure`].
//!
//! - A: link overrides and the slice are removed one at a time; each remaining value is
//!   then bisected toward its frozen one.
//! - B: the messages the jitter delays are reduced by delta debugging to a 1-minimal
//!   list, run as a listed jitter ([`Jitter::only`]).
//!
//! The search runs offline; its results are pinned, and a test checks that each still
//! fails the same way and that removing any one part loses the failure.

use std::sync::Arc;

use systemscope_runtime::jitter::{Jitter, MessageId};
use systemscope_rv32::m5;
use systemscope_rv32::m5ref::SLICE;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::runner::Start;
use systemscope_verify::bundle::{self, Bundle, Case, ReplayError};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{Recording, StreamResult, Verdict, compare_recordings};
use systemscope_verify::error::EngineError;
use systemscope_verify::minimize::same_failure;
use systemscope_verify::profile::{Plan, Role};
use systemscope_verify::report::{Builder, Make};
use systemscope_verify::result::VerificationResult;

use super::Fixture;
use super::campaign::{Subject, record};
use super::evidence::applied_jitter;
use super::verify::{self, defective_config};

/// The latency of every link on the frozen platform, in CPU cycles.
pub const FROZEN_LINK_CYCLES: u64 = 1;

/// One part of an A perturbation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// A link's latency override.
    Link(u32),
    /// The slice.
    Slice,
}

/// The parts of `p`'s A perturbation.
pub fn parts(p: &Perturbation) -> Vec<Part> {
    let mut parts: Vec<Part> = p.latencies.links.keys().map(|&i| Part::Link(i)).collect();
    if p.slice.is_some() {
        parts.push(Part::Slice);
    }
    parts
}

/// `p` without `part`: that link or the slice back to its frozen value.
pub fn without(p: &Perturbation, part: Part) -> Perturbation {
    let mut q = p.clone();
    match part {
        Part::Link(i) => {
            q.latencies.links.remove(&i);
        }
        Part::Slice => q.slice = None,
    }
    q
}

/// `p`'s jitter limited to `only`.
pub fn listed(p: &Perturbation, only: &[MessageId]) -> Perturbation {
    let mut q = p.clone();
    let jitter = q.jitter.as_mut().expect("a B perturbation");
    jitter.only = Some(Arc::from(only));
    q
}

/// The primary result of `perturbed` against `base`, if it diverged.
pub fn failure(plan: &Plan, base: &Recording, perturbed: &Recording) -> Option<StreamResult> {
    match compare_recordings(plan, base, perturbed) {
        Ok(Verdict::Compared(c)) if !c.primary.agrees() => Some(c.primary),
        _ => None,
    }
}

/// A finding to minimize: a subject, its unperturbed recording, and the failure its whole
/// perturbation gives.
pub struct Finding<'a> {
    /// The workload.
    pub fixture: &'a Fixture,
    /// The plan both sides share.
    pub plan: &'a Plan,
    /// The unpatched scenario or the race.
    pub subject: Subject,
    /// The subject's unperturbed recording.
    pub base: &'a Recording,
    /// The failure to keep.
    pub failure: StreamResult,
}

impl Finding<'_> {
    /// Whether a full run under `p` fails as the finding does.
    pub fn reproduces(&self, p: &Perturbation) -> bool {
        let r = record(self.fixture, self.subject, p);
        failure(self.plan, self.base, &r).is_some_and(|f| same_failure(&self.failure, &f))
    }
}

/// Reduces A perturbation `p` (§16): parts are removed while the failure stays, then each
/// value is bisected toward its frozen one, until neither changes anything. Removing any
/// one part of the result loses the failure.
pub fn minimize_a(finding: &Finding<'_>, p: &Perturbation) -> Perturbation {
    let mut p = p.clone();
    loop {
        let mut changed = false;
        for part in parts(&p) {
            let q = without(&p, part);
            if finding.reproduces(&q) {
                p = q;
                changed = true;
            }
        }
        for part in parts(&p) {
            let (frozen, value) = match part {
                Part::Link(i) => (FROZEN_LINK_CYCLES, p.latencies.links[&i]),
                Part::Slice => (u64::from(SLICE), u64::from(p.slice.expect("a slice"))),
            };
            let with = |v: u64| {
                let mut q = p.clone();
                match part {
                    Part::Link(i) => {
                        q.latencies.links.insert(i, v);
                    }
                    Part::Slice => q.slice = Some(u32::try_from(v).expect("a slice")),
                }
                q
            };
            // `near` loses the failure (it is the frozen value, or was tried), `far` keeps it.
            let (mut near, mut far) = (frozen, value);
            while near.abs_diff(far) > 1 {
                let mid = near.midpoint(far);
                if finding.reproduces(&with(mid)) {
                    far = mid;
                } else {
                    near = mid;
                }
            }
            if far != value {
                p = with(far);
                changed = true;
            }
        }
        if !changed {
            return p;
        }
    }
}

/// The messages the jitter of `p` delays in `subject`'s run, in delivery order.
pub fn delayed(
    fixture: &Fixture,
    subject: Subject,
    p: &Perturbation,
) -> Result<Vec<MessageId>, String> {
    let mut disk = fixture.disk.clone();
    if let InputVariant::Media(patch) = input(fixture, subject)? {
        patch.apply(&mut disk)?;
    }
    let config = defective_config(p, subject.0.and_then(|r| r.defect()));
    let run = m5::run_with(
        &fixture.firmware,
        &disk,
        &config,
        Start::Init { traced: true },
        Vec::new(),
    );
    let trace = run.finished.trace.ok_or("the run kept no trace")?;
    let jitter: &Jitter = p.jitter.as_ref().ok_or("not a B perturbation")?;
    let (_, applied) = applied_jitter(&trace, jitter, u64::MAX)?;
    Ok(applied
        .into_iter()
        .map(|a| (a.link, a.dir, a.ordinal))
        .collect())
}

/// `ids` in ascending order, as a listed jitter holds them.
fn sorted(ids: &[MessageId]) -> Vec<MessageId> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids
}

/// Reduces B perturbation `p` (§16): the shortest prefix, in delivery order, of the
/// messages it delays that keeps the failure is found by bisection, since delays after
/// the divergence cannot cause it; delta debugging then reduces that prefix to a
/// 1-minimal list.
pub fn minimize_b(finding: &Finding<'_>, p: &Perturbation) -> Result<Perturbation, String> {
    let all = delayed(finding.fixture, finding.subject, p)?;
    let keeps = |ids: &[MessageId]| finding.reproduces(&listed(p, &sorted(ids)));
    if !keeps(&all) {
        return Err("listing every delayed message loses the failure".to_owned());
    }
    // `short` loses the failure, `long` keeps it.
    let (mut short, mut long) = (0, all.len());
    while long - short > 1 {
        let mid = short.midpoint(long);
        if keeps(&all[..mid]) {
            long = mid;
        } else {
            short = mid;
        }
    }
    let only = ddmin(all[..long].to_vec(), keeps);
    Ok(listed(p, &sorted(&only)))
}

/// Zeller's delta debugging: a subsequence of `c`, which `test` accepts, from which
/// removing any one element makes `test` refuse it. `test(c)` must hold.
pub fn ddmin<T: Clone>(mut c: Vec<T>, mut test: impl FnMut(&[T]) -> bool) -> Vec<T> {
    let mut n = 2;
    while c.len() >= 2 {
        let size = c.len().div_ceil(n);
        let chunks: Vec<Vec<T>> = c.chunks(size).map(<[T]>::to_vec).collect();
        let n_chunks = chunks.len();
        if let Some(s) = chunks.iter().find(|s| test(s)) {
            c = s.clone();
            n = 2;
            continue;
        }
        let complement = |k: usize| -> Vec<T> {
            chunks
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != k)
                .flat_map(|(_, s)| s.iter().cloned())
                .collect()
        };
        if n_chunks > 2
            && let Some(rest) = (0..n_chunks).map(complement).find(|r| test(r))
        {
            c = rest;
            n = (n - 1).max(2);
            continue;
        }
        if n_chunks >= c.len() {
            break;
        }
        n = (n * 2).min(c.len());
    }
    c
}

/// The input both sides of `subject` run: its race's patch, if any.
fn input(fixture: &Fixture, subject: Subject) -> Result<InputVariant, String> {
    subject
        .0
        .map_or(Ok(InputVariant::None), |race| race.variant(&fixture.disk))
}

/// Verifies `subject` under `perturbations[1]` against `perturbations[0]` and bundles the
/// reproducer (§18): each perturbed side holds its `perturbation.bin`.
pub fn reproducer(
    fixture: &Fixture,
    subject: Subject,
    perturbations: [&Perturbation; 2],
) -> Result<Bundle, EngineError> {
    let input = input(fixture, subject).map_err(EngineError::Internal)?;
    let defect = subject.0.and_then(|r| r.defect());
    let makes = perturbations.map(|p| move || verify::defective_backend(fixture, p, defect));
    let bytes = perturbations.map(|p| (!p.is_empty()).then(|| p.to_bytes()));
    let sides = [0, 1].map(|i| Builder {
        make: Make::Runtime(&makes[i]),
        config: verify::config(),
        perturbation: bytes[i].as_deref(),
    });
    bundle::write(
        &super::campaign::the_plan(fixture),
        &verify::scenario(fixture),
        sides,
        [input.clone(), input],
        [None, None],
    )
}

/// Replays reproducer `b` of `subject`, each side built under the perturbation its
/// `perturbation.bin` holds, or none without one.
pub fn replay(
    fixture: &Fixture,
    subject: Subject,
    b: &Bundle,
) -> Result<VerificationResult, ReplayError> {
    let corrupt = |why: String| ReplayError::Corrupt(EngineError::CorruptReproducer(why));
    let mut perturbations = Vec::new();
    for role in [Role::Expected, Role::Actual] {
        perturbations.push(match bundle::perturbation(b, role) {
            None => Perturbation::default(),
            Some(bytes) => Perturbation::from_bytes(bytes)
                .map_err(|e| corrupt(format!("{role:?} perturbation.bin: {e:?}")))?,
        });
    }
    let defect = subject.0.and_then(|r| r.defect());
    let makes: Vec<_> = perturbations
        .iter()
        .map(|p| move || verify::defective_backend(fixture, p, defect))
        .collect();
    let bytes: Vec<_> = perturbations
        .iter()
        .map(|p| (!p.is_empty()).then(|| p.to_bytes()))
        .collect();
    let sides = [0, 1].map(|i| Builder {
        make: Make::Runtime(&makes[i]),
        config: verify::config(),
        perturbation: bytes[i].as_deref(),
    });
    let plan = super::campaign::the_plan(fixture);
    let scenario = verify::scenario(fixture);
    bundle::replay(
        b,
        Case {
            plan: &plan,
            scenario: &scenario,
            sides,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `test` holds when `c` contains every element of `needed`.
    fn minimized(len: u32, needed: &[u32]) -> (Vec<u32>, usize) {
        let mut calls = 0;
        let c = ddmin((0..len).collect(), |c| {
            calls += 1;
            needed.iter().all(|x| c.contains(x))
        });
        (c, calls)
    }

    #[test]
    fn delta_debugging_finds_the_needed_elements() {
        assert_eq!(minimized(1, &[0]).0, [0]);
        assert_eq!(minimized(1000, &[517]).0, [517]);
        assert_eq!(minimized(1000, &[3, 998]).0, [3, 998]);
        assert_eq!(minimized(64, &[1, 2, 3, 40]).0, [1, 2, 3, 40]);
        assert_eq!(minimized(10, &[]).0.len(), 1);
        let (_, calls) = minimized(100_000, &[12_345]);
        assert!(calls < 100, "{calls}");
    }

    #[test]
    fn parts_come_off_one_at_a_time() {
        let mut p = Perturbation::default();
        p.latencies.links.insert(3, 5);
        p.latencies.links.insert(7, 2);
        p.slice = Some(4000);
        assert_eq!(parts(&p), [Part::Link(3), Part::Link(7), Part::Slice]);
        let q = without(&without(&p, Part::Link(3)), Part::Slice);
        assert_eq!(parts(&q), [Part::Link(7)]);
        assert!(without(&q, Part::Link(7)).is_empty());
    }
}
