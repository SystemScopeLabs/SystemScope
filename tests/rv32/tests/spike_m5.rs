//! M5.2's Spike differential without Spike (`docs/m5-design.md` §21): two committed logs
//! of the pinned Spike, `m5-deleg-u` (a delegated SEI taken from U into S) and
//! `m5-u-nondeleg` (a non-delegated SEI taken from U into M), parsed strictly and judged
//! against SystemScope's `M5` run through the M4 stream compare. The full run against
//! Spike is `cargo xtask spike`.

use std::fs;

use systemscope_rv32::manifest::load;
use systemscope_rv32::progen::{ExpectedTrap, Program};
use systemscope_rv32::runner::{self, Finished, Start};
use systemscope_rv32::spike::{Event, judge_m3, m5_programs, parse_spike_m3_log};
use systemscope_rv32::spike_adapter::{Source, Suite, agreed};
use systemscope_rv32::{seigen, workspace_root};
use systemscope_rv32i::Rv32iProfile;

const DELEG_U: &str = "tests/rv32/spike/m5-deleg-u.log";
const NONDELEG_U: &str = "tests/rv32/spike/m5-u-nondeleg.log";

fn log(path: &str) -> String {
    fs::read_to_string(workspace_root().join(path)).unwrap()
}

fn program(name: &str) -> (Program, ExpectedTrap) {
    seigen::programs()
        .into_iter()
        .find(|(p, _)| p.name == name)
        .unwrap()
}

fn run(p: &Program) -> Finished {
    let image = load(&p.name, &p.elf).unwrap();
    runner::execute(
        runner::platform_with_profile(&image, false, runner::SEED, Rv32iProfile::M5),
        Start::Init { traced: true },
        Vec::new(),
    )
}

fn interrupts(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| matches!(e, Event::Interrupt { .. }))
        .collect()
}

#[test]
fn m5_runs_the_sei_programs_then_every_m3_2_program() {
    let names: Vec<String> = m5_programs().into_iter().map(|(p, _)| p.name).collect();
    assert_eq!(names.len(), 7 + 13);
    assert!(names[..7].iter().all(|n| n.starts_with("m5-")));
    assert!(names[7..].iter().all(|n| n.starts_with("m3-")));
}

#[test]
fn the_committed_logs_agree_with_systemscope() {
    for (path, name, to) in [(DELEG_U, "m5-deleg-u", 1), (NONDELEG_U, "m5-u-nondeleg", 3)] {
        let (p, expected) = program(name);
        let (theirs, trap) = parse_spike_m3_log(&log(path)).unwrap();
        let taken = interrupts(&theirs);
        assert_eq!(taken.len(), 1, "{name}");
        assert!(
            matches!(taken[0], Event::Interrupt { cause: 0x8000_0009, to: t, .. } if *t == to),
            "{name}: {:?}",
            taken[0]
        );
        let image = load(&p.name, &p.elf).unwrap();
        let n = judge_m3(Suite::M5, &run(&p), &image, &theirs, &trap, &expected).unwrap();
        assert_eq!(n, theirs.len(), "{name}");
    }
}

#[test]
fn spike_writes_of_mip_and_sie_compare_as_systemscope_records_them() {
    let (theirs, _) = parse_spike_m3_log(&log(DELEG_U)).unwrap();
    let csr_writes: Vec<(u16, u32)> = theirs
        .iter()
        .filter_map(|e| match e {
            Event::Retire { retire, .. } => retire.csr,
            _ => None,
        })
        .filter(|(csr, _)| matches!(csr, 0x344 | 0x104))
        .collect();
    // `mvip` + `mip` 0x280 is `mip` 0x200; `sie` + `mie` is `sie`.
    assert_eq!(csr_writes, [(0x344, 0x200), (0x104, 0)]);
}

#[test]
fn an_interrupt_is_compared() {
    let (p, _) = program("m5-deleg-u");
    let image = load(&p.name, &p.elf).unwrap();
    let text = log(DELEG_U);
    // Moved to M, or with another cause, it diverges.
    for edit in [
        |e: &mut Event| {
            if let Event::Interrupt { to, .. } = e {
                *to = 3;
            }
        },
        |e: &mut Event| {
            if let Event::Interrupt { cause, .. } = e {
                *cause = 0x8000_000b;
            }
        },
    ] {
        let (mut theirs, trap) = parse_spike_m3_log(&text).unwrap();
        theirs.iter_mut().for_each(edit);
        let err = agreed(Suite::M5, &image, Source::Parsed(theirs, Some(trap)), None).unwrap_err();
        assert!(err.contains("diverges"), "{err}");
    }
    // Dropped, the handler's retirements no longer line up.
    let (theirs, trap) = parse_spike_m3_log(&text).unwrap();
    let dropped: Vec<Event> = theirs
        .into_iter()
        .filter(|e| !matches!(e, Event::Interrupt { .. }))
        .collect();
    assert!(agreed(Suite::M5, &image, Source::Parsed(dropped, Some(trap)), None).is_err());
    // An interrupt is never part of the M3 profile.
    let err = agreed(Suite::M3, &image, Source::Log(text), None).unwrap_err();
    assert!(err.contains("interrupt"), "{err}");
}

#[test]
fn a_malformed_interrupt_line_is_rejected() {
    let text = log(DELEG_U);
    let line = "core   0: exception interrupt #9, epc 0x8000006c";
    assert!(text.contains(line));
    for bad in [
        "core   0: exception interrupt #99, epc 0x8000006c",
        "core   0: exception interrupt #x, epc 0x8000006c",
        "core   0: exception interrupt #9, epc 0x8000006",
        "core   0: exception interrupt #9",
    ] {
        assert!(
            parse_spike_m3_log(&text.replace(line, bad)).is_err(),
            "{bad}"
        );
    }
    // An interrupt with no handler after it.
    let cut = &text[..text.find(line).unwrap() + line.len() + 1];
    assert!(parse_spike_m3_log(cut).is_err());
}
