//! Workspace tasks, run as `cargo xtask <task>`.
//!
//! - `bless`: runs `m0-reference` for every fixed seed and rewrites the golden files in
//!   `tests/golden/`, printing what changed. Tests never write golden files; this is the
//!   only way they change, and the commit that changes them must say why in its body.
//! - `m1-golden bless`: runs every committed program on `m1-reference` and rewrites
//!   `tests/golden/m1-reference.json` and `tests/golden/m1-reference.mid.snap`, printing
//!   what changed. As with `bless`, this is the only way they change.
//! - `m1-golden verify`: reruns everything and requires the committed M1 golden files back
//!   byte for byte, then checks the portable snapshot. Writes nothing.
//! - `m1-golden emit <dir>`: writes this machine's M1 golden file and portable snapshot
//!   into `<dir>`, for another machine to `check`.
//! - `m1-golden check <dir>`: requires another machine's emitted files to equal the
//!   committed ones and this machine's, and restores and runs its snapshot here.
//! - `m2-golden bless | verify | emit <dir> | check <dir>`: the same for `block_irq.elf`
//!   on `m2-reference`, with `tests/golden/m2-reference.json` and
//!   `tests/golden/m2-reference.mid.snap`.
//! - `m3-golden bless | verify | emit <dir> | check <dir>`: the same for both M3 disks on
//!   `m3-reference`, with `tests/golden/m3-reference.json` and
//!   `tests/golden/m3-reference.mid.snap`. `verify` first verifies the M3 fixtures
//!   against their manifest, then reruns both scenarios and the portable snapshot.
//! - `m3-golden every-event [<from> <to> | shard <i> <n>]`: `docs/m3-design.md` §9.2's
//!   resume from every event of the reference scenario, of checkpoints `<from>..<to>`, or
//!   of shard `<i>` of `<n>` shards of about equal cost (for CI): each checkpoint
//!   is restored into a freshly built platform and run to the end, and must replay the
//!   uninterrupted run's events without reissuing anything and end in its state. Uses
//!   every available core; a release build is much faster.
//! - `m5-golden bless | verify | emit <dir> | check <dir>`: the same for the M5 disk on
//!   `m5-reference`, with `tests/golden/m5-reference.json` and
//!   `tests/golden/m5-reference.mid.snap`, the runtime snapshot with both readers
//!   blocked, which `verify` and `check` resume to the golden end.
//! - `m5-golden every-event [<from> <to> | shard <i> <n>]`: `docs/m5-design.md` §19's
//!   resume from every event of `m5-reference`, linear instead of M3's quadratic sweep:
//!   each checkpoint is restored into a freshly built platform, must snapshot back to
//!   itself, then dispatch the uninterrupted run's next event and reach its next
//!   checkpoint byte for byte. Uses every available core; a release build is much faster.
//! - `m5-golden every-event perturbed <a|b|c> [shard <i> <n>]`: the same on the platform
//!   perturbed by the mechanism's seed 0 (§19), which the weekly nightly runs for each mechanism.
//! - `m5-repro emit <dir> | check <dir> | replay <dir>`: as `m4-repro`, for the reproducer
//!   bundles of `m5-reference`'s seven minimized perturbations (`docs/m5-design.md` §18).
//! - `bench [<runs>]`: the simulator's throughput on the M3 disks and the M5 disk,
//!   untraced, as the median of `<runs>` runs (5 by default): events and retired
//!   instructions per second of wall-clock time. Report only, never a check; run it with
//!   `cargo run --release -p xtask -- bench`. The weekly nightly records it.
//! - `rv32-fixtures build`: Linux only, with the pinned toolchain on `PATH`. Rebuilds the
//!   40 `rv32ui` fixtures from the pinned `riscv-tests` with `tests/rv32/build-fixtures.sh`
//!   (the only step that uses the network), `hello.elf` with
//!   `tests/rv32/hello/build-hello.sh`, and `block_irq.elf` with
//!   `tests/rv32/block_irq/build-block-irq.sh`, the M3 firmware and user programs
//!   with `tests/rv32/m3/build-m3.sh`, and the M5 ones with `tests/rv32/m5/build-m5.sh`,
//!   then runs the `manifest` step. Tests never
//!   rebuild fixtures; the commit that changes them must say why in its body.
//! - `rv32-fixtures manifest [<cache-dir>]`: the second half of `build`, for a build run
//!   by hand. Checks the upstream checkout in the cache (default `target/rv32-fixtures`),
//!   rewrites `tests/rv32/fixtures/manifest.json` and `tests/rv32/hello/manifest.json`,
//!   writes the `block_irq.elf` disk fixture `tests/rv32/block_irq/disk.img` and rewrites
//!   `tests/rv32/block_irq/manifest.json`, runs the `m3-reference manifest` and
//!   `m5-reference manifest` steps, and verifies.
//! - `rv32-fixtures verify`: checks the committed fixtures, `hello.elf`, `block_irq.elf`,
//!   and its disk fixture, and the M3 and M5 fixtures against their manifests, with no
//!   network and no compiler.
//! - `m3-reference manifest`: the M3 half of `rv32-fixtures manifest`, which needs no
//!   compiler and no upstream checkout: writes the M3 disks `tests/rv32/m3/disk.img` and
//!   `disk-nofault.img` from the committed user ELFs, runs each on `m3-reference` and,
//!   if the run passes every other §12.3 check, writes its expected `os.*` trace, then
//!   rewrites `tests/rv32/m3/manifest.json` and verifies.
//! - `m3-reference run`: runs both M3 disks on `m3-reference`, judges each by §12.3, and
//!   prints the output and the metrics.
//! - `m3-reference verify`: `rv32-fixtures verify` for the M3 fixtures, then `run`, then
//!   a second run of each disk, whose digests must equal the first's.
//! - `m5-reference manifest | run | verify`: the same for the M5 disk
//!   `tests/rv32/m5/disk.img` on `m5-reference` (`docs/m5-design.md` §12), judged by
//!   `systemscope_rv32::m5::judge`.
//! - `corpus replay`: replays every entry of the regression corpus
//!   `tests/verification/corpus` (`docs/m4-design.md` §18) byte for byte.
//! - `corpus check <dir>`: whether the bundle in `<dir>` can be a corpus entry. It never
//!   writes into the corpus: promotion is a reviewed commit.
//! - `m4-mutants run <target> <out>`: the source mutants of one §25.2 target
//!   (`docs/m4-design.md`), each run by cargo-mutants against the source-mutant case,
//!   into `<out>/<target>`. The `source-mutants` workflow runs every target.
//! - `m4-mutants summarize <out>`: the counts and every mutant's fate; fails unless every
//!   mutant is detected or listed in `tests/verification/equivalent-mutants.txt`, and the
//!   locator equals the naive oracle for each.
//! - `m4-expected bless`: records every stream of the unmodified `m3-reference` into
//!   `tests/verification/expected/m3-reference` (`docs/m4-design.md` §25.2), the expected
//!   side of the source-mutant cases, then verifies.
//! - `m4-expected verify`: requires this build to reproduce those streams byte for byte.
//! - `m5-mutants run <target> <out> | summarize <out>`: the same for the M5 targets
//!   (`docs/m5-design.md` §21), each mutant run against M5's source-mutant case.
//! - `m5-expected bless | verify`: the same for `m5-reference`'s expectations in
//!   `tests/verification/expected/m5-reference`: its unperturbed streams and the digests
//!   of its perturbed runs. `bless` first checks that each race diverges, each minimized
//!   perturbation reproduces, the unpatched scenario agrees, and the tie seed reorders
//!   simultaneous events.
//! - `m5-coverage bless | verify [campaign]`: the coverage reports in
//!   `tests/verification/coverage` (`docs/m5-design.md` §17). Without `campaign`, the
//!   architectural coverage of `m3-reference` and `m5-reference`, two runs; with it, the
//!   `m5-reference` campaigns' schedule and perturbation coverage, 480 runs, which
//!   the weekly nightly verifies. `bless` writes, then verifies; `verify` requires this
//!   build to reproduce the committed report byte for byte.
//! - `m4-repro emit <dir>`: writes this machine's M4 reproducer bundles of
//!   `m3-reference` (`docs/m4-design.md` §16) into `<dir>`, one `<name>.repro` directory
//!   each, for another machine to `check`.
//! - `m4-repro check <dir>`: requires another machine's emitted bundles to equal this
//!   machine's byte for byte, and replays each here.
//! - `m4-repro replay <dir>`: validates and replays every `*.repro` bundle in `<dir>`.
//! - `spike build [<dir>]`: Linux only, with git, a C++ compiler, make, and dtc. Fetches
//!   and builds the pinned Spike into `<dir>` (default `target/spike`) with
//!   `tests/rv32/build-spike.sh`, then runs `verify`.
//! - `spike verify [<dir>]`: checks the Spike in `<dir>`: its stamp, its source checkout
//!   at the pin, its version line, and smoke runs of `simple` and `add` whose logs must
//!   equal the committed ones. Prints the binary's BLAKE3.
//! - `spike diff [<dir>]`: `verify`, then M1-A3: every selected fixture's retirements on
//!   SystemScope against Spike's, then the generated programs of the fixed seeds, then
//!   the misaligned-access programs, which must all match; then the directed M2 CSR and
//!   `MRET` programs with the M2 CPU profile against Spike with Zicsr, which must match
//!   too; then the directed M3 privilege and Sv32 programs with the M3 CPU profile, each
//!   up to its first exception taken in M. Every comparison runs through the M4 stream
//!   compare. Then SK-1 (`docs/m4-design.md` §25.1): its minimized reproducer must
//!   diverge as selected and replay with a second Spike run from
//!   `target/spike-logs/sk1.repro`. Spike's logs go to `target/spike-logs`, and the
//!   generated ELFs to `target/spike-logs/progen`, `privgen`, and `vmgen`.
//! - `spike random [<dir>]`: `verify`, then one generated program, for the seed in
//!   `M1_PROGEN_SEED` (decimal or `0x` hex), against Spike. The nightly workflow runs it.
//! - `spike campaign [<dir>]`: `verify`, then the Spike campaign (`docs/m4-design.md`
//!   §18): the generated programs of seeds 64 to 127 against Spike through the M4 stream
//!   compare. A divergence is minimized to `target/spike-logs/campaign/<program>.repro`,
//!   replayed, and fails the task.
//!
//! - `act4 build [<cache-dir>]`: Linux x86_64 only, with git, curl, tar, xz, make, and a
//!   host C compiler. Generates the ACT4 RV32I corpus with the pinned ACT4, Sail, and GCC
//!   in `/tmp/systemscope-act4` (`tests/act4/build-act4.sh`; the cache defaults to
//!   `target/act4/cache`) into `target/act4/out`, then runs `install target/act4/out`.
//!   Tests never regenerate the corpus; the commit that changes it must say why.
//! - `act4 install <out-dir>`: the second half of `build`, for a generation run by hand.
//!   Checks its record, replaces `tests/act4/fixtures/*.elf` with its ELFs, rewrites
//!   `tests/act4/manifest.json`, and verifies.
//! - `act4 check <out-dir>`: requires a generation to reproduce the committed corpus
//!   byte for byte: the same tests, identical ELFs, and the same manifest. Writes nothing.
//! - `act4 verify`: checks the committed corpus against its manifest, with no network,
//!   ACT4, Sail, or compiler.
//! - `act4 run`: `verify`, then M1-A4: runs every committed ACT4 ELF on `m1-reference`,
//!   all of which must pass; then runs them all again with the M2 and M3 CPU profiles.
//!
//! The `spike` tasks run `<dir>/bin/spike`, or the command in `SPIKE` if it is set: the
//! same pinned build, reached another way (such as through WSL). Nothing else in the
//! workspace needs Spike.

mod bench;
mod differential;
mod fixtures;
mod golden;
mod verification;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use systemscope_acceptance::corpus;
use systemscope_acceptance::golden::{GOLDEN_PATH, Golden, MID_SNAPSHOT_PATH, describe_changes};
use systemscope_acceptance::m1::golden as m1;
use systemscope_acceptance::m2::golden as m2;
use systemscope_acceptance::m3::golden as m3g;
use systemscope_acceptance::m3::{Fixture as M3Fixture, checkpoint as m3c};
use systemscope_acceptance::m4::{expected as m4x, mutants as m4m, repro as m4r};
use systemscope_acceptance::m5 as m5g;
use systemscope_acceptance::m5::campaign as m5c;
use systemscope_acceptance::m5::coverage as m5cov;
use systemscope_acceptance::m5::expected as m5x;
use systemscope_acceptance::m5::repro as m5r;
use systemscope_acceptance::m5::verify as m5v;
use systemscope_acceptance::sweep;
use systemscope_rv32::act4::{self, ACT4_MANIFEST, ACT4_SCRIPT};
use systemscope_rv32::block_irq::{
    self, BLOCK_IRQ_DIR, BLOCK_IRQ_MANIFEST, BLOCK_IRQ_SCRIPT, BlockIrqManifest,
};
use systemscope_rv32::hello::{self, HELLO_DIR, HELLO_MANIFEST, HELLO_SCRIPT, HelloManifest};
use systemscope_rv32::m3::{self, M3_DIR, M3_MANIFEST, M3_SCRIPT, M3Manifest, Metrics, Scenario};
use systemscope_rv32::m5::{self, M5_DIR, M5_MANIFEST, M5_SCRIPT, M5Manifest};
use systemscope_rv32::manifest::{Manifest, verify};
use systemscope_rv32::progen::{self, FIXED_SEEDS, MISALIGNED};
use systemscope_rv32::runner::Start;
use systemscope_rv32::spike::{
    self, DiffReport, SPIKE_COMMIT, SPIKE_DIR, SPIKE_ISA_M2, SPIKE_LOGS, SPIKE_PRIV_M3,
    SPIKE_SCRIPT,
};
use systemscope_rv32::{
    BUILD_SCRIPT, FIXTURE_DIR, MANIFEST_PATH, Rv32iProfile, SELECTED, upstream,
};
use systemscope_rv32::{csrgen, privgen, spike_adapter, vmgen};

use bench::*;
use differential::*;
use fixtures::*;
use golden::*;
use verification::*;

const USAGE: &str = "usage: cargo xtask bless\n       \
                     cargo xtask m1-golden bless | verify | emit <dir> | check <dir>\n       \
                     cargo xtask m2-golden bless | verify | emit <dir> | check <dir>\n       \
                     cargo xtask m3-golden bless | verify | emit <dir> | check <dir> \
                     | every-event [<from> <to> | shard <i> <n>]\n       \
                     cargo xtask m3-reference manifest | run | verify\n       \
                     cargo xtask m5-golden bless | verify | emit <dir> | check <dir> \
                     | every-event [<from> <to> | [perturbed <a|b|c>] [shard <i> <n>]]\n       \
                     cargo xtask m5-reference manifest | run | verify\n       \
                     cargo xtask corpus replay | check <dir>\n       \
                     cargo xtask m4-mutants run <target> <out> | summarize <out>\n       \
                     cargo xtask m4-expected bless | verify\n       \
                     cargo xtask m5-mutants run <target> <out> | summarize <out>\n       \
                     cargo xtask m5-expected bless | verify\n       \
                     cargo xtask m5-coverage bless | verify [campaign]\n       \
                     cargo xtask m4-repro emit <dir> | check <dir> | replay <dir>\n       \
                     cargo xtask m5-repro emit <dir> | check <dir> | replay <dir>\n       \
                     cargo xtask bench [<runs>]\n       \
                     cargo xtask rv32-fixtures build | manifest [<cache-dir>] | verify\n       \
                     cargo xtask spike build | verify | diff | random | campaign [<dir>]\n       \
                     cargo xtask act4 build [<cache-dir>] | install <out-dir> | check <out-dir> \
                     | verify | run";
/// The golden file's name in an `m1-golden emit` directory.
const M1_RESULT: &str = "m1-reference.json";
/// The golden file's name in an `m2-golden emit` directory.
const M2_RESULT: &str = "m2-reference.json";
/// The golden file's name in an `m3-golden emit` directory.
const M3_RESULT: &str = "m3-reference.json";
/// The golden file's name in an `m5-golden emit` directory.
const M5_RESULT: &str = "m5-reference.json";
/// Where `rv32-fixtures build` fetches and builds, relative to the workspace root.
const RV32_CACHE: &str = "target/rv32-fixtures";
/// Where `act4 build` keeps the pinned stack, relative to the workspace root.
const ACT4_CACHE: &str = "target/act4/cache";
/// Where `act4 build` generates the corpus, relative to the workspace root.
const ACT4_OUT: &str = "target/act4/out";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["bless"] => bless(),
        ["m1-golden", "bless"] => m1_bless(),
        ["m1-golden", "verify"] => m1_verify(),
        ["m1-golden", "emit", dir] => m1_emit(Path::new(dir)),
        ["m1-golden", "check", dir] => m1_check(Path::new(dir)),
        ["m2-golden", "bless"] => m2_bless(),
        ["m2-golden", "verify"] => m2_verify(),
        ["m2-golden", "emit", dir] => m2_emit(Path::new(dir)),
        ["m2-golden", "check", dir] => m2_check(Path::new(dir)),
        ["m3-golden", "bless"] => m3_bless(),
        ["m3-golden", "verify"] => m3_golden_verify(),
        ["m3-golden", "emit", dir] => m3_emit(Path::new(dir)),
        ["m3-golden", "check", dir] => m3_check(Path::new(dir)),
        ["m3-golden", "every-event"] => m3_every_event(Checkpoints::All),
        ["m3-golden", "every-event", "shard", i, n] => match (i.parse(), n.parse()) {
            (Ok(i), Ok(n)) if i < n => m3_every_event(Checkpoints::Shard(i, n)),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        ["m3-golden", "every-event", from, to] => match (from.parse(), to.parse()) {
            (Ok(from), Ok(to)) => m3_every_event(Checkpoints::Range(from..to)),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        ["rv32-fixtures", "build"] => rv32_build(),
        ["rv32-fixtures", "manifest"] => rv32_manifest(&root().join(RV32_CACHE)),
        ["rv32-fixtures", "manifest", cache] => rv32_manifest(Path::new(cache)),
        ["rv32-fixtures", "verify"] => rv32_verify(),
        ["m3-reference", "manifest"] => m3_manifest(),
        ["m3-reference", "run"] => m3_run(),
        ["m3-reference", "verify"] => m3_verify(),
        ["m5-reference", "manifest"] => m5_manifest(),
        ["m5-reference", "run"] => m5_run(),
        ["m5-reference", "verify"] => m5_verify(),
        ["m5-golden", "bless"] => m5_bless(),
        ["m5-golden", "verify"] => m5_golden_verify(),
        ["m5-golden", "emit", dir] => m5_emit(Path::new(dir)),
        ["m5-golden", "check", dir] => m5_check(Path::new(dir)),
        ["m5-golden", "every-event"] => m5_every_event(None, Checkpoints::All),
        ["m5-golden", "every-event", "perturbed", m] => match mechanism(m) {
            Some(m) => m5_every_event(Some(m), Checkpoints::All),
            None => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        ["m5-golden", "every-event", "perturbed", m, "shard", i, n] => {
            match (mechanism(m), i.parse(), n.parse()) {
                (Some(m), Ok(i), Ok(n)) if i < n => {
                    m5_every_event(Some(m), Checkpoints::Shard(i, n))
                }
                _ => {
                    eprintln!("{USAGE}");
                    ExitCode::FAILURE
                }
            }
        }
        ["m5-golden", "every-event", "shard", i, n] => match (i.parse(), n.parse()) {
            (Ok(i), Ok(n)) if i < n => m5_every_event(None, Checkpoints::Shard(i, n)),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        ["m5-golden", "every-event", from, to] => match (from.parse(), to.parse()) {
            (Ok(from), Ok(to)) => m5_every_event(None, Checkpoints::Range(from..to)),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        ["corpus", "replay"] => corpus_replay(),
        ["corpus", "check", dir] => corpus_check(Path::new(dir)),
        ["m4-mutants", "run", target, out] => mutants_run(&m4m::TARGETS, target, out),
        ["m4-mutants", "summarize", dir] => mutants_summarize(&m4m::TARGETS, Path::new(dir)),
        ["m4-expected", "bless"] => m4_expected_bless(),
        ["m4-expected", "verify"] => m4_expected_verify(),
        ["m5-mutants", "run", target, out] => mutants_run(&m4m::M5_TARGETS, target, out),
        ["m5-mutants", "summarize", dir] => mutants_summarize(&m4m::M5_TARGETS, Path::new(dir)),
        ["m5-expected", "bless"] => m5_expected(true),
        ["m5-expected", "verify"] => m5_expected(false),
        ["m5-coverage", "bless"] => m5_coverage(true, m5cov::Reports::Arch),
        ["m5-coverage", "verify"] => m5_coverage(false, m5cov::Reports::Arch),
        ["m5-coverage", "bless", "campaign"] => m5_coverage(true, m5cov::Reports::Campaign),
        ["m5-coverage", "verify", "campaign"] => m5_coverage(false, m5cov::Reports::Campaign),
        ["m4-repro", "emit", dir] => m4_emit(Path::new(dir)),
        ["m4-repro", "check", dir] => m4_check(Path::new(dir)),
        ["m4-repro", "replay", dir] => m4_replay(Path::new(dir)),
        ["m5-repro", "emit", dir] => m5_repro_emit(Path::new(dir)),
        ["m5-repro", "check", dir] => m5_repro_check(Path::new(dir)),
        ["m5-repro", "replay", dir] => m5_repro_replay(Path::new(dir)),
        ["bench"] => bench(5),
        ["bench", runs] => match runs.parse() {
            Ok(runs) if runs > 0 => bench(runs),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        ["act4", "build"] => act4_build(&root().join(ACT4_CACHE)),
        ["act4", "build", cache] => act4_build(Path::new(cache)),
        ["act4", "install", out] => act4_install(Path::new(out)),
        ["act4", "check", out] => act4_check(Path::new(out)),
        ["act4", "verify"] => act4_verify(),
        ["act4", "run"] => act4_run(),
        ["spike", task] => spike_task(task, &root().join(SPIKE_DIR)),
        ["spike", task, dir] => spike_task(task, Path::new(dir)),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace root")
        .to_path_buf()
}

fn report(result: Result<(), Vec<String>>, ok: &str) -> ExitCode {
    match result {
        Ok(()) => {
            println!("{ok}");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for e in errors {
                eprintln!("{e}");
            }
            ExitCode::FAILURE
        }
    }
}
