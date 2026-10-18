//! First-divergence localization: lockstep scan, stream compare, and checkpoint search,
//! chosen by capability (`docs/m4-design.md` §11, §12.3).
//!
//! Every strategy reports the first divergence the naive oracle finds. Lockstep and stream
//! compare stop there; checkpoint search finds it from recorded prefix digests and
//! confirms it by direct comparison after restoring each side.

use std::collections::VecDeque;

use crate::backend::{Backend, Capabilities, Capability, Side, run};
use crate::case::{Scenario, execution_id};
use crate::checkpoint::Checkpoint;
use crate::compare::{
    self, Comparison, Divergence, Recorded, Recording, StreamResult, Verdict, compare_cut,
    compare_recordings,
};
use crate::error::EngineError;
use crate::observation::{Observation, PrefixDigest};
use crate::profile::{Incomparable, Plan, Role};

/// How a first divergence was located (§11.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Both sides step boundary by boundary.
    Lockstep,
    /// One side's streams are recorded whole; the other steps against them.
    StreamCompare,
    /// Binary search over recorded prefix digests, then a scan from checkpoints.
    Search,
    /// Neither side steps: both are recorded whole and compared.
    Recorded,
}

/// The primary observations around a first divergence, for the report (§11.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// The ordinal of the first observation kept: the divergence minus the report window,
    /// or 0.
    pub first: u64,
    /// The expected side's observations from `first`, up to the report window after the
    /// divergence, or fewer where its stream ended or it failed.
    pub expected: Vec<Observation>,
    /// The actual side's, likewise.
    pub actual: Vec<Observation>,
    /// The failures that cut a side's window short.
    pub failures: Vec<(Role, EngineError)>,
}

/// The result of locating the first divergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// The strategy used.
    pub strategy: Strategy,
    /// The comparison.
    pub verdict: Verdict,
    /// The report window, when the primary stream diverged.
    pub window: Option<Window>,
}

impl Located {
    /// The first divergence's ordinal and kind, if the primary stream diverged.
    pub fn divergence(&self) -> Option<(u64, &Divergence)> {
        match &self.verdict {
            Verdict::Compared(Comparison {
                primary:
                    StreamResult::Diverged {
                        ordinal,
                        divergence,
                        ..
                    },
                ..
            }) => Some((*ordinal, divergence)),
            _ => None,
        }
    }
}

/// Locates the first divergence with the strategy both sides' capabilities allow:
/// checkpoint search when `indexes` are given and both sides can step and restore,
/// lockstep when both can step, stream compare when one can, and whole recordings when
/// neither can. A capability a side does not declare is never used.
pub fn locate(
    plan: &Plan,
    scenario: &Scenario,
    expected: Side<'_>,
    actual: Side<'_>,
    indexes: Option<(&Index, &Index)>,
) -> Result<Located, EngineError> {
    let steps = |c: &Capabilities| c.contains(&Capability::StepBoundary);
    let restores = |c: &Capabilities| steps(c) && c.contains(&Capability::Restore);
    let (e, a) = (
        expected.backend.capabilities(),
        actual.backend.capabilities(),
    );
    match (steps(&e), steps(&a)) {
        (true, true) => match indexes {
            Some((ie, ia)) if restores(&e) && restores(&a) => {
                search(plan, scenario, (expected, ie), (actual, ia))
            }
            _ => lockstep(plan, scenario, expected, actual),
        },
        (true, false) => stream_compare(plan, scenario, Role::Actual, expected, actual),
        (false, true) => stream_compare(plan, scenario, Role::Expected, expected, actual),
        (false, false) => recorded(plan, scenario, expected, actual),
    }
}

/// Steps both sides one primary boundary at a time from their initial states, up to the
/// first divergence, the stop rule, or a failure. Requires
/// [`Capability::StepBoundary`] on both sides.
pub fn lockstep(
    plan: &Plan,
    scenario: &Scenario,
    mut expected: Side<'_>,
    mut actual: Side<'_>,
) -> Result<Located, EngineError> {
    start(plan, scenario, &mut expected)?;
    start(plan, scenario, &mut actual)?;
    scan(
        plan,
        Strategy::Lockstep,
        Cursor::Live(expected.backend),
        Cursor::Live(actual.backend),
    )
}

/// Records every stream of the `recorded` side as [`run`] does, then steps the other side
/// against it; a failure of the recorded side ends its replay. Requires
/// [`Capability::StepBoundary`] on the other side only.
pub fn stream_compare(
    plan: &Plan,
    scenario: &Scenario,
    recorded: Role,
    expected: Side<'_>,
    actual: Side<'_>,
) -> Result<Located, EngineError> {
    let e = cursor(plan, scenario, expected, recorded == Role::Expected)?;
    let a = cursor(plan, scenario, actual, recorded == Role::Actual)?;
    scan(plan, Strategy::StreamCompare, e, a)
}

/// Starts a side live or, if `record`, records it as [`run`] does.
fn cursor<'a>(
    plan: &Plan,
    scenario: &Scenario,
    mut side: Side<'a>,
    record: bool,
) -> Result<Cursor<'a>, EngineError> {
    if !record {
        start(plan, scenario, &mut side)?;
        return Ok(Cursor::Live(side.backend));
    }
    let Recording { streams, failure } = run(plan, scenario, side);
    Ok(Cursor::Replay {
        streams,
        next: 0,
        failure,
    })
}

/// Records both sides whole and compares the recordings.
pub fn recorded(
    plan: &Plan,
    scenario: &Scenario,
    expected: Side<'_>,
    actual: Side<'_>,
) -> Result<Located, EngineError> {
    let e = run(plan, scenario, expected);
    let a = run(plan, scenario, actual);
    let verdict = compare_recordings(plan, &e, &a)?;
    let window = diverged_at(&verdict).map(|d| {
        let w = u64::from(plan.profile.report_window);
        let first = d.saturating_sub(w);
        let mut failures = Vec::new();
        let mut keep = |role: Role, r: &Recording| {
            let primary = r.streams.first().map_or(&[][..], Vec::as_slice);
            let end = (d + w + 1).min(primary.len() as u64);
            if let Some(f) = &r.failure
                && (primary.len() as u64) < d + w + 1
            {
                failures.push((role, f.clone()));
            }
            primary[first as usize..end as usize].to_vec()
        };
        Window {
            first,
            expected: keep(Role::Expected, &e),
            actual: keep(Role::Actual, &a),
            failures,
        }
    });
    Ok(Located {
        strategy: Strategy::Recorded,
        verdict,
        window,
    })
}

fn start(plan: &Plan, scenario: &Scenario, side: &mut Side<'_>) -> Result<(), EngineError> {
    side.backend
        .start(scenario, &side.input, &side.config, &plan.profile)
}

fn diverged_at(verdict: &Verdict) -> Option<u64> {
    match verdict {
        Verdict::Compared(Comparison {
            primary: StreamResult::Diverged { ordinal, .. },
            ..
        }) => Some(*ordinal),
        _ => None,
    }
}

/// A side being scanned: live, or replaying its recording, which ends in its failure if
/// it failed.
enum Cursor<'a> {
    Live(&'a mut dyn Backend),
    Replay {
        streams: Recorded,
        next: usize,
        failure: Option<EngineError>,
    },
}

impl Cursor<'_> {
    fn next(&mut self, primary: &str) -> Result<Option<Observation>, EngineError> {
        match self {
            Cursor::Live(b) => b.next(primary),
            Cursor::Replay {
                streams,
                next,
                failure,
            } => {
                let o = streams[0].get(*next).cloned();
                *next += usize::from(o.is_some());
                match (o, failure) {
                    (None, Some(f)) => Err(f.clone()),
                    (o, _) => Ok(o),
                }
            }
        }
    }

    /// The observations of auxiliary stream `i`, `name`: everything emitted so far, or,
    /// with `whole`, everything up to the stop rule.
    fn auxiliary(
        &mut self,
        i: usize,
        name: &str,
        whole: bool,
    ) -> Result<Vec<Observation>, EngineError> {
        match self {
            Cursor::Live(b) if whole => {
                let mut out = Vec::new();
                while let Some(o) = b.next(name)? {
                    out.push(o);
                }
                Ok(out)
            }
            Cursor::Live(b) => b.emitted(name),
            Cursor::Replay { streams, .. } => Ok(std::mem::take(&mut streams[i])),
        }
    }
}

/// The lockstep scan of two started sides (§11.3).
fn scan(
    plan: &Plan,
    strategy: Strategy,
    mut e: Cursor<'_>,
    mut a: Cursor<'_>,
) -> Result<Located, EngineError> {
    let primary = plan.profile.primary.stream.as_str();
    let mut seen = [Vec::new(), Vec::new()];
    let located = |verdict: Verdict, window: Option<Window>| Located {
        strategy,
        verdict,
        window,
    };
    let (eo, ao) = loop {
        let eo = match e.next(primary) {
            Ok(o) => o,
            Err(f) => return failed(plan, seen, Role::Expected, f).map(|v| located(v, None)),
        };
        let ao = match a.next(primary) {
            Ok(o) => o,
            Err(f) => return failed(plan, seen, Role::Actual, f).map(|v| located(v, None)),
        };
        match (eo, ao) {
            (None, None) => break (None, None),
            (Some(eo), Some(ao)) if eo == ao => {
                seen[0].push(eo);
                seen[1].push(ao);
            }
            other => break other,
        }
    };
    let streams = plan.profile.streams().count();
    let whole = eo.is_none() && ao.is_none();
    let d = seen[0].len() as u64;
    let ended = [eo.is_none(), ao.is_none()];
    seen[0].extend(eo);
    seen[1].extend(ao);
    let mut recorded = [vec![seen[0].clone()], vec![seen[1].clone()]];
    for (i, s) in plan.profile.auxiliary.iter().enumerate() {
        recorded[0].push(e.auxiliary(i + 1, &s.stream, whole)?);
        recorded[1].push(a.auxiliary(i + 1, &s.stream, whole)?);
    }
    debug_assert!(recorded.iter().all(|r| r.len() == streams));
    if whole {
        let verdict = compare::compare(plan, &recorded[0], &recorded[1])?;
        return Ok(located(verdict, None));
    }
    let verdict = compare_cut(plan, &recorded[0], &recorded[1])?;
    if diverged_at(&verdict).is_none() {
        return Ok(located(verdict, None));
    }
    // Collect the report window after the divergence, never past a side's end or failure.
    let w = u64::from(plan.profile.report_window);
    let first = d.saturating_sub(w) as usize;
    let mut failures = Vec::new();
    let mut keep = |c: &mut Cursor<'_>, role: Role, seen: &[Observation], ended: bool| {
        let mut kept = seen[first..].to_vec();
        for _ in 0..if ended { 0 } else { w } {
            match c.next(primary) {
                Ok(Some(o)) => kept.push(o),
                Ok(None) => break,
                Err(f) => {
                    failures.push((role, f));
                    break;
                }
            }
        }
        kept
    };
    let window = Window {
        first: first as u64,
        expected: keep(&mut e, Role::Expected, &seen[0], ended[0]),
        actual: keep(&mut a, Role::Actual, &seen[1], ended[1]),
        failures,
    };
    Ok(located(verdict, Some(window)))
}

/// The result of a scan that `role` ended with `failure`: the primary observations both
/// sides took so far are compared as recordings (§20).
fn failed(
    plan: &Plan,
    seen: [Vec<Observation>; 2],
    role: Role,
    failure: EngineError,
) -> Result<Verdict, EngineError> {
    let streams = plan.profile.streams().count();
    let [e, a] = seen.map(|primary| {
        let mut streams = vec![Vec::new(); streams];
        streams[0] = primary;
        Recording {
            streams,
            failure: None,
        }
    });
    let (mut e, mut a) = (e, a);
    match role {
        Role::Expected => e.failure = Some(failure),
        Role::Actual => a.failure = Some(failure),
    }
    compare_recordings(plan, &e, &a)
}

/// Why a side's index cannot be compared from an ordinal on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The observation is incomparable under the plan.
    Incomparable(Incomparable),
    /// The observation breaks the projection's contract with the profile.
    Broken(EngineError),
}

/// One side's primary stream recorded for checkpoint search (§12.3): its prefix digests
/// at every boundary and its checkpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// The side's `ExecutionId`.
    pub execution: [u8; 32],
    /// `P(0)` to `P(n)`, for the `n` primary observations recorded.
    pub prefixes: Vec<[u8; 32]>,
    /// Why the stream ended after `n` observations: `None` at the stop rule.
    pub failure: Option<EngineError>,
    /// The first observation the plan refuses, with its ordinal.
    pub refused: Option<(u64, Refusal)>,
    /// The side's checkpoints.
    pub checkpoints: Vec<Checkpoint>,
}

impl Index {
    /// The number of primary observations recorded.
    pub fn len(&self) -> u64 {
        self.prefixes.len() as u64 - 1
    }

    /// Whether no primary observation was recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Runs one side as `role` to the end of its primary stream, recording the prefix digest
/// at every boundary and, when the side can snapshot, a checkpoint after every `every`
/// boundaries (none when `every` is 0). A checkpoint the side cannot take is left out:
/// density never changes the result (§12.3). Requires [`Capability::StepBoundary`].
pub fn record_index(
    plan: &Plan,
    scenario: &Scenario,
    mut side: Side<'_>,
    role: Role,
    every: u64,
) -> Result<Index, EngineError> {
    let execution = execution_id(scenario, &side.input, &side.backend.describe());
    let snapshots = every > 0 && side.backend.capabilities().contains(&Capability::Snapshot);
    start(plan, scenario, &mut side)?;
    let spec = &plan.profile.primary;
    let mut index = Index {
        execution,
        prefixes: vec![[0; 32]],
        failure: None,
        refused: None,
        checkpoints: Vec::new(),
    };
    let mut prefix = PrefixDigest::new();
    loop {
        let o = match side.backend.next(&spec.stream) {
            Ok(Some(o)) => o,
            Ok(None) => break,
            Err(f) => {
                index.failure = Some(f);
                break;
            }
        };
        let ordinal = prefix.count();
        if index.refused.is_none() {
            index.refused = match compare::check(spec, ordinal, role, &o) {
                Ok(Ok(())) => None,
                Ok(Err(why)) => Some((ordinal, Refusal::Incomparable(why))),
                Err(broken) => Some((ordinal, Refusal::Broken(broken))),
            };
        }
        prefix.absorb(&o);
        index.prefixes.push(prefix.digest());
        if snapshots
            && prefix.count().is_multiple_of(every)
            && let Ok(c) = side.backend.checkpoint()
        {
            index.checkpoints.push(c);
        }
    }
    Ok(index)
}

/// Locates the first divergence from both sides' indexes (§12.3): a binary search for the
/// last ordinal whose prefix digests agree, then each side restored from its own latest
/// checkpoint before the report window and stepped through it, confirming that the
/// replay reproduces its index and that the observations differ there. An agreement is
/// confirmed by a lockstep scan from the initial states, which also compares the
/// auxiliary streams. Requires [`Capability::StepBoundary`] and [`Capability::Restore`]
/// on both sides.
pub fn search(
    plan: &Plan,
    scenario: &Scenario,
    expected: (Side<'_>, &Index),
    actual: (Side<'_>, &Index),
) -> Result<Located, EngineError> {
    let indexes = [expected.1, actual.1];
    let id = |side: &Side<'_>| execution_id(scenario, &side.input, &side.backend.describe());
    for (id, index) in [(id(&expected.0), expected.1), (id(&actual.0), actual.1)] {
        if index.execution != id {
            return Err(EngineError::Internal(
                "the index belongs to another execution".to_owned(),
            ));
        }
        if index.prefixes.first() != Some(&[0; 32]) {
            return Err(EngineError::Internal(
                "an index starts at P(0) = 0".to_owned(),
            ));
        }
    }
    // A failed side's stream is cut where it failed, as for recordings (§20).
    let cut = indexes
        .iter()
        .filter(|i| i.failure.is_some())
        .map(|i| i.len())
        .min();
    let n = indexes.map(|i| i.len().min(cut.unwrap_or(u64::MAX)));
    let common = n[0].min(n[1]);
    let good = last_agreeing(&expected.1.prefixes, &actual.1.prefixes, common);
    let d = if good < common {
        Some(good)
    } else {
        (n[0] != n[1]).then_some(common)
    };
    // The comparator checks every observation up to the divergence before it diffs. The
    // sides agree before the divergence, so a refusal there is both sides' first, and the
    // expected side's is the one a scan meets first.
    let checked = d.map_or(common, |d| d + 1);
    let refused = indexes
        .iter()
        .zip(n)
        .find_map(|(index, n)| index.refused.as_ref().filter(|(j, _)| *j < checked.min(n)));
    let located = |verdict: Verdict, window: Option<Window>| Located {
        strategy: Strategy::Search,
        verdict,
        window,
    };
    match refused {
        Some((_, Refusal::Broken(e))) => return Err(e.clone()),
        Some((_, Refusal::Incomparable(why))) => {
            return Ok(located(Verdict::Incomparable(why.clone()), None));
        }
        None => {}
    }
    let Some(d) = d else {
        // The streams agree as far as both were recorded.
        let failed = indexes
            .iter()
            .filter(|i| i.failure.is_some())
            .min_by_key(|i| i.len());
        if let Some(i) = failed {
            return Err(i.failure.clone().expect("a failed side"));
        }
        let mut l = lockstep(plan, scenario, expected.0, actual.0)?;
        l.strategy = Strategy::Search;
        return Ok(l);
    };
    let w = u64::from(plan.profile.report_window);
    let first = d.saturating_sub(w);
    let mut failures = Vec::new();
    let e = replay(
        plan,
        scenario,
        expected,
        Role::Expected,
        d,
        first,
        &mut failures,
    )?;
    let a = replay(
        plan,
        scenario,
        actual,
        Role::Actual,
        d,
        first,
        &mut failures,
    )?;
    let at = |kept: &[Observation]| kept.get((d - first) as usize).cloned();
    let divergence = match (at(&e), at(&a)) {
        (Some(eo), Some(ao)) => compare::diff(&eo, &ao).ok_or_else(|| {
            EngineError::BoundaryDesync(format!(
                "the indexes diverge at {d}, and the replayed observations agree"
            ))
        })?,
        (Some(eo), None) => Divergence::Ended {
            ended: Role::Actual,
            kind: eo.kind,
        },
        (None, Some(ao)) => Divergence::Ended {
            ended: Role::Expected,
            kind: ao.kind,
        },
        (None, None) => {
            return Err(EngineError::BoundaryDesync(format!(
                "both replays ended before {d}"
            )));
        }
    };
    let verdict = Verdict::Compared(Comparison {
        primary: StreamResult::Diverged {
            stream: plan.profile.primary.stream.clone(),
            ordinal: d,
            divergence,
        },
        auxiliary: Vec::new(),
        not_compared: plan.not_compared.clone(),
        failures: Vec::new(),
    });
    let window = Window {
        first,
        expected: e,
        actual: a,
        failures,
    };
    Ok(located(verdict, Some(window)))
}

/// Steps both sides from a common origin up to the first primary divergence (§16.2): a
/// side with a checkpoint is restored at it, and a side without one is started and
/// stepped to the boundaries the checkpoint covers, which must reproduce its prefix
/// digest. Without checkpoints the origin is the initial state. The divergence and its
/// report window are those [`search`] reports, so the origin must cover at most the
/// divergence minus the report window. An agreement is confirmed by [`lockstep`] from
/// the initial states, and from a checkpoint it is an error. Requires
/// [`Capability::StepBoundary`] on both sides and [`Capability::Restore`] on a side with
/// a checkpoint.
pub fn resume<'a>(
    plan: &Plan,
    scenario: &Scenario,
    expected: (Side<'a>, Option<&Checkpoint>),
    actual: (Side<'a>, Option<&Checkpoint>),
) -> Result<Located, EngineError> {
    const ROLES: [Role; 2] = [Role::Expected, Role::Actual];
    let spec = &plan.profile.primary;
    let primary = spec.stream.as_str();
    let checkpoints = [expected.1, actual.1];
    let mut marks = [None, None];
    for (i, c) in checkpoints.iter().enumerate() {
        if let Some(c) = c {
            let m = c.mark(primary).ok_or_else(|| {
                EngineError::BoundaryDesync(format!(
                    "{:?}: the checkpoint marks no {primary}",
                    ROLES[i]
                ))
            })?;
            marks[i] = Some((m.covered, m.prefix));
        }
    }
    let origin = match marks {
        [Some(e), Some(a)] if e != a => {
            return Err(EngineError::BoundaryDesync(format!(
                "the checkpoints cover {} and {} boundaries, or different prefixes",
                e.0, a.0
            )));
        }
        [Some(m), _] | [None, Some(m)] => Some(m),
        [None, None] => None,
    };
    let mut sides = [expected.0, actual.0];
    for (i, side) in sides.iter_mut().enumerate() {
        start(plan, scenario, side)?;
        match (checkpoints[i], origin) {
            (Some(c), _) => side.backend.restore(c)?,
            (None, Some((covered, prefix))) => {
                let desync =
                    |why: String| EngineError::BoundaryDesync(format!("{:?}: {why}", ROLES[i]));
                let mut p = PrefixDigest::new();
                while p.count() < covered {
                    match side.backend.next(primary) {
                        Ok(Some(o)) => p.absorb(&o),
                        Ok(None) => return Err(desync(format!("ended at {}", p.count()))),
                        Err(f) => return Err(desync(format!("failed at {}: {f}", p.count()))),
                    }
                }
                if p.digest() != prefix {
                    return Err(desync(format!("P({covered}) is not the checkpoint's")));
                }
            }
            (None, None) => {}
        }
    }
    let c = origin.map_or(0, |(covered, _)| covered);
    let w = u64::from(plan.profile.report_window);
    let mut recent = [VecDeque::new(), VecDeque::new()];
    let mut refused = [None, None];
    let mut at = c;
    let diverged = loop {
        let mut o = [None, None];
        for i in 0..2 {
            o[i] = sides[i].backend.next(primary)?;
            if let Some(o) = &o[i]
                && refused[i].is_none()
            {
                refused[i] = match compare::check(spec, at, ROLES[i], o) {
                    Ok(Ok(())) => None,
                    Ok(Err(why)) => Some(Refusal::Incomparable(why)),
                    Err(broken) => Some(Refusal::Broken(broken)),
                };
            }
        }
        match o {
            [None, None] => break None,
            [Some(eo), Some(ao)] if eo == ao => {
                for (r, o) in recent.iter_mut().zip([eo, ao]) {
                    r.push_back(o);
                    if r.len() as u64 > w {
                        r.pop_front();
                    }
                }
                at += 1;
            }
            [eo, ao] => break Some((eo, ao)),
        }
    };
    let located = |verdict: Verdict, window: Option<Window>| Located {
        strategy: Strategy::Lockstep,
        verdict,
        window,
    };
    // The expected side's refusal is the one a scan from the initial states meets first.
    match refused.into_iter().flatten().next() {
        Some(Refusal::Broken(e)) => return Err(e),
        Some(Refusal::Incomparable(why)) => {
            return Ok(located(Verdict::Incomparable(why), None));
        }
        None => {}
    }
    let Some((eo, ao)) = diverged else {
        if origin.is_some() {
            return Err(EngineError::Internal(format!(
                "the sides agree from the checkpoints at {c}"
            )));
        }
        let [e, a] = sides;
        return lockstep(plan, scenario, e, a);
    };
    let d = at;
    let first = d.saturating_sub(w);
    if first < c {
        return Err(EngineError::Internal(format!(
            "the checkpoints cover {c} boundaries, past the report window before {d}"
        )));
    }
    let divergence = match (&eo, &ao) {
        (Some(eo), Some(ao)) => compare::diff(eo, ao).ok_or_else(|| {
            EngineError::Internal(format!("the observations at {d} differ without a diff"))
        })?,
        (Some(eo), None) => Divergence::Ended {
            ended: Role::Actual,
            kind: eo.kind.clone(),
        },
        (None, Some(ao)) => Divergence::Ended {
            ended: Role::Expected,
            kind: ao.kind.clone(),
        },
        (None, None) => unreachable!("both ended is an agreement"),
    };
    let mut failures = Vec::new();
    let mut kept = [Vec::new(), Vec::new()];
    for (i, o) in [eo, ao].into_iter().enumerate() {
        kept[i].extend(recent[i].drain(..));
        let Some(o) = o else {
            continue;
        };
        kept[i].push(o);
        for _ in 0..w {
            match sides[i].backend.next(primary) {
                Ok(Some(o)) => kept[i].push(o),
                Ok(None) => break,
                Err(f) => {
                    failures.push((ROLES[i], f));
                    break;
                }
            }
        }
    }
    let [e, a] = kept;
    let verdict = Verdict::Compared(Comparison {
        primary: StreamResult::Diverged {
            stream: spec.stream.clone(),
            ordinal: d,
            divergence,
        },
        auxiliary: Vec::new(),
        not_compared: plan.not_compared.clone(),
        failures: Vec::new(),
    });
    let window = Window {
        first,
        expected: e,
        actual: a,
        failures,
    };
    Ok(located(verdict, Some(window)))
}

/// The last ordinal in `0..=n` at which the prefix digests agree; `P(0)` always does.
/// Prefix agreement is monotone, so bisection finds it (§11.2).
fn last_agreeing(e: &[[u8; 32]], a: &[[u8; 32]], n: u64) -> u64 {
    let (mut good, mut bad) = (0, n + 1);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if e[mid as usize] == a[mid as usize] {
            good = mid;
        } else {
            bad = mid;
        }
    }
    good
}

/// Restores one side at its latest checkpoint covering at most `first` primary boundaries,
/// or starts it, and steps it to `d` plus the report window, checking every prefix digest
/// up to `d` against its index. Returns the observations from `first`.
fn replay(
    plan: &Plan,
    scenario: &Scenario,
    (mut side, index): (Side<'_>, &Index),
    role: Role,
    d: u64,
    first: u64,
    failures: &mut Vec<(Role, EngineError)>,
) -> Result<Vec<Observation>, EngineError> {
    let primary = plan.profile.primary.stream.as_str();
    let desync = |why: String| EngineError::BoundaryDesync(format!("{role:?}: {why}"));
    start(plan, scenario, &mut side)?;
    let covered = |c: &Checkpoint| c.mark(primary).map(|m| m.covered);
    let latest = index
        .checkpoints
        .iter()
        .filter(|c| covered(c).is_some_and(|n| n <= first))
        .max_by_key(|c| covered(c));
    let mut prefix = PrefixDigest::new();
    if let Some(c) = latest {
        let m = c.mark(primary).expect("filtered on its mark");
        if index.prefixes.get(m.covered as usize) != Some(&m.prefix) {
            return Err(desync(format!(
                "the checkpoint's P({}) is not its index's",
                m.covered
            )));
        }
        side.backend.restore(c)?;
        prefix = PrefixDigest::resume(m.prefix, m.covered);
    }
    let w = u64::from(plan.profile.report_window);
    let mut kept = Vec::new();
    while prefix.count() <= d + w {
        let at = prefix.count();
        let o = match side.backend.next(primary) {
            Ok(Some(o)) => o,
            Ok(None) if at < d || (at == d && index.len() != d) => {
                return Err(desync(format!("the replay ended at {at}")));
            }
            Ok(None) => break,
            Err(f) if at <= d => {
                return Err(desync(format!("the replay failed at {at}: {f}")));
            }
            Err(f) => {
                failures.push((role, f));
                break;
            }
        };
        prefix.absorb(&o);
        if at <= d && index.prefixes.get(at as usize + 1) != Some(&prefix.digest()) {
            return Err(desync(format!("P({}) is not its index's", at + 1)));
        }
        if at >= first {
            kept.push(o);
        }
    }
    Ok(kept)
}
