//! M4.1 on `m3-reference` (`docs/m4-design.md` §25.1): the unmodified sides agree, and
//! the state-patch controlled divergences SP-1 to SP-3 diverge exactly where a selector,
//! a pure function of the reference `arch` stream, says they must, which the naive oracle
//! confirms. M4.2 (§25.2): every locator strategy, and checkpoint search at every
//! density, finds the same first divergence and reports the same window. M4.3: each
//! case's same-engine report carries its state, trace, and slice evidence, and the
//! code-byte patches CB-1 and CB-2 diverge as §25.1 derives. M4.4 (§16): every case's
//! reproducer bundle, from the initial states and from checkpoints, replays byte for byte.
//! M4.5 (§17.2): each minimized bundle starts from each side's latest checkpoint covering
//! at most the divergence minus the report window, and replays the same failure.

use systemscope_acceptance::m3::Fixture;
use systemscope_acceptance::m3::golden::Golden;
use systemscope_acceptance::m4::verify::{self, CPU_PATH};
use systemscope_contracts::trace::Value;
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::bundle::{
    Case, MANIFEST, Manifest, RESULT, read_dir, replay, write, write_dir,
};
use systemscope_verify::case::{InputVariant, MediaPatch, StatePatch};
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::compare::{
    Divergence, Outcome, Recorded, Recording, StreamResult, Verdict, compare, compare_recordings,
    naive_first_divergence,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::latent::{SameSide, StateDivergence, first_state_divergence};
use systemscope_verify::locate::{
    Index, Located, Strategy, lockstep, record_index, search, stream_compare,
};
use systemscope_verify::minimize::minimize;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::report::{Builder, same_engine};
use systemscope_verify::result::{ResultOutcome, VerificationResult};
use systemscope_verify::runtime::RuntimeBackend;
use systemscope_verify::slice::Link;
use systemscope_verify::state::{Change, diff_state};
use systemscope_verify::trace::trace_and_slices;

const GOLDEN_JSON: &str = include_str!("../../golden/m3-reference.json");

fn fixture() -> Fixture {
    Fixture::read(&workspace_root(), Scenario::Reference).unwrap_or_else(|e| panic!("{e}"))
}

fn the_plan(b: &RuntimeBackend) -> Plan {
    plan(&verify::profile(), &b.support(), &b.support()).unwrap()
}

/// Records every stream of one side, up to any failure.
fn record(fixture: &Fixture, input: InputVariant) -> (RuntimeBackend, Recording) {
    let mut b = verify::backend(fixture);
    let plan = the_plan(&b);
    let side = Side {
        backend: &mut b,
        input,
        config: verify::config(),
    };
    let recording = run(&plan, &verify::scenario(fixture), side);
    (b, recording)
}

/// Records a side that must reach the stop rule.
fn complete(fixture: &Fixture, input: InputVariant) -> (RuntimeBackend, Recorded) {
    let (b, recording) = record(fixture, input);
    let streams = recording.complete().unwrap_or_else(|e| panic!("{e}"));
    (b, streams)
}

fn u32_field(o: &Observation, name: &str) -> u32 {
    match o.field(name) {
        Some(ObservedValue::Present(Value::U64(v))) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn is(o: &Observation, name: &str, value: Value) -> bool {
    o.field(name) == Some(&ObservedValue::Present(value))
}

/// Patches `field` of the CPU from `old` to `new` after `event_index` events.
fn patch(event_index: u64, field: &str, old: u32, new: u32) -> InputVariant {
    InputVariant::State(StatePatch {
        event_index,
        field: format!("{CPU_PATH}/{field}"),
        old: Value::U64(old.into()),
        new: Value::U64(new.into()),
    })
}

fn side(b: &mut RuntimeBackend, input: InputVariant) -> Side<'_> {
    Side {
        backend: b,
        input,
        config: verify::config(),
    }
}

/// A verdict without the auxiliary streams and failures, which strategies report only as
/// evidence.
fn primary(v: &Verdict) -> Verdict {
    let mut v = v.clone();
    if let Verdict::Compared(c) = &mut v {
        c.auxiliary.clear();
        c.failures.clear();
    }
    v
}

/// Locates the first divergence against the unmodified side `b`, which recorded
/// `expected`, by lockstep, stream compare with either side recorded, and checkpoint
/// search with no checkpoints, about one per 10,000 events, and one per 997 primary
/// boundaries. Checks that each finds the naive primary result and the same window, and
/// returns the lockstep result and both sides' indexes at about one checkpoint per 10,000
/// events.
fn locate_everywhere(
    fixture: &Fixture,
    b: &RuntimeBackend,
    expected: &Recorded,
    input: InputVariant,
) -> (Located, [Index; 2]) {
    let scenario = verify::scenario(fixture);
    let plan = the_plan(b);
    let (_, actual) = record(fixture, input.clone());
    let complete = Recording {
        streams: expected.clone(),
        failure: None,
    };
    let naive = compare_recordings(&plan, &complete, &actual).unwrap();
    let n = expected[0].len() as u64;
    let per_boundary = b.event_index("arch", n - 1).unwrap() / n;
    let pair = || (verify::backend(fixture), verify::backend(fixture));
    let mut results = Vec::new();
    let mut indexes = Vec::new();
    let (mut x, mut y) = pair();
    let (e, a) = (
        side(&mut x, InputVariant::None),
        side(&mut y, input.clone()),
    );
    results.push(lockstep(&plan, &scenario, e, a).unwrap());
    for role in [Role::Expected, Role::Actual] {
        let (mut x, mut y) = pair();
        let (e, a) = (
            side(&mut x, InputVariant::None),
            side(&mut y, input.clone()),
        );
        results.push(stream_compare(&plan, &scenario, role, e, a).unwrap());
    }
    for every in [0, 10_000 / per_boundary, 997] {
        let (mut x, mut y) = pair();
        let e = side(&mut x, InputVariant::None);
        let ie = record_index(&plan, &scenario, e, Role::Expected, every).unwrap();
        let a = side(&mut y, input.clone());
        let ia = record_index(&plan, &scenario, a, Role::Actual, every).unwrap();
        // The unmodified side checkpoints every `every` boundaries. An m3 checkpoint is
        // about 150 KB, so the index at one per 10,000 events stays near 2.4 MB.
        let states: Vec<usize> = ie.checkpoints.iter().map(|c| c.state.len()).collect();
        assert_eq!(states.len() as u64, n.checked_div(every).unwrap_or(0));
        assert!(states.iter().all(|s| *s < 256 << 10), "{states:?}");
        assert!(states.iter().sum::<usize>() < 4 << 20, "{states:?}");
        let (mut x, mut y) = pair();
        let (e, a) = (
            side(&mut x, InputVariant::None),
            side(&mut y, input.clone()),
        );
        results.push(search(&plan, &scenario, (e, &ie), (a, &ia)).unwrap());
        indexes.push([ie, ia]);
    }
    let window = results[0].window.clone();
    for (i, l) in results.iter().enumerate() {
        assert_eq!(primary(&l.verdict), primary(&naive), "strategy {i}");
        assert_eq!(l.window, window, "strategy {i}");
    }
    let strategies: Vec<Strategy> = results.iter().map(|l| l.strategy).collect();
    assert_eq!(
        strategies[..3],
        [
            Strategy::Lockstep,
            Strategy::StreamCompare,
            Strategy::StreamCompare
        ]
    );
    assert!(strategies[3..].iter().all(|s| *s == Strategy::Search));
    // The window is both sides' own observations around the divergence.
    if let Some(w) = &window {
        let (d, _) = results[0].divergence().unwrap();
        let report = u64::from(plan.profile.report_window);
        assert_eq!(w.first, d.saturating_sub(report));
        for (kept, whole) in [(&w.expected, &expected[0]), (&w.actual, &actual.streams[0])] {
            let end = (d + report + 1).min(whole.len() as u64) as usize;
            assert_eq!(kept[..], whole[w.first as usize..end]);
        }
    }
    (results.swap_remove(0), indexes.swap_remove(1))
}

/// Runs the patched side and checks that the comparator, and the naive oracle, put the
/// first divergence at `ordinal`, as every locator strategy does against `b`, and that
/// a state patch's sides first differ in state in the CPU at the patch (§12.4) and in
/// their canonical traces after it (§14.1); a media patch's, in RAM and the trace before
/// the CPU. Each side's predecessor slice (§15) holds only links the case can record,
/// and the same-engine report carries all of it, byte-identical across runs. Returns the
/// divergence, the patched side's failures after it, if any, the lockstep result, and
/// the first state divergence up to the divergent observation.
fn diverge(
    fixture: &Fixture,
    b: &RuntimeBackend,
    expected: &Recorded,
    input: InputVariant,
    ordinal: u64,
) -> (
    Divergence,
    Vec<(Role, EngineError)>,
    Located,
    StateDivergence,
) {
    let (located, indexes) = locate_everywhere(fixture, b, expected, input.clone());
    let (a, actual) = record(fixture, input.clone());
    // The scan ends where either side emits the divergent observation.
    let until = [b, &a]
        .map(|s| s.event_index("arch", ordinal).unwrap())
        .into_iter()
        .max()
        .unwrap();
    let scan = |scope: &[&str]| {
        let (mut x, mut y) = (verify::backend(fixture), verify::backend(fixture));
        first_state_divergence(
            &the_plan(b),
            &verify::scenario(fixture),
            same(&mut x, InputVariant::None, &indexes[0]),
            same(&mut y, input.clone(), &indexes[1]),
            scope,
            until,
        )
        .unwrap()
    };
    let state = scan(&[]);
    let trace = {
        let (mut x, mut y) = (verify::backend(fixture), verify::backend(fixture));
        trace_and_slices(
            &the_plan(b),
            &verify::scenario(fixture),
            same(&mut x, InputVariant::None, &indexes[0]),
            same(&mut y, input.clone(), &indexes[1]),
            3,
            until,
        )
        .unwrap()
    };
    let (trace, slices) = trace;
    let t = trace.event.expect("the traces differ");
    let d = (trace.record - trace.first) as usize;
    assert_ne!(trace.expected[d], trace.actual[d]);
    assert!(t <= until, "{trace:?}");
    // Each side's slice ends at its first differing record, in trace order, within its
    // window.
    assert_eq!(slices.len(), 2);
    for (sl, (role, lines)) in slices.iter().zip([
        (Role::Expected, &trace.expected),
        (Role::Actual, &trace.actual),
    ]) {
        assert_eq!(sl.role, role);
        let last = sl.entries.last().unwrap();
        assert_eq!((last.record, last.link), (trace.record, Link::Divergent));
        assert_eq!(last.line, lines[d]);
        assert!(sl.first <= trace.record);
        assert!(
            sl.entries
                .windows(2)
                .all(|w| sl.first <= w[0].record && w[0].record < w[1].record)
        );
        if let InputVariant::State(_) = &input {
            // A fetch or a retirement: only the CPU's own port traffic precedes it, with
            // each access's bus grant and downstream hop. The page tables and code it
            // read were written before the window.
            let rest = &sl.entries[..sl.entries.len() - 1];
            for e in rest {
                assert!(
                    matches!(e.link, Link::Access | Link::Rename | Link::Request),
                    "{sl:?}"
                );
            }
            for u in &sl.unresolved {
                let load = rest.iter().find(|e| e.record == u.record).unwrap();
                assert_eq!((load.link, u.len), (Link::Access, 4), "{sl:?}");
            }
        }
    }
    if let InputVariant::State(p) = &input {
        assert!(state.origin < p.event_index, "{state:?}");
        // The patch shows in the trace no earlier than the next event.
        assert!(p.event_index < t, "{trace:?}");
        assert_eq!(
            (state.event, &state.differing[..]),
            (Some(p.event_index), &[CPU_PATH.to_owned()][..])
        );
    }
    if let InputVariant::Media(_) = &input {
        // The disk's media identity differs from the start; RAM first differs at the
        // DMA write of the patched bytes, and the CPU only later.
        assert_eq!(
            (state.origin, state.event, &state.differing[..]),
            (0, Some(0), &["soc.disk".to_owned()][..])
        );
        let (ram, cpu) = (scan(&["soc.ram"]), scan(&[CPU_PATH]));
        let (Some(r), Some(c)) = (ram.event, cpu.event) else {
            panic!("{ram:?} {cpu:?}")
        };
        assert!(0 < r && r < c && c <= until, "{ram:?} {cpu:?}");
        assert_eq!(ram.differing, ["soc.ram"]);
        assert_eq!(cpu.differing, [CPU_PATH]);
        // The trace first differs where the controller hands the patched block to the
        // DMA engine, before it reaches RAM.
        let first = &trace.expected[d];
        assert_eq!(
            (first.component.as_str(), first.kind.as_str()),
            ("soc.blk", "runtime.dispatch")
        );
        assert!(
            first
                .fields
                .contains(&("msg".to_owned(), Value::Str("ReadResult".to_owned())))
        );
        assert!(t <= r, "{trace:?}");
        // The patched block's result follows its request, the medium's read, and the
        // transfer's command with the MMIO write that issued it, which the bus renamed
        // from the kernel's own write.
        for sl in &slices {
            let got: Vec<(Link, &str, &str)> = sl
                .entries
                .iter()
                .map(|e| (e.link, e.line.component.as_str(), e.line.kind.as_str()))
                .collect();
            let dispatch = "runtime.dispatch";
            assert_eq!(
                got,
                [
                    (Link::Rename, "soc.bus", dispatch),
                    (Link::Rename, "soc.bus", "platform.bus.grant"),
                    (Link::Dma, "soc.blk", dispatch),
                    (Link::Dma, "soc.blk", "platform.blk.command"),
                    (Link::Request, "soc.disk", dispatch),
                    (Link::Dma, "soc.disk", "platform.disk.read"),
                    (Link::Divergent, "soc.blk", dispatch),
                ]
            );
            let txn = |i: usize| {
                sl.entries[i]
                    .line
                    .fields
                    .iter()
                    .find(|f| f.0 == "txn")
                    .cloned()
            };
            assert_eq!(txn(4), txn(6));
            // The kernel issued the command through the bus.
            assert!(
                sl.entries[0]
                    .line
                    .fields
                    .contains(&("source".to_owned(), Value::U64(7)))
            );
            assert_eq!(sl.unresolved, []);
        }
    }
    // The whole same-engine report at the same density carries exactly this evidence,
    // and gives the same bytes every time (§6.7, §16).
    let n = expected[0].len() as u64;
    let every = 10_000 / (b.event_index("arch", n - 1).unwrap() / n);
    let report = || {
        same_engine(
            &the_plan(b),
            &verify::scenario(fixture),
            &|| verify::backend(fixture),
            [InputVariant::None, input.clone()],
            verify::config(),
            every,
        )
    };
    let r = report();
    let bytes = r.to_bytes();
    assert_eq!(report().to_bytes(), bytes);
    assert_eq!(VerificationResult::from_bytes(&bytes).as_ref(), Ok(&r));
    assert_eq!(r.outcome(), ResultOutcome::Compared(Outcome::Diverged));
    assert_eq!(
        primary(r.verdict.as_ref().unwrap()),
        primary(&located.verdict)
    );
    assert_eq!(r.window, located.window);
    assert_eq!(r.state.as_ref().unwrap().first.as_ref(), Some(&state));
    assert_eq!(
        (r.trace.canonical.as_ref(), &r.slices),
        (Some(&trace), &slices)
    );
    reproduce(fixture, b, input.clone(), &r, Some(ordinal));
    for every in [every, 16] {
        minimized(fixture, b, input.clone(), &r, Some(ordinal), every);
    }
    let text = r.to_string();
    assert!(text.starts_with("outcome: diverged\nprimary: arch: diverged at "));
    assert!(text.contains("\npredecessor slice (actual), window from record "));
    let b = a;
    assert_eq!(
        naive_first_divergence(&expected[0], &actual.streams[0]),
        Some(ordinal)
    );
    let expected = Recording {
        streams: expected.clone(),
        failure: None,
    };
    let verdict = compare_recordings(&the_plan(&b), &expected, &actual).unwrap();
    let Verdict::Compared(c) = verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!(c.outcome(), Outcome::Diverged);
    let StreamResult::Diverged {
        stream,
        ordinal: at,
        divergence,
    } = c.primary
    else {
        unreachable!()
    };
    assert_eq!((stream.as_str(), at), ("arch", ordinal));
    assert_eq!(located.divergence(), Some((ordinal, &divergence)));
    (divergence, c.failures, located, state)
}

/// A side's checkpoint after it emits `boundaries` primary observations.
fn checkpoint_after(fixture: &Fixture, input: &InputVariant, boundaries: u64) -> Checkpoint {
    let mut b = verify::backend(fixture);
    let scenario = verify::scenario(fixture);
    b.start(&scenario, input, &verify::config(), &verify::profile())
        .unwrap();
    for _ in 0..boundaries {
        b.next("arch").unwrap().unwrap();
    }
    b.checkpoint().unwrap()
}

/// Bundles the case of the unmodified side against `input` from the initial states and,
/// for a divergence at `diverged`, from both sides', and the actual side's, checkpoint
/// after the divergence minus the report window: the latest origin §17.2 allows. Each
/// bundle replays byte-identically, finds what the same-engine report `r` finds, and
/// survives a directory.
fn reproduce(
    fixture: &Fixture,
    b: &RuntimeBackend,
    input: InputVariant,
    r: &VerificationResult,
    diverged: Option<u64>,
) {
    let plan = the_plan(b);
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let case = Case {
        plan: &plan,
        scenario: &scenario,
        sides: Builder::same(&make, verify::config()),
    };
    let inputs = [InputVariant::None, input];
    let w = u64::from(plan.profile.report_window);
    let ks = diverged.map(|d| {
        inputs
            .clone()
            .map(|i| checkpoint_after(fixture, &i, d.saturating_sub(w)))
    });
    let mut origins = vec![[None, None]];
    if let Some([e, a]) = &ks {
        origins.extend([[Some(e), Some(a)], [None, Some(a)]]);
    }
    for o in origins {
        let bundle = write(
            &plan,
            &scenario,
            Builder::same(&make, verify::config()),
            inputs.clone(),
            o,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let replayed = replay(&bundle, case).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(replayed.to_bytes(), bundle.entries[RESULT]);
        assert_eq!(
            (&replayed.verdict, &replayed.window),
            (&r.verdict, &r.window)
        );
        assert_eq!(
            replayed.state.as_ref().map(|s| (s.read_at, &s.differences)),
            r.state.as_ref().map(|s| (s.read_at, &s.differences))
        );
        let m = Manifest::parse(&bundle.entries[MANIFEST]).unwrap();
        assert_eq!(m.sides.map(|s| s.origin.is_some()), o.map(|c| c.is_some()));
        let dir = std::env::temp_dir().join(format!(
            "ss-m4-{}-{}",
            std::process::id(),
            blake3::hash(&bundle.entries[MANIFEST]).to_hex()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        write_dir(&bundle, &dir).unwrap();
        let read = read_dir(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read.unwrap(), bundle);
    }
}

/// Minimizes the case of the unmodified side against `input` with a checkpoint every
/// `every` primary boundaries: each side's origin is its own index's latest checkpoint
/// covering at most the divergence at `diverged` minus the report window, or its initial
/// state, and the bundle replays byte for byte and finds what `r` finds.
fn minimized(
    fixture: &Fixture,
    b: &RuntimeBackend,
    input: InputVariant,
    r: &VerificationResult,
    diverged: Option<u64>,
    every: u64,
) {
    let plan = the_plan(b);
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let inputs = [InputVariant::None, input];
    let m = minimize(
        &plan,
        &scenario,
        Builder::same(&make, verify::config()),
        inputs.clone(),
        every,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let w = u64::from(plan.profile.report_window);
    let want = [Role::Expected, Role::Actual].map(|role| {
        let d = diverged?;
        let mut b = make();
        let i = record_index(
            &plan,
            &scenario,
            side(&mut b, inputs[role as usize].clone()),
            role,
            every,
        )
        .unwrap();
        i.checkpoints
            .iter()
            .map(|c| c.mark("arch").unwrap().covered)
            .filter(|&c| c <= d.saturating_sub(w))
            .max()
    });
    assert_eq!(m.origins, want, "every {every}");
    // Every m3 checkpoint is taken, so the latest is the last multiple of `every`.
    if let Some(d) = diverged.filter(|&d| d >= w + every) {
        let c = (d - w) / every * every;
        assert_eq!(want, [Some(c), Some(c)]);
    }
    let manifest = Manifest::parse(&m.bundle.entries[MANIFEST]).unwrap();
    assert_eq!(manifest.sides.map(|s| s.origin), want);
    let case = Case {
        plan: &plan,
        scenario: &scenario,
        sides: Builder::same(&make, verify::config()),
    };
    let replayed = replay(&m.bundle, case).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(replayed, m.result);
    assert_eq!(
        (&m.result.verdict, &m.result.window),
        (&r.verdict, &r.window)
    );
    assert_eq!(
        m.result.state.as_ref().map(|s| (s.read_at, &s.differences)),
        r.state.as_ref().map(|s| (s.read_at, &s.differences))
    );
}

fn same<'a>(
    backend: &'a mut RuntimeBackend,
    input: InputVariant,
    index: &'a Index,
) -> SameSide<'a> {
    SameSide {
        backend,
        input,
        config: verify::config(),
        checkpoints: &index.checkpoints,
    }
}

/// [`diverge`] on a `retire` whose first differing field is `field`, by a side that still
/// reaches the stop rule.
fn expect_field(
    fixture: &Fixture,
    b: &RuntimeBackend,
    expected: &Recorded,
    input: InputVariant,
    ordinal: u64,
    field: &str,
) -> StateDivergence {
    let (divergence, failures, _, state) = diverge(fixture, b, expected, input, ordinal);
    let Divergence::Fields { kind, fields } = divergence else {
        panic!("{divergence:?}")
    };
    assert_eq!((kind.as_str(), fields[0].name.as_str()), ("retire", field));
    assert!(failures.is_empty(), "{failures:?}");
    state
}

#[test]
fn unmodified_sides_agree_on_every_stream() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let (_, actual) = complete(&fixture, InputVariant::None);
    let Verdict::Compared(c) = compare(&the_plan(&b), &expected, &actual).unwrap() else {
        panic!("incomparable")
    };
    assert_eq!(c.outcome(), Outcome::Agree);
    assert!(c.not_compared.is_empty());
    let (located, _) = locate_everywhere(&fixture, &b, &expected, InputVariant::None);
    assert_eq!(located.verdict, Verdict::Compared(c.clone()));
    assert_eq!(located.window, None);
    let agree = VerificationResult {
        verdict: Ok(located.verdict.clone()),
        window: None,
        state: None,
        trace: Default::default(),
        slices: Vec::new(),
    };
    reproduce(&fixture, &b, InputVariant::None, &agree, None);
    minimized(&fixture, &b, InputVariant::None, &agree, None, 997);
    let golden = Golden::parse(GOLDEN_JSON).unwrap();
    let g = golden
        .records
        .iter()
        .find(|r| r.disk_blake3 == fixture.disk_blake3)
        .unwrap();
    // Every retirement and delivered exception, then the halting trap.
    let StreamResult::Agree { compared, .. } = c.primary else {
        unreachable!()
    };
    assert_eq!(compared, g.instret + g.exceptions + 1);
    // The frozen per-kind counts of `docs/m4-design.md` §28.1.
    let kinds = |k: &str| expected[0].iter().filter(|o| o.kind == k).count();
    assert_eq!((kinds("retire"), kinds("exception")), (2_176, 23 + 1));
    let last = expected[0].last().unwrap();
    assert_eq!(last.kind, "exception");
    assert!(is(last, "cause", Value::Str(g.halt.clone())));
    assert_eq!(u32_field(last, "pc"), g.halt_pc);
    assert!(is(last, "to", Value::Str("M".to_owned())));
    // The `effect` stream carries the UART output and the shutdown.
    let uart: Vec<u8> = expected[1]
        .iter()
        .filter(|o| o.kind == "uart_tx")
        .map(|o| u32_field(o, "byte") as u8)
        .collect();
    assert_eq!(uart, g.uart);
    assert_eq!(expected[1].last().unwrap().kind, "shutdown");
    let syscalls = expected[2]
        .iter()
        .filter(|o| o.kind == "syscall_enter")
        .count();
    assert_eq!(syscalls as u64, g.syscalls);
    // Observing changed nothing: the run ends in the golden state.
    let runtime = b.runtime().unwrap();
    assert_eq!(runtime.state_digest().unwrap(), g.state);
    assert_eq!(runtime.execution_digest(), g.execution);
}

/// SP-1: `pc ^= 0x4` after boundary 0, the first retirement, in the M-mode boot stub.
#[test]
fn sp1_a_patched_pc_diverges_at_the_next_retirement() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let arch = &expected[0];
    assert!(is(&arch[0], "priv", Value::U64(3)));
    // The checkpoint's pc is the next retirement's.
    let pc = u32_field(&arch[1], "pc");
    let event = b.event_index("arch", 0).unwrap();
    let input = patch(event, "pc", pc, pc ^ 0x4);
    expect_field(&fixture, &b, &expected, input, 1, "pc");
}

/// Whether `insn` is `ADD`, `SUB`, `XOR`, `ADDI`, or `XORI` with `rd` and `rs1` not `x0`,
/// so that flipping bit 0 of `x[rs1]` changes `rd_value`. `SUB` and `XOR` also need
/// `rs2 ≠ rs1`, since `x - x` and `x ^ x` are 0 whatever `x` is. Returns `rs1`.
fn injective_rs1(insn: u32) -> Option<u32> {
    let (opcode, rd, funct3) = (insn & 0x7f, (insn >> 7) & 0x1f, (insn >> 12) & 0x7);
    let (rs1, rs2, funct7) = ((insn >> 15) & 0x1f, (insn >> 20) & 0x1f, insn >> 25);
    let ok = match (opcode, funct3, funct7) {
        (0x13, 0 | 4, _) => true,
        (0x33, 0, 0) => true,
        (0x33, 0, 0x20) | (0x33, 4, 0) => rs2 != rs1,
        _ => false,
    };
    (ok && rd != 0 && rs1 != 0).then_some(rs1)
}

/// The registers after the first `n` observations: reset leaves them 0, and only
/// retirements write them.
fn registers(arch: &[Observation], n: usize) -> [u32; 32] {
    let mut x = [0; 32];
    for o in arch[..n].iter().filter(|o| o.kind == "retire") {
        if let Some(ObservedValue::Present(Value::U64(rd))) = o.field("rd") {
            x[*rd as usize] = u32_field(o, "rd_value");
        }
    }
    x
}

/// SP-2: `x[rs1] ^= 1` before `r`, the first U-mode retirement of an injective
/// instruction.
#[test]
fn sp2_a_patched_register_diverges_at_its_first_reader() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let arch = &expected[0];
    let (r, rs1) = arch
        .iter()
        .enumerate()
        .filter(|(_, o)| o.kind == "retire" && is(o, "priv", Value::U64(0)))
        .find_map(|(i, o)| injective_rs1(u32_field(o, "insn")).map(|rs1| (i, rs1)))
        .expect("a U-mode injective instruction");
    let old = registers(arch, r)[rs1 as usize];
    let event = b.event_index("arch", r as u64 - 1).unwrap();
    let input = patch(event, &format!("x{rs1}"), old, old ^ 1);
    expect_field(&fixture, &b, &expected, input, r as u64, "rd_value");
}

/// SP-3: `stvec ^= 0x4` before `e`, the first `EnvironmentCallFromU`; `arch[e]` agrees,
/// since no `exception` field reads `stvec`, and `arch[e + 1]` does not.
///
/// `docs/m4-design.md` §25.1 expected a `retire` at `arch[e + 1]` differing in `pc`. In
/// fact the patched handler starts at its second instruction, a store through `x31`,
/// which its first, `csrrw x31, sscratch, x31`, would have pointed at the trap frame. The
/// store faults on the user's `x31`, so `arch[e + 1]` is a `StorePageFault` where the
/// expected side retires: the same ordinal, as a kind divergence.
#[test]
fn sp3_a_patched_stvec_diverges_at_the_handler() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let arch = &expected[0];
    let e = arch
        .iter()
        .position(|o| {
            o.kind == "exception" && is(o, "cause", Value::Str("EnvironmentCallFromU".into()))
        })
        .expect("a U-mode ecall");
    // MODE 0: the handler's address is stvec itself.
    let stvec = u32_field(&arch[e + 1], "pc");
    assert_eq!(stvec & 0x3, 0);
    // `csrrw x31, sscratch, x31`.
    assert_eq!(u32_field(&arch[e + 1], "insn"), 0x140F_9FF3);
    let event = b.event_index("arch", e as u64 - 1).unwrap();
    let input = patch(event, "stvec", stvec, stvec ^ 0x4);
    let (divergence, failures, located, _) = diverge(&fixture, &b, &expected, input, e as u64 + 1);
    assert_eq!(
        divergence,
        Divergence::Kind {
            expected: "retire".to_owned(),
            actual: "exception".to_owned(),
        }
    );
    // The fault repeats at the same store, so the kernel never reaches the shutdown: the
    // side runs out of budget after the divergence, which still decides.
    let budget = format!(
        "the safety budget of {} events ran out before the stop rule",
        verify::config().budget
    );
    assert_eq!(
        failures,
        [(Role::Actual, EngineError::BackendFailed(budget))]
    );
    // A lockstep scan stops at the divergence, long before the budget.
    let Verdict::Compared(c) = &located.verdict else {
        unreachable!()
    };
    assert!(c.failures.is_empty(), "{:?}", c.failures);
    assert!(located.window.unwrap().failures.is_empty());
}

#[test]
fn a_patch_of_the_wrong_old_value_fails_the_side() {
    let fixture = fixture();
    let (_, recording) = record(&fixture, patch(1, "pc", 0xDEAD_BEEC, 0));
    let e = recording.complete().unwrap_err();
    assert!(e.to_string().contains("soc.cpu0/pc is 0x"), "{e}");
    for (field, why) in [
        ("soc.bus/x", "is not a CPU field"),
        ("soc.cpu0/x0", "is not patchable"),
        ("soc.cpu0/x32", "is not patchable"),
        ("soc.cpu0/mepc", "is not patchable"),
    ] {
        let p = StatePatch {
            event_index: 0,
            field: field.to_owned(),
            old: Value::U64(0),
            new: Value::U64(0),
        };
        let err = verify::patch(&mut [], &p).unwrap_err();
        assert!(err.ends_with(why), "{field}: {err}");
    }
}

/// M4.3 (§13): after SP-1's patch, the sides' states differ exactly in the CPU's `pc`, as
/// `inspect` and the portable decoder report it, and in the CPU's snapshot bytes.
#[test]
fn sp1_states_differ_in_the_patched_pc() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let pc = u32_field(&expected[0][1], "pc");
    let event = b.event_index("arch", 0).unwrap();
    let plan = the_plan(&b);
    let scenario = verify::scenario(&fixture);
    let mut states = Vec::new();
    for input in [InputVariant::None, patch(event, "pc", pc, pc ^ 0x4)] {
        let mut b = verify::backend(&fixture);
        b.start(&scenario, &input, &verify::config(), &plan.profile)
            .unwrap();
        b.next("arch").unwrap().unwrap();
        states.push(b.state().unwrap());
    }
    let d = diff_state(&states[0], &states[1]);
    let named: Vec<(&str, &str, String)> = d
        .iter()
        .map(|d| (d.component.as_str(), d.field.as_str(), d.source.to_string()))
        .collect();
    assert_eq!(
        named,
        [
            (CPU_PATH, "pc", "inspect".to_owned()),
            (CPU_PATH, "blake3", "derived:snapshot".to_owned()),
            (CPU_PATH, "pc", "decoder:m3-portable/3".to_owned()),
        ]
    );
    let pcs = (Value::U64(pc.into()), Value::U64((pc ^ 0x4).into()));
    for i in [0, 2] {
        assert_eq!(
            d[i].change,
            Change::Changed {
                expected: pcs.0.clone(),
                actual: pcs.1.clone(),
            }
        );
    }
    // Every component has its snapshot digest and decoded fields; RAM's are page
    // digests.
    for path in systemscope_rv32::m3ref::PATHS {
        let decoded = if path == "soc.ram" {
            "derived:page blake3"
        } else {
            "decoder:m3-portable/"
        };
        for source in ["derived:snapshot", decoded] {
            let read = states[0]
                .iter()
                .any(|f| f.component == path && f.source.to_string().starts_with(source));
            assert!(read, "{path} {source}");
        }
    }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// Entry 0 of the executable table, `hello`: the disk offset of the word at its ELF's
/// `e_entry`, inside a `PT_LOAD` segment's file bytes, and `e_entry` (m3-design §8.1).
fn hello_entry(disk: &[u8]) -> (u64, u32) {
    let le16 = |b: &[u8], at: usize| usize::from(u16::from_le_bytes([b[at], b[at + 1]]));
    assert_eq!(le32(disk, 0), 0x3058_5353, "SSX0");
    let (lba, len) = (le32(disk, 0x10) as usize, le32(disk, 0x14) as usize);
    let elf = &disk[lba * 512..][..len];
    let entry = le32(elf, 0x18);
    let (phoff, size, count) = (le32(elf, 0x1C) as usize, le16(elf, 0x2A), le16(elf, 0x2C));
    let offset = (0..count)
        .map(|i| &elf[phoff + i * size..])
        .find_map(|ph| {
            let (kind, offset, vaddr, filesz) =
                (le32(ph, 0), le32(ph, 4), le32(ph, 8), le32(ph, 16));
            let inside = entry >= vaddr && entry + 4 <= vaddr + filesz;
            (kind == 1 && inside).then_some(offset + entry - vaddr)
        })
        .expect("e_entry inside a PT_LOAD segment");
    ((lba * 512) as u64 + u64::from(offset), entry)
}

/// The first `arch` ordinal where `hello`, pid 1, is current, in U-mode, at `entry`:
/// `os.process.create` gives its entry, and the last `os.process.switch` at or before a
/// boundary's event the current pid.
fn hello_starts(b: &RuntimeBackend, expected: &Recorded, entry: u32) -> u64 {
    let os = &expected[2];
    let created = os
        .iter()
        .find(|o| o.kind == "create" && u32_field(o, "pid") == 1)
        .expect("hello is created");
    assert_eq!(u32_field(created, "entry"), entry);
    let switches: Vec<(u64, u32)> = os
        .iter()
        .enumerate()
        .filter(|(_, o)| o.kind == "switch")
        .map(|(k, o)| (b.event_index("os", k as u64).unwrap(), u32_field(o, "to")))
        .collect();
    let x = expected[0].iter().enumerate().position(|(i, o)| {
        let event = b.event_index("arch", i as u64).unwrap();
        let current = switches.iter().take_while(|(e, _)| *e <= event).last();
        is(o, "priv", Value::U64(0))
            && is(o, "pc", Value::U64(entry.into()))
            && current.is_some_and(|(_, pid)| *pid == 1)
    });
    x.expect("hello starts") as u64
}

/// Replaces `hello`'s entry word on the disk with `new` of the word it holds.
fn code_patch(disk: &[u8], new: impl Fn(u32) -> u32) -> (InputVariant, u32) {
    let (at, entry) = hello_entry(disk);
    let old = le32(disk, at as usize);
    let input = InputVariant::Media(MediaPatch {
        offset: at,
        old: old.to_le_bytes().to_vec(),
        new: new(old).to_le_bytes().to_vec(),
    });
    (input, entry)
}

/// CB-1: `hello`'s first instruction becomes `addi x0, x0, 0`, or `addi x0, x0, 1` if it
/// already is that; `arch[x]` differs in `insn`.
#[test]
fn cb1_a_patched_code_word_diverges_at_its_first_retirement() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let (input, entry) = code_patch(&fixture.disk, |w| {
        if w == 0x0000_0013 {
            0x0010_0013
        } else {
            0x0000_0013
        }
    });
    let x = hello_starts(&b, &expected, entry);
    expect_field(&fixture, &b, &expected, input, x, "insn");
}

/// CB-2: `hello`'s first instruction becomes 0, an illegal instruction delegated to S;
/// `arch[x]` is an exception where the expected side retires.
#[test]
fn cb2_an_illegal_code_word_diverges_as_an_exception() {
    let fixture = fixture();
    let (b, expected) = complete(&fixture, InputVariant::None);
    let (input, entry) = code_patch(&fixture.disk, |_| 0);
    let x = hello_starts(&b, &expected, entry);
    let (divergence, failures, _, _) = diverge(&fixture, &b, &expected, input, x);
    assert_eq!(
        divergence,
        Divergence::Kind {
            expected: "retire".to_owned(),
            actual: "exception".to_owned(),
        }
    );
    assert!(failures.is_empty(), "{failures:?}");
}
