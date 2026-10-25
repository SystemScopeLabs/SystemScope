# Contributing

## Checks

CI runs these on every push and pull request (`.github/workflows/ci.yml`):

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo machete
cargo nextest run --workspace --locked --no-fail-fast
cargo test --workspace --doc --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo deny --locked check
```

`cargo deny` checks every dependency against the RustSec advisories, the licenses allowed in `deny.toml`, and its sources. nextest reports a test as slow after a minute and kills it after ten (`.config/nextest.toml`). Every workflow job sets up through `.github/actions/setup`, which holds the `contracts` pin.

The acceptance tests (`tests/acceptance`, `docs/m0-design.md` §9) run the full `m0-reference`, so they take about a minute. Run one alone with `cargo nextest run -p systemscope-acceptance --test at1` (or `at2`, `at3`, `compile_fail`).

The nightly random-seed tests are ignored by default. To reproduce a nightly failure, pass its seed:

```sh
M0_SEED=0x1234 cargo nextest run -p systemscope-acceptance --run-ignored only
```

The same night's seed is also `M1_PROGEN_SEED` (the generated RV32I program), `M4_FAULT_SEED` (the register fault the M4 locator and the naive oracle must find alike), and `M5_CAMPAIGN_SEED` (the perturbation seed under which the unpatched `m5-reference` scenario must agree). `.github/workflows/nightly.yml` prints all four. The offline minimization search, `m5_minimize`'s `search`, is ignored too but is not a seeded test; the nightly skips it.

## Golden Files

`tests/golden/` holds the M0 golden digests for the fixed seeds and a portable mid-run snapshot. Tests only read these files.

- Change them only with `cargo xtask bless`. It reruns the scenario, prints each changed digest as old → new, and rewrites the files.
- Never bless to make a failing test pass without knowing why the digests moved. A digest changes only when simulated behavior, an encoding, or the workload changes.
- Commit golden changes on their own, and say in the commit body what changed and why the new digests are right.

## M1 Golden Files

`tests/golden/m1-reference.json` and `tests/golden/m1-reference.mid.snap` hold the M1 results for `hello.elf` and the 40 `rv32ui` tests, and the portable snapshot (`docs/m1-design.md` §10.1). They are separate from the M0 golden files, and tests and CI only read them.

- `cargo xtask m1-golden verify` reruns every program and checks both files. CI runs it on both operating systems. The tests are `cargo nextest run -p systemscope-acceptance --test m1_a6` (or `m1_a7`, `m1_a8`).
- `cargo xtask m1-golden emit <dir>` writes this machine's result and snapshot to `<dir>`, and `cargo xtask m1-golden check <dir>` checks another machine's against the committed files and a local run, and restores its snapshot. CI uses them to compare Linux and Windows.
- **Do not run `cargo xtask m1-golden bless` to make a failing M1 test pass.** It rewrites both files from the current run. A result changes only when the CPU, the platform, an encoding, or a committed ELF changes on purpose; otherwise a failure is a bug to fix. Bless only then, commit the files on their own, and say in the commit body what changed and why the new values are right.

## M2 and M3 Golden Files

`tests/golden/m2-reference.{json,mid.snap}` hold the `block_irq.elf` result on `m2-reference` and its portable snapshot (`docs/m2-design.md` §15). `tests/golden/m3-reference.{json,mid.snap}` hold the results of both M3 disks on `m3-reference` and the portable snapshot (`docs/m3-design.md` §17.4). The same rules as the M1 golden files apply, and tests and CI only read them.

- `cargo xtask m2-golden verify` and `cargo xtask m3-golden verify` rerun the scenarios and check both files; `m3-golden verify` first checks the M3 fixtures against their manifest. `emit <dir>` and `check <dir>` compare two machines, and CI uses them between Linux and Windows. The tests are `m2_snapshot`, `m2_observation`, and `m2_golden`, and `m3_snapshot`, `m3_observation`, and `m3_golden`, in `systemscope-acceptance`.
- `cargo xtask m3-golden every-event` restores every one of the reference scenario's checkpoints, from the initial state to the final one, into a fresh platform and runs it to the end (`docs/m3-design.md` §9.2). A release build is much faster. `every-event shard <i> <n>` runs one of `n` shards of about equal cost, and CI runs eight.
- **Do not run `m2-golden bless` or `m3-golden bless` to make a failing test pass**, and never re-bless the M0, M1, or M2 golden files for an M3 change: they must stay byte-identical to their releases. CI never blesses.

## M4 Expected Streams and Regression Corpus

`tests/verification/expected/m3-reference/` holds every stream of the unmodified `m3-reference` run (`arch.obs`, `effect.obs`, `os.obs`), the expected side of the source-mutant cases (`docs/m4-design.md` §25.2). `cargo xtask m4-expected verify` and the `m4_expected` tests require this build to reproduce them byte for byte. The rules of the golden files apply: `cargo xtask m4-expected bless` only for an intended change to `m3-reference`'s observations, committed on its own.

`tests/verification/corpus/` is the regression corpus (`docs/m4-design.md` §18): one canonical directory bundle per entry, each `Diverged` or `Agree`. `cargo xtask corpus replay` and the `m4_corpus` tests replay every entry, and the recomputed `result.bin` must equal the committed one byte for byte. No tool writes into the corpus. To promote a reproducer:

1. Write it into a scratch directory, for example with `cargo xtask m4-repro emit target/m4-repro`.
2. Understand the divergence, and decide that it is intended as a regression case.
3. Run `cargo xtask corpus check <dir>`, which validates and replays it and refuses any other outcome or a profile the corpus cannot replay.
4. Copy the directory to `tests/verification/corpus/<name>/` unchanged, add `<name>` to the `m4_corpus` test, and commit it in a normal PR, saying in the body what the entry asserts.

Never edit an entry by hand. When an intended change alters an entry's result, regenerate it, re-review it, and commit it on its own.

## M5 Golden Files, Expectations, and Coverage

`tests/golden/m5-reference.{json,mid.snap}` hold the M5 disk's result on `m5-reference` and the runtime snapshot with both readers blocked (`docs/m5-design.md` §12, §19). The rules of the M1 to M3 golden files apply, and the earlier golden files must stay byte-identical for an M5 change.

- `cargo xtask m5-golden verify` reruns the scenario and resumes the snapshot to the golden end; `emit <dir>` and `check <dir>` compare two machines, and CI uses them between Linux and Windows.
- `cargo xtask m5-golden every-event [shard <i> <n>]` resumes from every event of `m5-reference` (§19). `every-event perturbed <a|b|c>` does the same on the platform perturbed by that mechanism's seed 0; the weekly nightly runs all three.
- `cargo xtask m5-reference manifest | run | verify` rewrites, runs, and checks the M5 disk `tests/rv32/m5/disk.img`, judged by `systemscope_rv32::m5::judge`. `rv32-fixtures build` rebuilds the M5 firmware and user programs with `tests/rv32/m5/build-m5.sh`.
- `tests/verification/expected/m5-reference/` holds the unperturbed streams and the digests of the perturbed runs. `cargo xtask m5-expected verify` requires this build to reproduce them. `m5-expected bless` first checks that each planted race diverges, each minimized perturbation reproduces, the unpatched scenario agrees, and the tie seed reorders simultaneous events; bless only for an intended change, committed on its own.
- `tests/verification/coverage/` holds the coverage reports (§17). `cargo xtask m5-coverage verify` checks the architectural coverage of `m3-reference` and `m5-reference`; `m5-coverage verify campaign` checks the campaigns' schedule and perturbation coverage, 480 runs, which the weekly nightly runs on both operating systems. `bless` follows the same rules.
- `cargo xtask m5-repro emit <dir> | check <dir> | replay <dir>` writes, compares, and replays the reproducer bundles of the seven minimized perturbations (§18). CI emits them on both operating systems and checks each against the other.
- `cargo xtask m5-mutants run <target> <out> | summarize <out>` runs the M5 source mutants (§21). A surviving mutant is either detected by a new test or listed, with its reason, in `tests/verification/equivalent-mutants.txt`.

## Robustness and Throughput

`tests/acceptance/tests/robustness.rs` feeds corrupted and arbitrary bytes to every decoder that reads outside input: the `m0-reference` and `m5-reference` snapshots, `result.bin`, observation evidence, `perturbation.bin`, and the bundle manifest. They must be refused with an error, never a panic, and anything that decodes must be the canonical encoding, so a corrupted snapshot that restores snapshots back to the same bytes. CI runs them on both operating systems. A new decoder of outside bytes gets a case there.

`cargo run --release -p xtask -- bench [<runs>]` prints the wall-clock throughput of the M3 disks and the M5 disk, untraced: events and retired instructions per second, the median of `<runs>` runs. It is a report, never a check; the weekly nightly records it in its summary.

## rv32 Fixtures

`tests/rv32/fixtures/` holds the 40 `rv32ui` ELFs built from the pinned `riscv-tests`, and `manifest.json`, which records their hashes and the pins (`docs/m1-design.md` §10.2, §10.6). Tests and CI only read them, so Linux and Windows run the same bytes.

- `cargo xtask rv32-fixtures verify` checks the fixtures against the manifest, with no network or compiler. CI runs it on both operating systems.
- `cargo xtask rv32-fixtures build` rebuilds them on Linux with the pinned toolchain on `PATH` (the Ubuntu 24.04 packages the manifest names) and rewrites the manifest. Rebuild only on purpose, for a new pin or environment change, and say why in the commit body. A CI job rebuilds on a clean machine and fails on any byte difference.
- Adding or removing a test is a change to the selection in `tests/rv32/src/lib.rs` and the manifest, reviewed like a golden change.
- `build` also rebuilds `hello.elf`, `block_irq.elf` and its disk, and the M3 firmware and five user programs (`tests/rv32/m3/build-m3.sh`), each pinned by its own manifest. The M3 user programs must stay `gp`-independent: the script and `the_user_programs_are_gp_independent_rv32i_executables` check it.
- `cargo xtask m3-reference manifest` rewrites the M3 disks and expected `os.*` traces from the committed ELFs, with no compiler. `cargo xtask m3-reference run` runs both disks and judges them by `docs/m3-design.md` §12.3, and `m3-reference verify` also checks the fixtures and that two runs are identical.
- `elf/tests/fixtures/user/user.elf`, the M3.1 parser fixture, is built by hand with `elf/tests/fixtures/user/build-user.sh`, not by `rv32-fixtures build`. `elf/tests/user_fixture.rs` checks it against its manifest.

## Spike Differential

M1-A3 compares every selected `rv32ui` fixture, the generated programs for 64 fixed seeds, and 9 misaligned-access programs, retirement by retirement, with Spike at the commit pinned in `tests/rv32/build-spike.sh` (`docs/m1-design.md` §10.3). The same task then runs the directed M2 CSR programs with the M2 CPU profile (`docs/m2-design.md` §4), the directed M3 privilege and Sv32 programs with the M3 CPU profile (`docs/m3-design.md` §5, [m3-2-spike-appendix.md](docs/m3-2-spike-appendix.md), [m3-3-spike-appendix.md](docs/m3-3-spike-appendix.md)), and the directed M5 SEI programs with the M5 CPU profile (`docs/m5-design.md`). The known divergences from Spike are listed in appendix B.6 and are kept out of the comparison, never special-cased in it. Only these tasks need Spike; the normal test suite runs without it, on committed Spike logs in `tests/rv32/spike/`.

- `cargo xtask spike build [<dir>]` fetches and builds the pinned Spike into `<dir>` (default `target/spike`) on Linux, with git, a C++ compiler, make, and `dtc`, then verifies it.
- `cargo xtask spike verify [<dir>]` checks the build's stamp, its clean checkout at the pin, and its version line, and requires it to write the committed `simple` and seed 0 logs again, and the committed start of the `lw-1` trap log. `cargo xtask spike diff [<dir>]` verifies, then runs all 40 fixtures, 64 generated programs, and 9 misaligned-access programs on both sides, then the M2 CSR, M3 privilege, M3 Sv32, and M5 SEI programs; all must match, and every misaligned access must trap on both. CI runs both in a Linux job.
- `M1_PROGEN_SEED=<seed> cargo xtask spike random [<dir>]` verifies, then runs the generated program for one seed; the nightly job runs it with the night's seed, and `M1_PROGEN_SEED=<seed> cargo nextest run -p systemscope-rv32 --run-ignored only` runs that program on SystemScope alone.
- The generator (`tests/rv32/src/progen.rs`) is pinned by a digest of the fixed seeds' programs. Changing what a seed produces needs a new generator context, and the committed seed 0 log comes again from the pinned Spike, never by hand.
- `SPIKE`, if set, is the command the tasks run instead of `<dir>/bin/spike`: the same pinned build, reached another way.
- A mismatch is a bug in SystemScope or a misread of Spike's log, never a case to special-case or skip. Do not edit the committed Spike logs by hand; they come from the pinned Spike.

## ACT4 Corpus

M1-A4 runs the ACT4 RV32I tests, self-checking ELFs whose expected values come from the Sail reference model (`docs/m1-design.md` §10.4). The ELFs and `tests/act4/manifest.json` are committed; only generating them needs ACT4, Sail, and the RISC-V GCC.

- `cargo xtask act4 verify` checks the ELFs against the manifest, the pins, and the configuration hashes, and `cargo xtask act4 run` runs them all on `m1-reference`, then again with the M2 and M3 CPU profiles. Neither needs a network or an external tool. CI runs both on both operating systems.
- `cargo xtask act4 build [<cache-dir>]` regenerates the corpus on Linux x86_64 with `tests/act4/build-act4.sh`, which fetches and checks the pinned stack. Regenerate only on purpose, for a new pin or configuration, and say why in the commit body. A CI job generates twice on a clean machine and fails on any byte difference from the other generation or from the committed files.
- The ACT4 corpus covers RV32I only. M3 implements the privileged subset documented in `docs/m3-design.md` §5, which is checked against Spike, not against ACT4. Sm appears in the UDB adapter configuration only because the pinned ACT4/UDB schema needs it to express MXLEN=32; it does not describe a CPU capability, and privileged tests stay off. Do not describe SystemScope as conforming to Sm, Ss, or Sv32.
- A failing ACT4 test is a bug to diagnose: keep the ELF, check Sail's signature and results and the disassembly, then Spike if needed; add a regression test, then fix it. Fix configuration bugs in the configuration, never in the CPU. Do not patch ACT4, bypass UDB validation, or add a workaround without discussing it first.
