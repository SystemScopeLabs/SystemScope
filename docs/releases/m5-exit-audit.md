# M5 exit audit (M5.8)

This audit checks M5, `docs/m5-design.md`, against its exit criteria (§26). It covers
function, compatibility, determinism, validation, and mutation evidence, and decides whether
M5 can close as a release candidate. Besides documentation, it adds one test:
`m5_evidence::every_finding_reports_its_first_differing_decision`. §26's **Evidence**
criterion names each finding, and M5.4b's test covered one perturbed run of the unpatched
scenario (§8). The test needs `m5::evidence::traced_subject`, which traces a planted race's
run like `traced`. Two doc comments in `tests/acceptance/src/m5/mod.rs` no longer link to
private items, so `cargo doc` passes with `-D warnings` (§12).

No schema, contract, fixture, golden file, or expected stream changed, and nothing was
re-blessed. The other changes bring the design's status, O2, `plan.md`, and `README.md` up
to date (§13).

This audit does not create a tag, a release, a version bump, or a changelog entry
(m5-design §25, M5.8: "Exit audit", docs).

- **Base:** `main` at `e653fa9` (the M5.7b merge, PR #68).
- **M5 range:** `17df769..e653fa9` (the M4 exit audit merge to the M5.7b merge), PRs #55–#68:

  | Step | PR | Merge |
  |---|---|---|
  | M5.0 design | #55 | `db521e4` |
  | M5.1 | #56 | `3f17c42` |
  | M5.2 | #57 | `bf05085` |
  | M5.3a | #58 | `78a3f8d` |
  | M5.3b | #59 | `36366ca` |
  | M5.4a | #60 | `c0f695b` |
  | CI: every acceptance and rv32 test target | #61 | `bbc562e` |
  | M5.4b | #62 | `a037d04` |
  | M5.5 | #63 | `4259f19` |
  | M5.6a | #64 | `1fd96d8` |
  | M5.6b | #65 | `ea8ee4c` |
  | M5.6c | #66 | `ceda5d2` |
  | M5.7a | #67 | `d2fbc7d` |
  | M5.7b | #68 | `e653fa9` |

- **Contracts pin:** `SystemScopeLabs/contracts` at `90900a128d51f6b42e86dc789447882c90c52ef4`
  throughout the range.
- **Toolchain:** `rust-toolchain.toml`.
- **Local runs:** Windows 11 x86-64.
- **CI:** the PR #67 runs at `d2fbc7d`: CI 40092595137, Source mutants 40092595129, and M5
  source mutants 40092595113, all successful. The PR #68 runs at `e653fa9`: CI 40095128365 and M5 source mutants 40095128396, both successful. After a
  fast-forward merge, `main` holds the same tree, so its run is not repeated.

## 1. Scope

m5-design §1 defines M5 as timing exploration. Timing becomes a seeded, recorded input (D1).
Campaigns perturb the concurrent system, the CPU's interrupts, the timer, the block device,
and the kernel's scheduling, and compare each run with the unperturbed one through the M4
engine on timing-invariant streams (D7). Each finding is located, explained by schedule
evidence, minimized to the perturbation it needs, and reproduced.

## 2. Milestone inventory

| Step | Delivered | Tested | Documented |
|---|---|---|---|
| M5.1 | Perturbation core: A in the builders, B in the runtime (`runtime/src/jitter.rs`), identity, snapshot state | `tests/rv32/tests/perturbation.rs`, `runtime/tests/*` | §27.1 |
| M5.2 | `M5` CPU profile (`rv32i/src/sei.rs`), `SimpleTimer` (`platform/src/timer.rs`), Spike-directed SEI tests | `rv32i/tests/{cpu_m5,cpu_timer_sei_runtime}.rs`, `platform/tests/timer.rs`, `tests/rv32/tests/spike_m5.rs`, the Linux `spike` job | §27.2 |
| M5.3a | Kernel: interrupt traps, preemption, `blk_read`, idle, shared page, shutdown dump (`os/src/interrupts.rs`) | `os/tests/m5_kernel.rs` | §27.3 |
| M5.3b | `m5-reference`, its firmware and six programs, golden files, `SLICE` | `tests/acceptance/tests/m5_golden.rs`, `cargo xtask m5-reference verify`, the M5 every-event and cross-OS jobs | §27.4 |
| M5.4a | `outcome` and `schedule` streams, the `m5.*` profiles, projection state in checkpoints, a perturbed backend | `tests/acceptance/tests/m5_verify.rs` | §27.5 |
| M5.4b | Schedule evidence, perturbation identity, the RAM state decoder | `tests/acceptance/tests/m5_evidence.rs` | §27.6 |
| M5.5 | Planted races R1–R3 and R5; campaigns over A and B | `tests/acceptance/tests/m5_campaign.rs` | §27.7 |
| M5.6a | Tie order C: rank-major sequences (`runtime/src/ties.rs`), snapshot version 3 | `runtime/tests/scheduler_props.rs`, `tests/rv32/tests/perturbation.rs`, `m5_campaign.rs` | §27.8 |
| M5.6b | Perturbation minimization, the listed jitter, `perturbation.bin` in reproducers | `tests/acceptance/tests/m5_reproducer.rs`, `verification/tests/bundle.rs` | §27.9 |
| M5.6c | Coverage reports; seeds and ranges frozen (O5) | `cargo xtask m5-coverage verify [campaign]` | §27.10 |
| M5.7a | Source mutants against committed expectations | `tests/acceptance/tests/m5_expected.rs`, the `m5-source-mutants` workflow | §27.11 |
| M5.7b | Reproducers cross-OS, the nightly campaign seed, perturbed every-event resume | `cargo xtask m5-repro`, the M5 cross-OS jobs, `m5_campaign::nightly_campaign_seed`, `m5_golden::perturbed_checkpoints_resume_exactly` | §27.12 |

## 3. Non-goals (m5-design §2.2)

None was implemented:
- no change to M0–M4 behavior (§5);
- no multiple harts, caches, TLB, atomics, or memory-model relaxation;
- no CLINT, `mtime`/`mtimecmp`, Sstc, or machine timer interrupt: the timer is a device on
  the interrupt controller (D4);
- no new comparison engine: M5 adds profiles, inputs, and evidence, and
  `systemscope-verify`'s changes are projection state in checkpoints and `perturbation.bin`
  in bundles (§4);
- no RTL, native kernel, or UI;
- no unbounded search: every campaign is an explicit seed list, the nightly seed is printed,
  and the minimization search runs offline from pinned seeds.

## 4. Architecture boundaries

- `systemscope-verify` still depends on exactly `systemscope-contracts`,
  `systemscope-runtime`, `blake3`, and `serde_json`. Platform knowledge enters through the
  `m5.*` projections in `tests/acceptance`.
- `systemscope-runtime` depends on `systemscope-contracts` and `blake3` only. B and C are
  runtime-internal (§22): neither changes `EventKey` nor a contracts type (D10).
- A is a builder input (`Config`), so components never see a perturbation.

## 5. Compatibility

- **Golden files.** `git diff v0.5.0-m4..main -- tests/golden` adds only
  `m5-reference.json` and `m5-reference.mid.snap`. Every M0–M4 golden file and mid-run
  snapshot is byte-identical to the M4 release.
- **Expected streams and corpus.** The only additions are under
  `tests/verification/expected/m5-reference`. M4's expected streams and corpus are unchanged.
- **Fixtures.** The only additions are under `tests/rv32/m5`. No existing fixture changed.
  The only modified data file is `tests/verification/equivalent-mutants.txt`, which gains an
  M5 section.
- **Identity.** With the empty perturbation every runtime path is the M0–M4 path (§19).
  Every M0–M4 golden is verified in CI on both operating systems, and
  `the_identity_override_keeps_the_frozen_schedule` checks that an override equal to a
  frozen latency changes nothing.
- **Contracts.** The pin is unchanged, and M5 adds no protocol (§22).

## 6. Determinism and cross-OS

- **Perturbed runs and campaign results.** `m5_expected::this_build_reproduces_the_expectations`
  runs on both operating systems in CI. It requires the BLAKE3 of every stream of 44
  campaign runs to equal the committed ones: the unpatched scenario and each race
  unperturbed, each race's first diverging seeds, the seven minimized perturbations, and
  the unpatched scenario at every CI seed of A, B, and C. The coverage campaign, 480 runs,
  must reproduce its committed report on both operating systems weekly.
- **Golden and snapshot.** `M5 cross-OS`: each OS resumes the other's mid snapshot to the
  end and checks its golden result.
- **Reproducers.** `M5 cross-OS`: each OS emits the seven minimized reproducers with
  `m5-repro emit`, and the other requires them byte for byte and replays them to the result
  they hold (run 40095128365).
- **Every-event resume.** The frozen platform resumes from every checkpoint in the eight
  `M5 every-event resume` shards. The platforms perturbed by each mechanism's seed 0 resume
  from every checkpoint weekly (`m5-perturbed-every-event`): locally, A, B, and C each
  resumed about 1.19 million checkpoints with no failure. `perturbed_checkpoints_resume_exactly`
  checks the first and last 500 under A and B in CI.

## 7. External validation

- **Spike** (pinned `19609434`), in the Linux `spike` job of run 40092595137: `m5-sei`
  selected 20 programs, the seven directed SEI programs and the thirteen M3.2 programs again,
  all 20 matched, 797 retirements compared, interrupts taken in S and in M included. Each
  comparison also agrees through the M4 stream compare. The M0–M4 Spike suites pass as in M4.
- **ACT4/Sail** and the rv32ui fixtures pass as in M4.
- The timer and the interrupt controller are SystemScope devices that Spike does not model
  (§21), so they are not compared with Spike.

## 8. Findings and evidence

| Race | First diverging seed under A | Under B | Minimized under A | Under B |
|---|---|---|---|---|
| R1 `counter-unlocked` | 0 | 0 | link 12, slice | 5 delays |
| R2 `peterson-turn` | 1 | 0 | links 4 and 11, slice | 42 delays |
| R3 `mailbox-data-first` | 0 | 1 | link 11 | not minimized (§16) |
| R5 `kernel-wake-lost` | 1 | 0 | slice | 1 delay |

Every earlier seed agrees (`m5_campaign::*_diverges_under_*`). The unpatched scenario
agrees for seeds 0–7 of A, B, and C, and, in the coverage campaign, for seeds 0–31 of each.

**Evidence for each finding.** M5.4b's test checked the evidence on one perturbed run of
the unpatched scenario, whose `outcome` agrees. §26 requires it of each finding, so this
audit adds `every_finding_reports_its_first_differing_decision`. For each of the eight
findings above, it requires:
- `outcome` to diverge;
- the evidence to name a first differing decision, at the engine's `schedule` divergence;
- the perturbation elements before it: A's overrides, or B's jittered deliveries up to that
  decision's event.

It passes in about 50 s and runs in CI's M5.4b step.

## 9. Mutation inventory

**M5 source mutants** (§21, §27.11), M5 source mutants run 40092595113:

| Target | Class | Generated | Diverged | Auxiliary | Failed | Perturbed | Crashed | Hung | Unviable | Missed |
|---|---|---|---|---|---|---|---|---|---|---|
| jitter | jitter draw | 7 | 0 | 0 | 0 | 6 | 0 | 0 | 1 | 0 |
| send | jitter application | 4 | 0 | 0 | 2 | 1 | 1 | 0 | 0 | 0 |
| ties | tie rank | 9 | 0 | 0 | 0 | 4 | 3 | 0 | 0 | 2 |
| sequence | tied sequence | 20 | 0 | 0 | 9 | 8 | 0 | 0 | 1 | 2 |
| preempt | preemption decision | 16 | 7 | 0 | 5 | 0 | 0 | 0 | 3 | 1 |
| advance | interrupt handling and timer | 26 | 4 | 1 | 14 | 0 | 1 | 0 | 1 | 5 |
| request | block requests | 15 | 1 | 3 | 5 | 2 | 0 | 0 | 4 | 0 |
| block | blocking and waking | 4 | 1 | 0 | 3 | 0 | 0 | 0 | 0 | 0 |
| **total** | | **101** | **13** | **4** | **38** | **21** | **5** | **0** | **10** | **10** |

All 10 missed mutants are listed as equivalent in `tests/verification/equivalent-mutants.txt`,
each with its reason:
- `ties` and `sequence`: the tie floor is always 0 on `m5-reference`, and the rank and
  counter bits are disjoint.
- `preempt`: idle traps are always S-mode interrupts.
- `advance`: the status checks always pass when their source is pending, `BLK_SOURCE` is 0,
  and the disk never fails.

None is missed unlisted, and every `Diverged` result equals the naive oracle's. M4's source
mutants pass unchanged (run 40092595129).

## 10. Known issues

- **R3 under B is not minimized (non-blocking).** Its first diverging seed is pinned, but
  the offline search had not finished after eight hours (§16). R3 is found under both
  mechanisms and minimized under A, and each race keeps at least one minimized finding.
- **C is not effective on `m5-reference` (non-blocking).** It changes no stream of any
  subject (§17). Its rank is judged by the dispatch order under seed 0 (§21), and C's
  property tests run on every reference platform.
- **The workload uses no `fence`, `wfi`, or misaligned and access-fault causes** (§17).
  `WFI` stays illegal (D10).
- **The nightly ran `m5_minimize::search` from M5.6b to M5.7b.** The search is ignored but
  not seeded, so the random-seed step ran it until the job was cancelled after six hours
  (runs 39848212966 and 40017140569). M5.7b excludes it.
- **The weekly perturbed every-event job has not yet run in CI.** It ran locally for A, B,
  and C (§6).

## 11. CI

The PR #67 runs passed every job: fmt/check/clippy/machete, the tests on both operating
systems, the M1–M5 cross-OS checks, the M2, M3, and M5 every-event resume shards, the rv32ui
fixture rebuild, ACT4/Sail, the Linux Spike differential, the nine M4 and eight M5
source-mutant targets with their summaries. PR #68 adds the M5 reproducer check to the
M5 cross-OS jobs, and passed every job (run 40095128365).

## 12. Final gate

Run locally on Windows at the audit's head, `e653fa9` plus this audit:

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo machete` | pass |
| `cargo nextest run --workspace --locked` | 1,587 passed, 7 skipped |
| `cargo test --workspace --doc --locked` | pass |
| `cargo doc --workspace --no-deps --locked`, `RUSTDOCFLAGS=-D warnings` | pass after the doc-link fix; CI does not run it |
| `cargo xtask m1-golden verify` to `m3-golden verify`, `m5-golden verify` | OK; each mid snapshot restores |
| `cargo xtask m4-expected verify`, `m5-expected verify` | the build reproduces both |
| `cargo xtask m5-coverage verify` | the architectural reports reproduce; the campaign report is weekly |
| `cargo xtask corpus replay` | 2 entries replay byte for byte |
| `cargo xtask m4-repro emit`, `m5-repro emit`, then `replay` | 6 M4 reproducers replay byte for byte; 7 M5 reproducers replay to their results |
| `cargo xtask rv32-fixtures verify`, `m5-reference verify` | the fixtures match their manifests; two runs of the M5 disk are identical |
| `git diff --check` | clean |

The Spike suites need the pinned Spike, so they are taken from CI run 40092595137 (§7).

## 13. Design updates

- m5-design status: M5 complete, checked by this audit.
- O2 and §11.3: `blk_read` is syscall 244, decided in M5.3a (§27.3). The design still
  marked it open.
- §25: M5.8 complete. §26 points to this audit.

## 14. Exit criteria (m5-design §26)

| # | Criterion | Evidence | Result |
|---|---|---|---|
| 1 | Identity: with the empty perturbation, M0–M4 goldens, expected streams, corpus, and reproducers byte-identical; every M0–M4 CI job passes | §5; every M0–M4 job in run 40092595137 | PASS |
| 2 | Determinism: perturbed runs, campaign results, minimized perturbations, and reproducers byte-identical on Linux and Windows; restore at every event | §6: `this_build_reproduces_the_expectations` on both OSes, the weekly coverage campaign, the M5 cross-OS reproducer check (run 40095128365), the every-event shards, and the perturbed resume | PASS |
| 3 | Detection: each of R1–R3 and R5 found by the CI campaign, by at least one mechanism, located by M4 at an `outcome` divergence | §8: `m5_campaign::*_diverges_under_*`, each `Judged::Diverged` at an `outcome` ordinal | PASS |
| 4 | No false positives: the unpatched scenario agrees on `m5.outcome` for every CI seed of every mechanism | `the_unpatched_scenario_agrees_under_{a,b,c}`; the coverage campaign, seeds 0–31 | PASS |
| 5 | Evidence: each finding reports the first differing schedule decision and the perturbation elements before it | `every_finding_reports_its_first_differing_decision` (this audit), `the_evidence_names_the_engines_schedule_divergence` | PASS |
| 6 | Minimization: each finding's perturbation 1-minimal with the same failure signature; its reproducer replays byte for byte | `m5_reproducer::*_under_*`: same failure, each part needed, replay to the bundle's result; R3 under B not minimized (§10) | PASS |
| 7 | Coverage: architectural, schedule, and perturbation coverage reported for `m5-reference` and `m3-reference` | `tests/verification/coverage/{m3-reference,m5-reference,m5-campaign}.json`, verified in CI and weekly. `m3-reference` gets architectural coverage, for comparison (§17): it has no preemption, so no schedule to cover | PASS |
| 8 | Spike: the `M5` profile's directed SEI tests agree with the pinned Spike | §7: `m5-sei`, 20 of 20 matched | PASS |
| 9 | Mutation: every source mutant of the M5 paths detected, or listed as equivalent with its reason | §9: 101 generated, 10 unviable, 81 detected, 10 listed | PASS |

**9 of 9 PASS, 0 FAIL.**

## 15. Release-candidate decision

**M5 release candidate: ACCEPT.**

- Every §26 criterion has concrete evidence and passes.
- No blocker was found:
  - no exit-criterion defect;
  - no mismatch between frozen semantics and code;
  - no M0–M4 regression;
  - no contracts change.
- §10's known issues are non-blocking.

`e653fa9` plus this audit is the frozen M5 release candidate. A tag, release notes, and a
version are left to a separate, explicit step.
