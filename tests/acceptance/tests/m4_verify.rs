//! M4.1 on `m3-reference` (`docs/m4-design.md` §25.1): the unmodified sides agree, and
//! the state-patch controlled divergences SP-1 to SP-3 diverge exactly where a selector,
//! a pure function of the reference `arch` stream, says they must, which the naive oracle
//! confirms. M4.2 (§25.2): every locator strategy, and checkpoint search at every
//! density, finds the same first divergence and reports the same window.

use systemscope_acceptance::m3::Fixture;
use systemscope_acceptance::m3::golden::Golden;
use systemscope_acceptance::m3::verify::{self, CPU_PATH};
use systemscope_contracts::trace::Value;
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::case::{InputVariant, StatePatch};
use systemscope_verify::compare::{
    Divergence, Outcome, Recorded, Recording, StreamResult, Verdict, compare, compare_recordings,
    naive_first_divergence,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{
    Located, Strategy, lockstep, record_index, search, stream_compare,
};
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::runtime::RuntimeBackend;

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
/// returns the lockstep result.
fn locate_everywhere(
    fixture: &Fixture,
    b: &RuntimeBackend,
    expected: &Recorded,
    input: InputVariant,
) -> Located {
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
    results.swap_remove(0)
}

/// Runs the patched side and checks that the comparator, and the naive oracle, put the
/// first divergence at `ordinal`, as every locator strategy does against `b`. Returns the
/// divergence, the patched side's failures after it, if any, and the lockstep result.
fn diverge(
    fixture: &Fixture,
    b: &RuntimeBackend,
    expected: &Recorded,
    input: InputVariant,
    ordinal: u64,
) -> (Divergence, Vec<(Role, EngineError)>, Located) {
    let located = locate_everywhere(fixture, b, expected, input.clone());
    let (b, actual) = record(fixture, input);
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
    (divergence, c.failures, located)
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
) {
    let (divergence, failures, _) = diverge(fixture, b, expected, input, ordinal);
    let Divergence::Fields { kind, fields } = divergence else {
        panic!("{divergence:?}")
    };
    assert_eq!((kind.as_str(), fields[0].name.as_str()), ("retire", field));
    assert!(failures.is_empty(), "{failures:?}");
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
    let located = locate_everywhere(&fixture, &b, &expected, InputVariant::None);
    assert_eq!(located.verdict, Verdict::Compared(c.clone()));
    assert_eq!(located.window, None);
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
    let (divergence, failures, located) = diverge(&fixture, &b, &expected, input, e as u64 + 1);
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
