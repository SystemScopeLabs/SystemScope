# M4 exit audit (M4.8)

This audit checks M4, `docs/m4-design.md`, against its exit criteria (§28.2). It covers
function, compatibility, determinism, validation, and mutation evidence, and decides whether
M4 can close as a release candidate. Besides documentation, it changes two things:
- `verification/src/trace.rs` trims the trace scan's ring with one `drain` instead of a
  loop. The behavior is the same. The loop guard was M4.3's one surviving mutant that is not
  equivalent, and no workload could show it (§9).
- A doc comment in `tests/acceptance/src/m3/verify.rs` no longer links to a private
  constant, so `cargo doc` passes with `-D warnings` (§12).

No schema, contract, fixture, golden file, or expected stream changed, and nothing was
re-blessed. The other changes bring the design's status, `plan.md`, and `README.md` up to
date, including the roadmap order agreed after M4.7 (§13).

This audit does not create a tag, a release, a version bump, or a changelog entry
(m4-design §27, M4.8: "Exit audit", docs).

- **Base:** `main` at `29f406b` (`docs(m4): record M4.7 and mark M4.8 next`, the M4.7
  merge).
- **M4 range:** `fd9f5f1..29f406b` (the M3 exit audit merge to the M4.7 merge), PRs
  #40–#53:

  | Step | PR | Merge |
  |---|---|---|
  | M4.0 design | #40 | `3f70992` |
  | cargo-mutants CI and the gaps it found | #41–#45, #49 | `52b933f`, `23daef4`, `cb05807`, `21cff7a`, `3b196d7`, `b229026` |
  | M4.1 | #46 | `ad738b2` |
  | M4.2 | #47 | `dcdc8a7` |
  | M4.3 | #48 | `258d7bf` |
  | M4.4 | #50 | `9ed452d` |
  | M4.5 | #51 | `f4e8cd4` |
  | M4.6 | #52 | `f9a89d6` |
  | M4.7 | #53 | `29f406b` |

- **Contracts pin:** `SystemScopeLabs/contracts` at `90900a128d51f6b42e86dc789447882c90c52ef4`
  throughout the range.
- **Toolchain:** `rust-toolchain.toml`.
- **Local runs:** Windows 11 x86-64, plus WSL Ubuntu 24.04 for Spike.
- **CI:** the PR #53 runs at `29f406b`: CI 39143186380 and Source mutants 39143186466, both
  successful. After a fast-forward merge, `main` holds the same tree, so its run is not
  repeated.

## 1. Scope

m4-design §1 defines M4 as the verification and debugging engine. It locates, characterizes,
minimizes, and reproduces the first divergence between two executions or implementations, at
stable architectural boundaries. The non-goals are in §2: no RTL, native kernel, or UI, no
scenario reduction (§17.3), and no fuzzing beyond deterministic seeded campaigns.

## 2. Milestone inventory

| Step | Delivered | Tested | Documented |
|---|---|---|---|
| M4.1 | `systemscope-verify`: observations, canonical encoding v1, prefix digest, comparator, profiles, the runtime adapter, the `rv32.*` projection, the naive oracle | `verification/tests/{observation,compare,case,runtime}.rs`, `tests/rv32/tests/projection.rs` | §30.1 |
| M4.2 | Locator: lockstep scan, stream compare, checkpoint index and search, window scan, capability choice | `verification/tests/{locate,locate_props,checkpoint}.rs` | §30.2 |
| M4.3 | State diff (snapshot, decoder, and `arch`-replay sources), trace evidence, relevant-predecessor slices, deterministic reports | `verification/tests/{state,latent,trace,slice,result}.rs` | §30.3 |
| M4.4 | Reproducer bundles: write, validate, replay; `cargo xtask m4-repro`; cross-OS CI | `verification/tests/bundle.rs`, the M4 cross-OS jobs | §30.4 |
| M4.5 | Prefix and replay-window minimization | `verification/tests/minimize.rs` | §30.5 |
| M4.6 | Heterogeneous sides (`report::Make`), the Spike backend and four Spike profiles, SK-1 | `verification/tests/heterogeneous.rs`, `tests/rv32/tests/{spike_adapter,sk1}.rs`, the Linux `spike` job | §30.6 |
| M4.7 | Recorded backend, expected streams, corpus, source mutants, fault and Spike campaigns | `verification/tests/recorded.rs`, `tests/acceptance/tests/m4_{expected,corpus,mutants,faults}.rs`, the source-mutants workflow | §30.7 |

The acceptance cases SP-1 to CB-2 and the agreement case are in
`tests/acceptance/tests/m4_verify.rs`.

## 3. Non-goals (m4-design §2)

None was implemented:
- no RTL code, Verilator, or hardware-description tooling;
- no native kernel;
- no UI, TypeScript, or display fields in observations or bundles;
- no scenario reduction;
- no fuzzing: every campaign is an explicit seed list, and the nightly seeds are printed.

## 4. Architecture boundaries

- `systemscope-verify` depends on exactly `systemscope-contracts`, `systemscope-runtime`,
  `blake3`, and `serde_json` (§22, D7). It knows nothing of `rv32i`, `platform`, `os`, `elf`,
  or any test crate. Platform knowledge enters through projections in `tests/rv32` and
  `tests/acceptance`.
- The Spike adapter reads Spike's log only and shares no decoding code with the SystemScope
  side (§8.4).
- Raw events are compared only between two runs of the same engine (D1, D2). Across backends,
  only the `arch` stream and its profiles are compared.

## 5. Compatibility

- **Golden files.** `git diff v0.4.0-m3..29f406b -- tests/golden` is empty: every M0–M3
  golden file and mid-run snapshot is byte-identical to the M3 release.
- **Fixtures.** The only change under the fixture directories is a new file,
  `tests/rv32/spike/rv32ui-add.log` (M4.6, SK-1's committed log). No existing fixture
  changed, and `rv32-fixtures verify` passes.
- **Production code.** Five commits in the range change non-test code in components or
  the runtime. Four are refactors from the cargo-mutants rounds: `43ec0f8` (os), `9dc3563`
  (platform), `d0b14fc` (rv32i), and `b229026` (toy). The fifth, `575c859`, adds
  `runtime::snapshot::component_states`, a read-only reader for M4.3. The goldens above show
  no behavior changed.
- **Contracts.** The pin is unchanged, and M4 adds no protocol or contract (§23).

## 6. Determinism and cross-OS

- Verification results and bundles are canonical (§8.3, §16.2). `reports_are_byte_identical_across_runs`
  and the M4.3 report checks in `m4_verify.rs` render each report twice and compare bytes.
- `M4 cross-OS (ubuntu-latest checks windows-latest)` and `(windows-latest checks
  ubuntu-latest)`: each OS emits the agreement case and the minimized SP-1 to CB-2 with
  `m4-repro emit`, and the other checks them byte for byte and replays them.
- The expected streams are canonical. The source-mutant workflow verifies them on Linux, and
  the tests verify them on both operating systems.
- SK-1 and the Spike campaign need Spike, which CI runs only on `ubuntu-24.04` (§28.1).

## 7. External validation

- **Spike** (pinned `19609434`): all 157 differential comparisons agree through the M4 stream
  compare: rv32ui 40, generated 64, misaligned 9, M2 CSR 20, M3 privilege 13, M3 Sv32 11.
  SK-1 is located at `arch[35]`, `retire`, `rd_value`, and its reproducer replays with a
  second Spike run. The campaign over seeds 64 to 127 agrees, with 22,429 observations
  compared.
- **ACT4/Sail** and the rv32ui fixtures pass as in M3.

## 8. Acceptance workload (m4-design §28.1)

| Item | Required | Found |
|---|---|---|
| primary `arch` observations of `reference` | 2,200: 2,176 `retire`, 23 `exception`, the halting trap | 2,200 (`unmodified_sides_agree_on_every_stream` asserts `instret + exceptions + 1` from the golden) |
| controlled divergences | SP-1, SP-2, SP-3, CB-1, CB-2, SK-1 | all six, each at its selector's boundary |
| Spike comparisons | 157 | 157 |
| engine mutation classes | twelve | twelve, each with mutants (§9) |
| source mutation targets | nine classes | nine, 154 mutants (§9) |

## 9. Mutation inventory

**Engine mutants**, from the step records in m4-design §30:

| Step | Scope | Generated | Caught | Unviable | Timeout | Missed after fixes |
|---|---|---|---|---|---|---|
| M4.1 | `systemscope-verify` | 113 | 92 | 20 | 1 | 0 |
| M4.2 | `systemscope-verify` | 277, rerun 61 | 207, then 56 | 50, then 3 | 8 | 2, both equivalent |
| M4.3 | `systemscope-verify` | 710, rerun 193 | 549, then 175 | 98, then 2 | 10, then 2 | 13 listed (12 equivalent, 1 not), plus `Session::inspect` |
| M4.4 | `bundle.rs`, `report.rs`, `locate::resume` | 145, rerun 50 | 113, then 44 | 22, then 6 | 0 | 0 |
| M4.5 | `minimize.rs`, `report::searched` | 18, rerun 20 | 13, then 18 | 4, then 2 | 0 | 0 |
| M4.6 | M4.6 changes to `locate`, `report`, `bundle`, `minimize` | 48 | 37 | 11 | 0 | 0 |
| M4.7 | `recorded.rs` | 18 | 14 | 4 | 0 | 0 |

Timeouts are hangs, which are detections. The listed equivalents carry their reasons in §30.2
and §30.3.

**M4.3's non-equivalent survivor.** §30.3 kept one mutant missed: the trace ring's
`b.len() > ring` to `==`. It changes the result only when the ring lands exactly on its size
and the next event's first record differs. The counter cannot produce that, and with the ring
at `SLICE_RECORDS` (4,096) no real workload can. This audit replaces the trimming loop with
`b.drain(..b.len().saturating_sub(ring))`, which keeps the same records. A rerun of
`scan`'s mutants generates 35: 30 caught, 2 unviable, and 3 missed. The 3 are equivalents
§30.3 already lists: `keep > 0`, the slice's `record + from`, and `kept.len() < want`.

**Engine mutation classes** (§25.2): each has mutants and named tests:
- the eight M4.1 classes are in `compare.rs` and `runtime.rs` (§30.1);
- prefix-digest continuation and boundary count are in `locate.rs` and `locate_props.rs`
  (`BoundaryDesync`, `a_replay_that_does_not_reproduce_its_index_is_a_boundary_desync`);
- nondeterministic report ordering is `reports_are_byte_identical_across_runs`;
- the minimizer's signature is `a_candidate_must_keep_the_searched_failure`.

**Source mutants** (§25.2, §30.7): 154 generated for the nine targets. 63 diverge, 6 stop a
side, 11 are unviable, and 74 are listed as equivalent with reasons. None is missed
unlisted, and every `Diverged` result equals the naive oracle's. The source-mutants workflow
gives the same table on Linux as a local run on Windows.

The scheduled `mutants.yml` run is exploratory and never a gate.

## 10. Known issues

- **Narrow acceptance workload (non-blocking).** `m3-reference` retires only LUI, AUIPC,
  ADDI, BNE, LW, and LBU among the ALU, branch, and load instructions, takes no interrupt,
  and walks only well-formed PTEs. So 69 of the 74 equivalent source mutants are behavior it
  never exercises. The workload is frozen for M4. Widening it, with interrupts and DMA, is
  part of M5 (§13).
- **SK-1 and the Spike campaign run on Linux only**, as §28.1 scopes them.

## 11. CI

The PR #53 runs passed every job: fmt/check/clippy/machete, the tests on both operating
systems, the M1–M4 cross-OS checks, the M2 and M3 every-event resume shards, the rv32ui
fixture rebuild, ACT4/Sail, the Linux Spike differential with SK-1 and the campaign, and the
nine source-mutant targets with their summary.

## 12. Final gate

Run locally on Windows at the audit's head:

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo machete` | pass |
| `cargo nextest run --workspace --locked` | 1,425 passed, 5 skipped |
| `cargo test --workspace --doc --locked` | pass |
| `cargo doc --workspace --no-deps --locked`, `RUSTDOCFLAGS=-D warnings` | pass after the doc-link fix; CI does not run it |
| `cargo xtask m1-golden verify`, `m2-golden verify`, `m3-golden verify` | OK |
| `cargo xtask m4-expected verify` | the build reproduces the expected streams |
| `cargo xtask corpus replay` | 2 entries replay byte for byte, `sp2` `Diverged` |
| `cargo xtask m4-repro emit target/m4-repro`, then `replay` | 6 reproducers replay byte for byte |
| `cargo xtask rv32-fixtures verify` | the M3 fixtures match their manifest |
| `git diff --check` | clean |

The Spike suites, SK-1, and the Spike campaign need the pinned Spike, so they are taken from
CI run 39143186380 (§7, §11).

## 13. Roadmap after M4

Agreed after M4.7, and recorded in `plan.md` §11 and `README.md`:

| Milestone | Scope |
|---|---|
| M5 | Timing Exploration: coverage of the `arch` stream, a wider reference workload with interrupts and DMA, and seeded timing-perturbation campaigns compared through the M4 engine |
| M6 | Native guest OS backend: a C/assembly tiny kernel |
| M7 | Time-travel queries and CLI first, then the Visualizer / Interactive Debugger on them |
| M8 | SystemVerilog CPU backend via Verilator (planned as M5 until M4.8) |

m4-design's references to the RTL backend as M5 (§2, §6–§9, §24.1, §26, §29) are frozen
history. They now refer to M8.

## 14. Exit criteria (m4-design §28.2)

| # | Criterion | Evidence | Result |
|---|---|---|---|
| 1 | Detection: each of the six controlled divergences is `Diverged` | `sp1_a_patched_pc_diverges_at_the_next_retirement`, `sp2_a_patched_register_diverges_at_its_first_reader`, `sp3_a_patched_stvec_diverges_at_the_handler`, `cb1_a_patched_code_word_diverges_at_its_first_retirement`, `cb2_an_illegal_code_word_diverges_as_an_exception`, `sk1_is_located_where_the_selector_says`; SK-1 against the pinned Spike in the Linux `spike` job | PASS |
| 2 | First divergence equals the naive oracle's and the selector's | the same tests compare every strategy with `compare_recordings` and the selector; `every_strategy_gives_the_naive_primary_result` | PASS |
| 3 | Agreement: `reference` against itself, 2,200 primary observations | `unmodified_sides_agree_on_every_stream` | PASS |
| 4 | Structured diff is deterministic | `reports_are_byte_identical_across_runs`; each SP/CB report rendered twice in `m4_verify.rs` | PASS |
| 5 | Reproducer rebuilds each side and replays byte-identically; corrupted variants refused | `a_diverged_bundle_holds_its_material_and_replays_byte_identically`, `every_manifest_byte_is_checked`, `every_entry_byte_is_checked`, `checkpoints_must_belong_to_their_side_and_restore`, `a_larger_local_budget_changes_nothing`, `a_heterogeneous_reproducer_starts_the_runtime_at_its_window`; every SP/CB bundle in `m4_verify.rs` | PASS |
| 6 | Minimization: origins at the latest covering checkpoints, same result | `a_minimized_divergence_replays_from_the_latest_window_checkpoint`, `the_window_starts_at_the_latest_common_checkpoint`; SP-1 to CB-2 minimized at 133 and 16 in `m4_verify.rs` (§30.5) | PASS |
| 7 | Cross-OS byte equality for SP-1 to CB-2 and the agreement case | the two `M4 cross-OS` jobs, run 39143186380 | PASS |
| 8 | Corpus: an SP-2 `Diverged` entry and an `Agree` entry, replayed in CI | `tests/verification/corpus/{sp2,agree}`; `every_corpus_entry_replays` | PASS |
| 9 | Capabilities: snapshot-capable and snapshot-incapable pairs | SystemScope ↔ SystemScope by checkpoint search (`m4_verify.rs`, `locate_chooses_by_capability_and_uses_no_other`); SystemScope ↔ Spike by stream compare (`sk1.rs`, `spike_adapter.rs`, the `spike` job) | PASS |
| 10 | Spike through M4: 157 comparisons, each `Agree` | the Linux `spike` job, run 39143186380 (§7) | PASS |
| 11 | No regression: M0–M3 goldens, schemas, fixtures, and pin unchanged; M0–M3 CI passes | §5; every M0–M3 job in run 39143186380 | PASS |
| 12 | Observation invariance | `observing_changes_nothing`; `unmodified_sides_agree_on_every_stream` checks the state and execution digests against the golden under the M4 adapters; `the_reference_scenario_is_the_same_under_every_observation` | PASS |
| 13 | Mutation: every engine class has mutants, every mutant killed; every source mutant detected or listed as equivalent | §9: no missed engine mutant outside the listed equivalents after this audit's `trace.rs` change; source mutants 154, 74 listed with reasons | PASS |

**13 of 13 PASS, 0 FAIL.**

## 15. Release-candidate decision

**M4 release candidate: ACCEPT.**

- Every §28.2 criterion has concrete evidence and passes.
- No blocker was found:
  - no exit-criterion defect;
  - no mismatch between frozen semantics and code;
  - no M0–M3 regression;
  - no contracts change.
- The narrow acceptance workload is recorded as a non-blocking known issue (§10). M5
  addresses it.

`29f406b` plus this audit is the frozen M4 release candidate. A tag, release notes, and a
version are left to a separate, explicit step.
