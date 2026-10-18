//! The Spike adapter and the Spike profile versions (`docs/m4-design.md` §8.2, §9.2):
//! the committed Spike logs agree with SystemScope through the M4 engine, by stream
//! compare, and every way Spike's stream can differ is a divergence where it starts.

use std::fs;

use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_rv32::manifest::load;
use systemscope_rv32::progen::{self, MISALIGNED};
use systemscope_rv32::projection::{HALT_TRAP, WRITE_TOHOST};
use systemscope_rv32::spike::{
    PROGEN_LOG, SIMPLE_LOG, TRAP_LOG, TRAP_LOG_CASE, elf_symbol, parse_spike_l_log,
};
use systemscope_rv32::spike_adapter::{
    Source, SpikeBackend, Suite, agreed, located, machine, systemscope,
};
use systemscope_rv32::{FIXTURE_DIR, workspace_root};
use systemscope_verify::backend::{Backend, Capability};
use systemscope_verify::case::{InputVariant, StatePatch};
use systemscope_verify::compare::{Divergence, StreamResult, Verdict};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::Strategy;
use systemscope_verify::profile::plan;

fn log(path: &str) -> String {
    fs::read_to_string(workspace_root().join(path)).unwrap()
}

/// `rv32ui-simple`'s image and `tohost`.
fn simple() -> (LoadImage, u32) {
    let bytes = fs::read(workspace_root().join(FIXTURE_DIR).join("rv32ui-simple.elf")).unwrap();
    (
        load("simple", &bytes).unwrap(),
        elf_symbol(&bytes, "tohost").unwrap(),
    )
}

/// The generated program of seed 0's image and `tohost`.
fn progen_0() -> (LoadImage, u32) {
    let program = progen::generate(0);
    (
        load(&program.name, &program.elf).unwrap(),
        elf_symbol(&program.elf, "tohost").unwrap(),
    )
}

/// `misaligned-lw-1`'s image.
fn lw_1() -> LoadImage {
    let (program, _) = progen::misaligned(&MISALIGNED[TRAP_LOG_CASE]);
    load(&program.name, &program.elf).unwrap()
}

#[test]
fn the_committed_logs_agree_through_the_engine() {
    let (image, tohost) = simple();
    let simple = Source::Log(log(SIMPLE_LOG));
    assert_eq!(agreed(Suite::Rv32ui, &image, simple, Some(tohost)), Ok(40));
    let (image, tohost) = progen_0();
    let progen = Source::Log(log(PROGEN_LOG));
    assert_eq!(agreed(Suite::Rv32ui, &image, progen, Some(tohost)), Ok(303));
    // Five retirements, then the trap taken in M.
    let trap = Source::Log(log(TRAP_LOG));
    assert_eq!(agreed(Suite::Misaligned, &lw_1(), trap, None), Ok(6));
}

#[test]
fn spike_is_compared_by_stream_compare_as_the_expected_side() {
    let (image, tohost) = simple();
    let mut spike = SpikeBackend::new(Suite::Rv32ui, Source::Log(log(SIMPLE_LOG)), Some(tohost));
    assert_eq!(
        spike.capabilities(),
        [Capability::ArchStream, Capability::DeterministicReplay].into()
    );
    assert!(matches!(
        spike.next("arch"),
        Err(EngineError::Unsupported(_))
    ));
    assert!(matches!(
        spike.record("arch"),
        Err(EngineError::Internal(_))
    ));
    let mut ours = systemscope(Suite::Rv32ui, &image, Some(tohost));
    let found = located(
        Suite::Rv32ui,
        &image,
        &mut spike,
        (&mut ours, InputVariant::None),
    )
    .unwrap();
    assert_eq!(found.strategy, Strategy::StreamCompare);
    // Spike runs only the committed program.
    let patch = InputVariant::State(StatePatch {
        event_index: 1,
        field: "soc.cpu0/x1".to_owned(),
        old: Value::U64(0),
        new: Value::U64(1),
    });
    let p = plan(&Suite::Rv32ui.profile(), &spike.support(), &ours.support()).unwrap();
    let scenario = systemscope_rv32::spike_adapter::scenario(Suite::Rv32ui, &image);
    let refused = spike.start(&scenario, &patch, &Suite::Rv32ui.spike_config(), &p.profile);
    assert!(
        matches!(refused, Err(EngineError::Unsupported(_))),
        "{refused:?}"
    );
    // Nor another suite's profile.
    let other = Suite::M2.profile();
    let refused = spike.start(
        &scenario,
        &InputVariant::None,
        &Suite::M2.spike_config(),
        &other,
    );
    assert!(
        matches!(refused, Err(EngineError::Internal(_))),
        "{refused:?}"
    );
}

/// The divergence of SystemScope's `simple` against Spike's doctored stream.
fn diverged(doctor: &dyn Fn(&mut Vec<systemscope_rv32::spike::Event>)) -> (u64, Divergence) {
    let (image, tohost) = simple();
    let mut events: Vec<_> = systemscope_rv32::spike::parse_spike_log(&log(SIMPLE_LOG))
        .unwrap()
        .into_iter()
        .map(machine)
        .collect();
    doctor(&mut events);
    let mut spike = SpikeBackend::new(Suite::Rv32ui, Source::Parsed(events, None), Some(tohost));
    let mut ours = systemscope(Suite::Rv32ui, &image, Some(tohost));
    let found = located(
        Suite::Rv32ui,
        &image,
        &mut spike,
        (&mut ours, InputVariant::None),
    )
    .unwrap();
    match found.verdict {
        Verdict::Compared(c) => match c.primary {
            StreamResult::Diverged {
                ordinal,
                divergence,
                ..
            } => (ordinal, divergence),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_different_spike_stream_diverges_where_it_starts() {
    use systemscope_rv32::spike::{Event, Mem};
    let retire = |e: &mut Event| match e {
        Event::Retire { retire, .. } => *retire,
        Event::Exception { .. } => panic!(),
    };
    let set =
        |events: &mut Vec<Event>, i: usize, f: &dyn Fn(&mut systemscope_rv32::spike::Retire)| {
            let mut r = retire(&mut events[i]);
            f(&mut r);
            events[i] = machine(r);
        };
    assert_eq!(diverged(&|e| set(e, 10, &|r| r.pc += 4)).0, 10);
    assert_eq!(diverged(&|e| set(e, 3, &|r| r.insn ^= 1)).0, 3);
    let (at, d) = diverged(&|e| {
        set(e, 5, &|r| {
            r.reg_write = r.reg_write.map(|(rd, v)| (rd, v ^ 1))
        });
    });
    assert_eq!(at, 5, "{d:?}");
    // A register write where SystemScope makes none.
    let (at, d) = diverged(&|e| {
        let i = e
            .iter_mut()
            .position(|e| retire(e).reg_write.is_none())
            .unwrap();
        set(e, i, &|r| r.reg_write = Some((7, 0)));
    });
    assert!(matches!(d, Divergence::Fields { .. }), "{at} {d:?}");
    let (at, _) = diverged(&|e| set(e, 20, &|r| r.mem = Mem::Load { addr: 0 }));
    assert_eq!(at, 20);
}

#[test]
fn a_stream_without_its_end_fails_on_the_spike_side() {
    let (image, tohost) = simple();
    // Cut before the end of write_tohost.
    let mut events: Vec<_> = systemscope_rv32::spike::parse_spike_log(&log(SIMPLE_LOG))
        .unwrap()
        .into_iter()
        .map(machine)
        .collect();
    events.pop();
    let cut = Source::Parsed(events, None);
    let err = agreed(Suite::Rv32ui, &image, cut, Some(tohost)).unwrap_err();
    assert!(err.contains("before write_tohost"), "{err}");
    // A trap program's log cut before its trap.
    let (retires, _) = parse_spike_l_log(&log(TRAP_LOG)).unwrap();
    let no_trap = Source::Parsed(retires.into_iter().map(machine).collect(), None);
    let err = agreed(Suite::Misaligned, &lw_1(), no_trap, None).unwrap_err();
    assert!(err.contains("before a trap taken in M"), "{err}");
    // An unparsable log.
    let garbage = Source::Log(format!("{}garbage\n", log(SIMPLE_LOG)));
    let err = agreed(Suite::Rv32ui, &image, garbage, Some(tohost)).unwrap_err();
    assert!(err.contains("garbage"), "{err}");
}

#[test]
fn the_four_profiles_keep_the_existing_rules() {
    let names: Vec<_> = Suite::ALL.iter().map(|s| s.profile().name).collect();
    assert_eq!(
        names,
        [
            "spike-rv32ui",
            "spike-misaligned",
            "spike-m2-csr",
            "spike-m3-privilege"
        ]
    );
    let stops: Vec<_> = Suite::ALL.iter().map(|s| s.profile().stop_rule).collect();
    assert_eq!(stops, [WRITE_TOHOST, HALT_TRAP, WRITE_TOHOST, HALT_TRAP]);
    let (image, tohost) = simple();
    for suite in Suite::ALL {
        let p = suite.profile();
        assert_eq!((p.version, p.report_window), (1, 3));
        assert!(p.auxiliary.is_empty());
        let retire = p.primary.kind("retire").unwrap();
        let fields: Vec<_> = retire.fields.iter().map(|f| f.name.as_str()).collect();
        // `paddr` is never compared against Spike; `priv` only in M3, CSRs from M2.
        assert!(!fields.contains(&"paddr"));
        assert!(retire.excluded.iter().any(|f| f == "paddr"));
        assert_eq!(fields.contains(&"priv"), suite == Suite::M3);
        assert_eq!(
            fields.contains(&"csr_value"),
            matches!(suite, Suite::M2 | Suite::M3)
        );
        // Only rv32ui compares no exception.
        assert_eq!(
            p.primary.kind("exception").is_some(),
            suite != Suite::Rv32ui
        );
        // Both sides support every field.
        let spike = SpikeBackend::new(suite, Source::Log(String::new()), Some(tohost));
        let ours = systemscope(suite, &image, Some(tohost));
        let narrowed = plan(&p, &spike.support(), &ours.support()).unwrap();
        assert!(narrowed.not_compared.is_empty());
    }
}
