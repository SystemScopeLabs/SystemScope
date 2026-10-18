//! The locator on the counter: lockstep, stream compare, checkpoint search, and whole
//! recordings agree with the naive comparison, use only declared capabilities, and refuse
//! indexes a replay contradicts (`docs/m4-design.md` §11, §12.3, §20).

mod common;

use common::*;
use systemscope_verify::backend::{Backend, Capabilities, Capability, Side, verify};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario};
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::compare::{Comparison, Divergence, StreamResult, Verdict};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{
    Index, Located, Strategy, Window, locate, lockstep, record_index, recorded, search,
    stream_compare,
};
use systemscope_verify::observation::Observation;
use systemscope_verify::profile::{ComparisonProfile, Plan, Role, Support, plan};
use systemscope_verify::runtime::RuntimeBackend;

/// A counter backend that declares only `caps` and panics on any call they do not allow.
struct Limited {
    inner: RuntimeBackend,
    caps: Capabilities,
}

impl Limited {
    fn new(caps: &[Capability]) -> Limited {
        Limited {
            inner: backend(),
            caps: caps.iter().copied().collect(),
        }
    }

    fn need(&self, c: Capability, call: &str) {
        assert!(self.caps.contains(&c), "{call} without {c:?}");
    }
}

impl Backend for Limited {
    fn describe(&self) -> BackendId {
        self.inner.describe()
    }
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }
    fn support(&self) -> Support {
        self.inner.support()
    }
    fn start(
        &mut self,
        scenario: &Scenario,
        input: &InputVariant,
        config: &BackendRunConfig,
        profile: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        self.inner.start(scenario, input, config, profile)
    }
    fn next(&mut self, stream: &str) -> Result<Option<Observation>, EngineError> {
        self.need(Capability::StepBoundary, "next");
        self.inner.next(stream)
    }
    fn emitted(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        self.need(Capability::StepBoundary, "emitted");
        self.inner.emitted(stream)
    }
    fn checkpoint(&self) -> Result<Checkpoint, EngineError> {
        self.need(Capability::Snapshot, "checkpoint");
        self.inner.checkpoint()
    }
    fn restore(&mut self, c: &Checkpoint) -> Result<(), EngineError> {
        self.need(Capability::Restore, "restore");
        self.inner.restore(c)
    }
    fn record(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        self.inner.record(stream)
    }
}

const ALL: [Capability; 4] = [
    Capability::ArchStream,
    Capability::StepBoundary,
    Capability::Snapshot,
    Capability::Restore,
];
const POST_RUN: [Capability; 1] = [Capability::ArchStream];

fn the_plan(profile: &ComparisonProfile) -> Plan {
    let b = backend();
    plan(profile, &b.support(), &b.support()).unwrap()
}

fn with(b: &mut dyn Backend, input: InputVariant, budget: u64) -> Side<'_> {
    Side {
        backend: b,
        input,
        config: BackendRunConfig { budget },
    }
}

/// One side of a case: its input and budget.
#[derive(Clone)]
struct Case {
    input: InputVariant,
    budget: u64,
}

fn case(input: InputVariant) -> Case {
    Case {
        input,
        budget: config().budget,
    }
}

fn index(plan: &Plan, c: &Case, role: Role, every: u64) -> Index {
    let mut b = backend();
    record_index(
        plan,
        &scenario(),
        with(&mut b, c.input.clone(), c.budget),
        role,
        every,
    )
    .unwrap()
}

fn side<'a>(b: &'a mut RuntimeBackend, c: &Case) -> Side<'a> {
    Side {
        backend: b,
        input: c.input.clone(),
        config: BackendRunConfig { budget: c.budget },
    }
}

/// Every strategy on `(e, a)`, with checkpoint search at several densities.
fn every_strategy(plan: &Plan, e: &Case, a: &Case) -> Vec<Result<Located, EngineError>> {
    let s = scenario();
    let mut out = Vec::new();
    let (mut x, mut y) = (backend(), backend());
    out.push(lockstep(plan, &s, side(&mut x, e), side(&mut y, a)));
    for recorded_side in [Role::Expected, Role::Actual] {
        let (mut x, mut y) = (backend(), backend());
        out.push(stream_compare(
            plan,
            &s,
            recorded_side,
            side(&mut x, e),
            side(&mut y, a),
        ));
    }
    for every in [0, 1, 3, 5] {
        let (ie, ia) = (
            index(plan, e, Role::Expected, every),
            index(plan, a, Role::Actual, every),
        );
        let (mut x, mut y) = (backend(), backend());
        out.push(search(
            plan,
            &s,
            (side(&mut x, e), &ie),
            (side(&mut y, a), &ia),
        ));
    }
    let (mut x, mut y) = (backend(), backend());
    out.push(recorded(plan, &s, side(&mut x, e), side(&mut y, a)));
    out
}

fn naive(profile: &ComparisonProfile, e: &Case, a: &Case) -> Result<Verdict, EngineError> {
    let (mut x, mut y) = (backend(), backend());
    verify(
        profile,
        &scenario(),
        with(&mut x, e.input.clone(), e.budget),
        with(&mut y, a.input.clone(), a.budget),
    )
}

fn primary(v: &Verdict) -> Option<&StreamResult> {
    match v {
        Verdict::Compared(c) => Some(&c.primary),
        Verdict::Incomparable(_) => None,
    }
}

/// Asserts every strategy gives the naive primary result and one report window.
fn agree_with_naive(profile: &ComparisonProfile, e: &Case, a: &Case) -> Option<Window> {
    let plan = the_plan(profile);
    let want = naive(profile, e, a);
    let results = every_strategy(&plan, e, a);
    let window = results[0].as_ref().ok().and_then(|l| l.window.clone());
    for (i, got) in results.iter().enumerate() {
        match (&want, got) {
            (Ok(Verdict::Incomparable(w)), Ok(l)) => {
                assert_eq!(l.verdict, Verdict::Incomparable(w.clone()), "strategy {i}")
            }
            (Ok(w), Ok(l)) => {
                assert_eq!(primary(&l.verdict), primary(w), "strategy {i}");
                assert_eq!(l.window, window, "strategy {i}");
            }
            (Err(w), Err(g)) => assert_eq!(g, w, "strategy {i}"),
            (w, g) => panic!("strategy {i}: {g:?}, not {w:?}"),
        }
    }
    window
}

/// The whole primary stream of one side.
fn stream(c: &Case) -> Vec<Observation> {
    let mut b = backend();
    b.start(
        &scenario(),
        &c.input,
        &BackendRunConfig { budget: c.budget },
        &profile(),
    )
    .unwrap();
    let mut out = Vec::new();
    while let Ok(Some(o)) = b.next("arch") {
        out.push(o);
    }
    out
}

#[test]
fn every_strategy_agrees_on_identical_sides() {
    let plain = case(InputVariant::None);
    assert_eq!(agree_with_naive(&profile(), &plain, &plain), None);
    // Lockstep and stream compare also compare the auxiliary streams to the stop rule.
    let plan = the_plan(&profile());
    let want = naive(&profile(), &plain, &plain).unwrap();
    for l in &every_strategy(&plan, &plain, &plain)[..3] {
        assert_eq!(l.as_ref().unwrap().verdict, want);
    }
}

#[test]
fn every_strategy_finds_the_first_divergence_and_its_window() {
    for boundary in [0u64, 4, 9] {
        let n = boundary + 1;
        let e = case(InputVariant::None);
        let a = case(patch_n(n, n, n + 1));
        let window = agree_with_naive(&profile(), &e, &a).expect("a window");
        let d = boundary + 1;
        let first = d.saturating_sub(3);
        let (es, as_) = (stream(&e), stream(&a));
        let end = |s: &[Observation]| (d as usize + 4).min(s.len());
        assert_eq!(
            window,
            Window {
                first,
                expected: es[first as usize..end(&es)].to_vec(),
                actual: as_[first as usize..end(&as_)].to_vec(),
                failures: Vec::new(),
            },
            "{boundary}"
        );
    }
}

#[test]
fn lockstep_stops_at_the_divergence_before_a_later_failure() {
    // After boundary 10 the patched counter passes its limit and its queue drains: lockstep
    // and search report the divergence at 11 with no failure, and the window notes the
    // failure that cuts the actual side short.
    let e = case(InputVariant::None);
    let a = case(patch_n(11, 11, 12));
    let window = agree_with_naive(&profile(), &e, &a).unwrap();
    assert_eq!(window.first, 8);
    assert_eq!(window.expected.len(), 4);
    assert_eq!(
        window.failures,
        [(
            Role::Actual,
            EngineError::BackendFailed(
                "the event queue drained after 12 events, before the stop rule".to_owned()
            )
        )]
    );
    let mut x = backend();
    let mut y = backend();
    let plan = the_plan(&profile());
    let l = lockstep(
        &plan,
        &scenario(),
        with(&mut x, e.input.clone(), e.budget),
        with(&mut y, a.input.clone(), a.budget),
    )
    .unwrap();
    let Verdict::Compared(c) = &l.verdict else {
        panic!()
    };
    assert!(c.failures.is_empty());
    assert_eq!(l.divergence().map(|d| d.0), Some(11));
}

#[test]
fn every_strategy_reports_a_failure_with_no_divergence_before_it() {
    let e = case(InputVariant::None);
    let short = Case {
        input: InputVariant::None,
        budget: 5,
    };
    let budget = EngineError::BackendFailed(
        "the safety budget of 5 events ran out before the stop rule".to_owned(),
    );
    assert_eq!(naive(&profile(), &e, &short), Err(budget.clone()));
    agree_with_naive(&profile(), &e, &short);
    agree_with_naive(&profile(), &short, &e);
}

#[test]
fn an_incomparable_observation_before_the_divergence_decides() {
    // `rd_value` is absent where 3 divides `n`: ordinal 2 first.
    let mut strict = profile();
    strict.primary.kinds[1].fields[1].may_be_absent = false;
    let plain = case(InputVariant::None);
    assert!(matches!(
        naive(&strict, &plain, &plain),
        Ok(Verdict::Incomparable(_))
    ));
    agree_with_naive(&strict, &plain, &plain);
    // A divergence at 4 is after it; a divergence at 0 is before it.
    agree_with_naive(&strict, &plain, &case(patch_n(4, 4, 5)));
    let before = agree_with_naive(&strict, &plain, &case(patch_n(0, 0, 1)));
    assert_eq!(before.map(|w| w.first), Some(0));
}

#[test]
fn locate_chooses_by_capability_and_uses_no_other() {
    let plan = the_plan(&profile());
    let s = scenario();
    let e = case(InputVariant::None);
    let a = case(patch_n(5, 5, 6));
    let (ie, ia) = (
        index(&plan, &e, Role::Expected, 2),
        index(&plan, &a, Role::Actual, 2),
    );
    let no_restore = [Capability::ArchStream, Capability::StepBoundary];
    for (ec, ac, indexes, strategy) in [
        (&ALL[..], &ALL[..], true, Strategy::Search),
        (&ALL[..], &ALL[..], false, Strategy::Lockstep),
        (&ALL[..], &no_restore[..], true, Strategy::Lockstep),
        (&POST_RUN[..], &ALL[..], true, Strategy::StreamCompare),
        (&ALL[..], &POST_RUN[..], false, Strategy::StreamCompare),
        (&POST_RUN[..], &POST_RUN[..], true, Strategy::Recorded),
    ] {
        let (mut x, mut y) = (Limited::new(ec), Limited::new(ac));
        let l = locate(
            &plan,
            &s,
            with(&mut x, e.input.clone(), e.budget),
            with(&mut y, a.input.clone(), a.budget),
            indexes.then_some((&ie, &ia)),
        )
        .unwrap();
        assert_eq!(l.strategy, strategy);
        assert_eq!(l.divergence().map(|d| d.0), Some(5), "{strategy:?}");
    }
}

#[test]
fn search_refuses_an_index_a_replay_contradicts() {
    let plan = the_plan(&profile());
    let s = scenario();
    let e = case(InputVariant::None);
    let a = case(patch_n(8, 8, 9));
    let (ie, ia) = (
        index(&plan, &e, Role::Expected, 2),
        index(&plan, &a, Role::Actual, 2),
    );
    let run = |ie: &Index, ia: &Index| {
        let (mut x, mut y) = (backend(), backend());
        search(
            &plan,
            &s,
            (with(&mut x, e.input.clone(), e.budget), ie),
            (with(&mut y, a.input.clone(), a.budget), ia),
        )
    };
    assert_eq!(run(&ie, &ia).unwrap().divergence().map(|d| d.0), Some(8));
    let desync = |r: Result<Located, EngineError>| match r {
        Err(EngineError::BoundaryDesync(why)) => why,
        other => panic!("{other:?}"),
    };
    // A prefix digest the replay does not reproduce, inside the scanned window.
    let mut forged = ia.clone();
    forged.prefixes[7] = [9; 32];
    assert_eq!(desync(run(&ie, &forged)), "Actual: P(7) is not its index's");
    // A checkpoint whose digest is not its index's.
    let mut forged = ie.clone();
    for c in &mut forged.checkpoints {
        c.streams[0].prefix = [9; 32];
    }
    assert_eq!(
        desync(run(&forged, &ia)),
        "Expected: the checkpoint's P(4) is not its index's"
    );
    // Digests that diverge where the observations agree.
    let mut forged = ia.clone();
    for p in &mut forged.prefixes[4..] {
        *p = [9; 32];
    }
    assert!(desync(run(&ie, &forged)).starts_with("Actual: P(4) is not"));
    // Another execution's index.
    assert_eq!(
        run(&ia, &ia),
        Err(EngineError::Internal(
            "the index belongs to another execution".to_owned()
        ))
    );
}

#[test]
fn an_index_records_digests_checkpoints_and_refusals() {
    let plan = the_plan(&profile());
    let plain = case(InputVariant::None);
    let i = index(&plan, &plain, Role::Expected, 5);
    assert_eq!(i.len(), LIMIT);
    assert!(!i.is_empty());
    assert_eq!(i.failure, None);
    assert_eq!(i.refused, None);
    // After boundaries 5 and 10; the last, 12, is at the stop rule.
    let covered: Vec<u64> = i
        .checkpoints
        .iter()
        .map(|c| c.mark("arch").unwrap().covered)
        .collect();
    assert_eq!(covered, [5, 10]);
    assert!(
        index(&plan, &plain, Role::Expected, 0)
            .checkpoints
            .is_empty()
    );
    let mut strict = profile();
    strict.primary.kinds[1].fields[1].may_be_absent = false;
    let i = index(&the_plan(&strict), &plain, Role::Actual, 0);
    assert!(matches!(i.refused, Some((2, _))));
    let short = Case {
        input: InputVariant::None,
        budget: 5,
    };
    let i = index(&plan, &short, Role::Actual, 0);
    assert_eq!(i.len(), 5);
    assert!(i.failure.is_some());
}

#[test]
fn a_divergence_reports_its_kind() {
    // Patching n past the limit's halt: the patched side's stream ends first.
    let e = case(InputVariant::None);
    let a = case(patch_n(10, 10, 11));
    agree_with_naive(&profile(), &e, &a);
    let plan = the_plan(&profile());
    let (mut x, mut y) = (backend(), backend());
    let l = lockstep(
        &plan,
        &scenario(),
        with(&mut x, e.input.clone(), e.budget),
        with(&mut y, a.input.clone(), a.budget),
    )
    .unwrap();
    let (d, divergence) = l.divergence().unwrap();
    assert_eq!(d, 10);
    assert!(matches!(divergence, Divergence::Fields { .. }));
    let Verdict::Compared(Comparison { auxiliary, .. }) = &l.verdict else {
        panic!()
    };
    // The auxiliary stream is compared over what both sides emitted, as evidence.
    assert!(auxiliary[0].agrees());
}
