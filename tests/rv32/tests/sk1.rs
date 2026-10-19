//! SK-1, the Spike-side controlled divergence (`docs/m4-design.md` §25.1, §28.2), against
//! the committed Spike log of `rv32ui-add`, with no Spike needed: the locator, by stream
//! compare, and the naive oracle both find the selector's divergence, and the minimized
//! heterogeneous reproducer starts SystemScope at its window and replays it.

use std::fs;

use systemscope_contracts::trace::Value;
use systemscope_rv32::spike::ADD_LOG;
use systemscope_rv32::spike_adapter::{
    CPU_PATH, Sk1, Source, SpikeBackend, Suite, agreed, config, injective_rs1, located, patch,
    systemscope,
};
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::bundle::{Case, validate};
use systemscope_verify::case::{InputVariant, StatePatch};
use systemscope_verify::compare::{Divergence, StreamResult, Verdict};
use systemscope_verify::locate::Strategy;
use systemscope_verify::observation::Observation;

fn sk1() -> (Sk1, Source) {
    let root = workspace_root();
    let log = fs::read_to_string(root.join(ADD_LOG)).unwrap();
    (Sk1::select(&root).unwrap(), Source::Log(log))
}

/// One side's whole `arch` stream.
fn arch(
    sk1: &Sk1,
    backend: &mut dyn Backend,
    input: InputVariant,
    spike: bool,
) -> Vec<Observation> {
    let config = match spike {
        true => Suite::Rv32ui.spike_config(),
        false => config(),
    };
    let side = Side {
        backend,
        input,
        config,
    };
    let recording = run(&sk1.plan().unwrap(), &sk1.scenario(), side);
    assert_eq!(recording.failure, None);
    recording.streams.into_iter().next().unwrap()
}

#[test]
fn sk1_is_located_where_the_selector_says() {
    let (sk1, log) = sk1();
    let tohost = Some(sk1.tohost);
    // The unpatched sides agree over the whole stream.
    let n = agreed(Suite::Rv32ui, &sk1.image, log.clone(), tohost).unwrap();
    assert!(sk1.r > 3 && sk1.r < n, "{} of {n}", sk1.r);
    let InputVariant::State(p) = &sk1.patch else {
        panic!("{:?}", sk1.patch)
    };
    assert!(p.field.starts_with(&format!("{CPU_PATH}/x")), "{p:?}");
    // The locator.
    let mut spike = SpikeBackend::new(Suite::Rv32ui, log.clone(), tohost);
    let mut ours = systemscope(Suite::Rv32ui, &sk1.image, tohost);
    let found = located(
        Suite::Rv32ui,
        &sk1.image,
        &mut spike,
        (&mut ours, sk1.patch.clone()),
    )
    .unwrap();
    assert_eq!(found.strategy, Strategy::StreamCompare);
    let Verdict::Compared(c) = &found.verdict else {
        panic!("{:?}", found.verdict)
    };
    let StreamResult::Diverged {
        ordinal,
        divergence: Divergence::Fields { kind, fields },
        ..
    } = &c.primary
    else {
        panic!("{:?}", c.primary)
    };
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        (*ordinal, kind.as_str(), names),
        (sk1.r, "retire", vec!["rd_value"])
    );
    // The naive oracle: both complete streams, and their first difference.
    let mut spike = SpikeBackend::new(Suite::Rv32ui, log, tohost);
    let expected = arch(&sk1, &mut spike, InputVariant::None, true);
    let mut ours = systemscope(Suite::Rv32ui, &sk1.image, tohost);
    let actual = arch(&sk1, &mut ours, sk1.patch.clone(), false);
    let naive = (0..).find(|&i| expected.get(i) != actual.get(i)).unwrap();
    assert_eq!(naive as u64, sk1.r);
}

#[test]
fn the_minimized_sk1_reproducer_starts_systemscope_at_its_window() {
    let (sk1, log) = sk1();
    let full = sk1.minimized(&log, 0).unwrap();
    assert_eq!(full.origins, [None, None]);
    sk1.check(&full.result).unwrap();
    let mut moved = sk1.clone();
    moved.r += 1;
    assert!(moved.check(&full.result).is_err());
    for every in [1, 2] {
        let m = sk1.minimized(&log, every).unwrap();
        // The latest checkpoint at most the report window before the divergence; Spike
        // starts from its initial state.
        let want = (sk1.r - 3) / every * every;
        assert_eq!(m.origins, [None, Some(want)], "{every}");
        sk1.check(&m.result).unwrap();
        assert_eq!(m.result.verdict, full.result.verdict);
        assert_eq!(m.result.window, full.result.window);
        assert_eq!(sk1.replay(&m.bundle, &log).unwrap(), m.result);
        let (plan, scenario) = (sk1.plan().unwrap(), sk1.scenario());
        let tohost = Some(sk1.tohost);
        let spike = || -> Box<dyn Backend> {
            Box::new(SpikeBackend::new(Suite::Rv32ui, log.clone(), tohost))
        };
        let ours = || systemscope(Suite::Rv32ui, &sk1.image, tohost);
        let sides = [
            systemscope_verify::report::Builder {
                make: systemscope_verify::report::Make::Other(&spike),
                config: Suite::Rv32ui.spike_config(),
                perturbation: None,
            },
            systemscope_verify::report::Builder {
                make: systemscope_verify::report::Make::Runtime(&ours),
                config: config(),
                perturbation: None,
            },
        ];
        let case = Case {
            plan: &plan,
            scenario: &scenario,
            sides,
        };
        assert!(validate(&m.bundle, case).is_ok());
    }
    // Replayed against another Spike stream, the reproducer no longer fails the same way.
    let unpatched = Source::Log(log_without(&log, sk1.r));
    assert!(
        sk1.replay(&sk1.minimized(&log, 1).unwrap().bundle, &unpatched)
            .is_err()
    );
}

/// `log` with bit 0 of retirement `r`'s register value flipped: another Spike stream.
fn log_without(log: &Source, r: u64) -> String {
    let Source::Log(text) = log else { panic!() };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let line = &mut lines[r as usize];
    let (head, value) = line.rsplit_once(' ').unwrap();
    let v = u32::from_str_radix(value.trim_start_matches("0x"), 16).unwrap() ^ 1;
    *line = format!("{head} {v:#010x}");
    lines.iter().map(|l| format!("{l}\n")).collect()
}

#[test]
fn the_cpu_patch_checks_its_field_and_old_value() {
    let (sk1, _) = sk1();
    let mut b = systemscope(Suite::Rv32ui, &sk1.image, Some(sk1.tohost));
    let plan = sk1.plan().unwrap();
    b.start(
        &sk1.scenario(),
        &InputVariant::None,
        &config(),
        &plan.profile,
    )
    .unwrap();
    b.next("arch").unwrap();
    let bytes = b.runtime().unwrap().snapshot().unwrap();
    let pc = match b.state().unwrap().iter().find(|f| f.field == "pc") {
        Some(f) => f.value.clone(),
        None => panic!("no pc"),
    };
    let p = |field: &str, old: Value| StatePatch {
        event_index: 0,
        field: format!("{CPU_PATH}/{field}"),
        old,
        new: Value::U64(4),
    };
    let mut patched = bytes.clone();
    assert_eq!(patch(&mut patched, &p("pc", pc.clone())), Ok(()));
    assert_ne!(patched, bytes);
    // The old value must be the one held.
    let mut again = patched.clone();
    assert!(patch(&mut again, &p("pc", pc)).unwrap_err().contains("not"));
    for field in ["x0", "x32", "mstatus"] {
        let err = patch(&mut bytes.clone(), &p(field, Value::U64(0))).unwrap_err();
        assert!(err.contains("not patchable"), "{err}");
    }
    let mut other = p("x1", Value::U64(0));
    other.field = "soc.ram/x1".to_owned();
    assert!(
        patch(&mut bytes.clone(), &other)
            .unwrap_err()
            .contains("not a CPU field")
    );
    // Only injective instructions are selected.
    assert_eq!(injective_rs1(0x0020_81b3), Some(1)); // add x3, x1, x2
    assert_eq!(injective_rs1(0x4010_81b3), None); // sub x3, x1, x1
    assert_eq!(injective_rs1(0x0000_8193), Some(1)); // addi x3, x1, 0
    assert_eq!(injective_rs1(0x0000_0193), None); // addi x3, x0, 0
    assert_eq!(injective_rs1(0x0000_8013), None); // addi x0, x1, 0
    assert_eq!(injective_rs1(0x0020_f1b3), None); // and x3, x1, x2
}
