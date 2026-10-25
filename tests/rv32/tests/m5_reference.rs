//! M5.3b: `m5-reference` and its workload (`docs/m5-design.md` §11, §12).
//!
//! Everything here runs the committed firmware and disk on the real `m5-reference`: the
//! M3 boot path loads the six entries, the timer preempts the two `counter`s, the two
//! `reader`s block on the block controller, `spinner` and `handoff` share a mailbox, and
//! the shutdown dumps the shared words. Results are read from the components' views,
//! their trace records, and snapshots, never from the ELFs.

use std::fs;
use std::path::PathBuf;

use systemscope_contracts::trace::TraceOrigin;
use systemscope_elf::{BLOCK_SIZE, LoadImage, parse_exec_table};
use systemscope_os::Window;
use systemscope_rv32::m3::os_trace;
use systemscope_rv32::m3ref::{DISK_BLOCKS, STAGING_SIZE};
use systemscope_rv32::m5::{
    self, ENTRIES, M5_FILES, M5_INPUTS, M5_SCRIPT, M5Manifest, M5Run, Metrics, PROGRAMS,
    expected_final, expected_lines, judge, judge_without_trace,
};
use systemscope_rv32::m5ref::{
    self, BLK_SOURCE, BuildError, Config, DATA_BLOCKS, DATA_LBA, SLICE, TIMER_SOURCE,
};
use systemscope_rv32::runner::Start;
use systemscope_rv32::{TOOLCHAIN, m2ref, workspace_root};

fn root() -> PathBuf {
    workspace_root()
}

fn fixture() -> (LoadImage, Vec<u8>, String) {
    m5::read_fixture(&root()).unwrap()
}

fn fresh(traced: bool) -> M5Run {
    let (firmware, disk, _) = fixture();
    m5::run(&firmware, &disk, Start::Init { traced }, Vec::new())
}

#[test]
fn the_committed_m5_fixtures_match_their_manifest() {
    let root = root();
    let manifest = m5::verify(&root).unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(manifest, M5Manifest::read(&root).unwrap());
    let programs: Vec<&str> = manifest.programs.iter().map(|p| p.file.as_str()).collect();
    let expected: Vec<String> = PROGRAMS.iter().map(|p| format!("{p}.elf")).collect();
    assert_eq!(programs, expected);
    assert_eq!(manifest.disk.programs, ENTRIES);
    let (_, disk, trace) = fixture();
    // The media is named by the disk's BLAKE3, which keys the expected trace.
    assert_eq!(m2ref::media_image(&disk).image_hash, manifest.disk.blake3);
    assert!(trace.starts_with("os.boot entries=6"));
    let inputs: Vec<&str> = manifest.inputs.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(inputs, M5_INPUTS);
    let mut files: Vec<String> = fs::read_dir(root.join(m5::M5_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, M5_FILES);
}

#[test]
fn the_disk_holds_the_entries_then_the_data_blocks() {
    let (_, disk, _) = fixture();
    let table: &[u8; BLOCK_SIZE] = disk[..BLOCK_SIZE].try_into().unwrap();
    let blocks = (disk.len() / BLOCK_SIZE) as u32;
    let entries = parse_exec_table(table, blocks, STAGING_SIZE as u32)
        .unwrap()
        .entries;
    assert_eq!(entries.len(), ENTRIES.len());
    let end = entries
        .iter()
        .map(|e| u64::from(e.start_lba) + u64::from(e.blocks()))
        .max()
        .unwrap();
    assert!(
        end <= u64::from(DATA_LBA),
        "the executables reach block {end}"
    );
    for lba in DATA_LBA..DATA_LBA + DATA_BLOCKS {
        let at = lba as usize * BLOCK_SIZE;
        assert_eq!(disk[at..at + BLOCK_SIZE], m5ref::data_block(lba));
    }
    // The rest of the capacity is zero.
    let end = (DATA_LBA + DATA_BLOCKS) as usize * BLOCK_SIZE;
    assert_eq!(disk.len() as u64, DISK_BLOCKS * BLOCK_SIZE as u64);
    assert!(disk[end..].iter().all(|&b| b == 0));
}

/// The value of `NAME=value` or `NAME='value'` in a build script.
fn script_value<'a>(script: &'a str, name: &str) -> &'a str {
    script
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name}"))
        .trim_matches('\'')
        .trim_matches('"')
}

#[test]
fn the_build_script_pins_what_the_crate_relies_on() {
    let root = root();
    let script = fs::read_to_string(root.join(M5_SCRIPT)).unwrap();
    let m3 = fs::read_to_string(root.join(systemscope_rv32::m3::M3_SCRIPT)).unwrap();
    assert_eq!(
        script_value(&script, "GCC_VERSION"),
        TOOLCHAIN[0].version_line
    );
    assert_eq!(
        script_value(&script, "AS_VERSION"),
        TOOLCHAIN[1].version_line
    );
    // The same flags as M3's, which the manifest records as M3's constants.
    for line in ["COMMON=(", "FIRMWARE_FLAGS=(", "USER_FLAGS=("] {
        let of = |s: &str| {
            let at = s.find(line).unwrap_or_else(|| panic!("no {line}"));
            s[at..].split_once(')').unwrap().0.to_owned()
        };
        assert_eq!(of(&script), of(&m3), "{line}");
    }
    let programs: Vec<&str> = script_value(&script, "USER_PROGRAMS").split(' ').collect();
    assert_eq!(programs, PROGRAMS);
    for check in [
        "! grep -qx wfi",
        "R_RISCV_(RELAX|GPREL)",
        "__global_pointer",
        "two builds differ",
    ] {
        assert!(script.contains(check), "the script lacks {check:?}");
    }
    for name in PROGRAMS.iter().copied().chain(["firmware"]) {
        let source = fs::read_to_string(root.join(format!("tests/rv32/m5/{name}.S"))).unwrap();
        assert!(source.contains(".option norelax"), "{name}.S");
    }
}

#[test]
fn the_frozen_platform_validates_and_the_builder_rejects_a_mismatched_kernel() {
    let (firmware, disk, _) = fixture();
    let build = |config: &Config| m5ref::build(&firmware, &disk, config).map(|_| ());
    let frozen = Config::frozen();
    assert_eq!(frozen.m5.slice, SLICE);
    assert_eq!(build(&frozen), Ok(()));
    let with = |f: &dyn Fn(&mut Config)| {
        let mut c = frozen.clone();
        f(&mut c);
        build(&c)
    };
    let moved = |w: Window| Window {
        base: w.base + 0x1000,
        size: w.size,
    };
    assert_eq!(
        with(&|c| c.m5.irqc = moved(c.m5.irqc)),
        Err(BuildError::IrqcMismatch(moved(frozen.m5.irqc)))
    );
    assert_eq!(
        with(&|c| c.m5.timer = moved(c.m5.timer)),
        Err(BuildError::TimerMismatch(moved(frozen.m5.timer)))
    );
    assert_eq!(
        with(&|c| std::mem::swap(&mut c.m5.blk_source, &mut c.m5.timer_source)),
        Err(BuildError::SourceMismatch {
            blk: TIMER_SOURCE,
            timer: BLK_SOURCE
        })
    );
    // An m3-reference invariant still holds.
    assert!(matches!(
        with(&|c| c.m3.controller_blocks += 1),
        Err(BuildError::M3(_))
    ));
}

#[test]
fn the_workload_passes_on_m5_reference_and_covers_the_kernel() {
    let (firmware, disk, expected) = fixture();
    let run = m5::run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
    judge(&run, &expected).unwrap();
    let mut lines: Vec<String> = String::from_utf8(run.output.clone().unwrap())
        .unwrap()
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect();
    lines.sort();
    assert_eq!(lines, expected_lines());
    Metrics::of(&run).coverage.check().unwrap();
    assert!(expected.contains(&format!("os.final word=0 value={}", expected_final()[0])));
}

#[test]
fn the_judge_refuses_a_disk_with_a_corrupted_data_block() {
    let (firmware, mut disk, _) = fixture();
    disk[DATA_LBA as usize * BLOCK_SIZE + 17] ^= 1;
    let run = m5::run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
    // The first reader finds the wrong byte and exits 1; the shutdown reports it.
    let err = judge_without_trace(&run).unwrap_err();
    assert!(err.starts_with("a1 = Some(1)"), "{err}");
    let os = os_trace(run.finished.trace.as_ref().unwrap());
    assert!(
        os.contains(
            "os.process.exit pid=3 status=1
"
        ),
        "{os}"
    );
}

#[test]
fn two_runs_are_identical_and_tracing_changes_nothing() {
    let traced = fresh(true);
    let again = fresh(true);
    assert_eq!(Metrics::of(&traced), Metrics::of(&again));
    assert_eq!(traced.snapshot, again.snapshot);
    let untraced = fresh(false);
    let (a, b) = (&traced.finished.outcome, &untraced.finished.outcome);
    assert_eq!(
        (a.events, a.instret, a.state, a.execution),
        (b.events, b.instret, b.state, b.execution)
    );
    assert_eq!(traced.snapshot, untraced.snapshot);
}

/// An empty script (`docs/m6-design.md` §7.2) runs the frozen schedule: only the state
/// digest, which covers the snapshot's format and script, differs.
#[test]
fn an_empty_script_runs_the_frozen_schedule() {
    let (firmware, disk, _) = fixture();
    let mut config = Config::frozen();
    config.m3.script = Some(systemscope_runtime::script::Script::default());
    let scripted = m5::run_with(
        &firmware,
        &disk,
        &config,
        Start::Init { traced: false },
        Vec::new(),
    );
    let frozen = fresh(false);
    let (a, b) = (&scripted.finished.outcome, &frozen.finished.outcome);
    assert_eq!(
        (a.events, a.instret, a.execution),
        (b.events, b.instret, b.execution)
    );
    assert_eq!(scripted.output, frozen.output);
    let states = |run: &M5Run| m2ref::components(run.snapshot.as_ref().unwrap()).unwrap();
    assert_eq!(states(&scripted), states(&frozen));
}

#[test]
fn a_checkpoint_resumes_to_the_same_end_and_trace() {
    let (firmware, disk, expected) = fixture();
    let reference = m5::run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
    let trace = reference.finished.trace.as_ref().unwrap();
    // A checkpoint while the readers wait and the counters are preempted.
    let k = 600_000usize;
    let mut rt = m5ref::build(&firmware, &disk, &Config::frozen()).unwrap();
    rt.init().unwrap();
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    let snapshot = rt.snapshot().unwrap();
    let before = trace
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.origin == TraceOrigin::Runtime)
        .nth(k)
        .map(|(at, _)| at)
        .unwrap();
    let prefix = systemscope_runtime::trace::Trace {
        header: trace.header.clone(),
        records: trace.records[..before].to_vec(),
    };
    let resumed = m5::run(
        &firmware,
        &disk,
        Start::Restore {
            snapshot,
            prefix: Some(prefix),
        },
        Vec::new(),
    );
    judge(&resumed, &expected).unwrap();
    assert_eq!(resumed.snapshot, reference.snapshot);
    let (a, b) = (&resumed.finished.outcome, &reference.finished.outcome);
    assert_eq!(
        (a.state, a.execution, a.trace),
        (b.state, b.execution, b.trace)
    );
    assert_eq!(
        os_trace(resumed.finished.trace.as_ref().unwrap()),
        os_trace(trace)
    );
}
