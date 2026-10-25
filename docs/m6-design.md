# M6 Design: Systematic Schedule Exploration

> Status: Draft for the M6.0 freeze · Parent: [plan.md](../plan.md) · Builds on: [m5-design.md](m5-design.md), [m4-design.md](m4-design.md), [m0-design.md](m0-design.md)

This document is the design of M6. Items marked **(open)** are decided at the step §20 names, without changing a frozen decision.

---

## 1. Goal and Shape

M6 replaces sampling with search. It explores the orderings the single-core M5 system admits, within stated bounds, and says what it explored.

M5 made timing an input and drew it from seeds. A seeded campaign finds a race when some seed happens to move an interrupt or a completion into the race's window. M5's races have wide windows, so 32 seeds found each of them. A race whose window is one instruction wide, or that needs two events to land in a particular order, is found by a seed only by luck, and a campaign that finds nothing says nothing about what it missed.

M6 keeps everything that made M5 findings reproducible: a run is deterministic, its timing is a recorded input, and every finding comes with a reproducer. What changes is how runs are chosen. M6 starts from the reference run, finds every point where the model admits another ordering, and explores those orderings systematically:

```text
              reference run (no deviation)
                         │
         choice points: tie groups and deliveries
         whose order at one component can change
                         │
        ┌────────────────┼────────────────┐
        ▼                ▼                ▼
   1 deviation      1 deviation   …   k deviations      ← bounded
   (from a checkpoint, never from the start)
        │                │                │
        └── each run ↔ reference (M4 engine, m5.outcome)
                         │
           Agree ────────┴──────── Diverged
                                      │
                      minimized schedule → M4 reproducer
```

M6 has four parts:

1. **Scripted schedules** (§7): an explicit list of deviations from the reference ordering, carried by M5's two runtime mechanisms, ties and jitter.
2. **A conflict relation** (§8): two events conflict when they reach the same component. Only conflicting orders are explored.
3. **A bounded search** (§9): every schedule with at most `k` deviations, each run branched from a checkpoint, with runs that rejoin the reference cut short.
4. **Schedule coverage, coverage-guided choice, and minimization** (§11–§12), checked against new planted bugs whose windows are narrow (§10).

## 2. Scope and Non-goals

### 2.1 In Scope

- scripted schedules: tie choices and message delays named by stable event identities (§7), in `perturbation.bin`, snapshots, and reproducers;
- the component-level conflict relation and the choice points of a run (§8);
- the bounded search, checkpoint branching, convergence, and deterministic parallel rounds (§9);
- planted order-dependent bugs R6–R8 with narrow windows, and a measured comparison with M5's seeded campaigns (§10);
- schedule coverage and coverage-guided choice (§11);
- schedule minimization and M4 reproducers (§12–§13);
- the search on `m5-reference`, and on `m2-reference` and `m3-reference` with their own judges.

### 2.2 Non-goals

- **No change to M0–M5 behavior.** A run without a schedule script is byte-identical to its golden files (§14).
- **No multiple harts.** The system stays the single-core M5 system. Atomics, inter-processor interrupts, and memory-model relaxation are plan.md §13 candidates after M9.
- **No address-level dependencies.** Conflicts are component-level (§8). They are refined only if M6's measurements show them too coarse (O7).
- **No new comparison engine, profile, or stream.** M6 reuses `m5.outcome`, the `schedule` stream, M4's locator, and M5's evidence.
- **No unbounded search and no completeness claim beyond the bounds.** "Exhausted" always names its bounds (§9.1).
- **No wall-clock budgets.** A budget counts runs and events, so a search's result never depends on host speed (§9.6).
- **No Perfetto export or UI.** M7 shows M6 schedules.

## 3. Frozen M6 Decisions

| # | Decision | Section |
|---|---|---|
| D1 | A schedule is a perturbation: a script of deviations from the reference ordering, encoded in `perturbation.bin` and replayed like any other perturbation | §7 |
| D2 | Two kinds of deviation only: a **tie choice** (reorder two events with equal `(tick, phase)` at one component) and a **delay** (deliver one message later, past a stated number of events at its target). Both are carried by M5's mechanisms: scripted ranks in the rank-major `sequence`, and scripted jitter on FIFO lanes | §7.2, §7.3 |
| D3 | Deviations name events by stable identities: a message by its directed link and ordinal, a self-wake by its component and ordinal. Never by `sequence`, which a deviation changes for every later event | §7.1 |
| D4 | Two events conflict when they are delivered to the same component. Orders of non-conflicting events are never explored | §8 |
| D5 | A delay is a position, not a number of cycles: "after the `i`-th later event at the target". The delays between two consecutive events at the target are treated as one, represented by the smallest. This is a stated approximation (§8.3) | §7.3, §8.3 |
| D6 | The search is bounded by the number of deviations `k`, the positions per delay `p`, the delay cycles `D`, and the link scope. Within its bounds it enumerates every schedule once. A budget-limited search reports what it left unexplored | §9 |
| D7 | Every branch restores a checkpoint of the run it extends. A run that rejoins the reference state after its last deviation is cut short as `Agree` | §9.3, §9.4 |
| D8 | The search is a pure function of the scenario, the bounds, and the budget. It advances in rounds, so its result is the same for every thread count and on every host | §9.6 |
| D9 | Coverage orders the search but never gates it. An exhaustive search explores the same set in any order | §11 |
| D10 | New planted bugs R6–R8 have narrow windows or need a tie choice. Each is measured against M5's seeded campaigns, and the comparison is reported, not judged | §10 |
| D11 | The contracts pin does not move. `EventKey` and every contracts type are unchanged | §17 |

## 4. What M0–M5 Provide

- **Deterministic DES (M0).** Events are ordered by `(tick, phase, sequence)`. Components interact only by messages on links and their own self-wakes, and keep their state to themselves. A session snapshots and restores at every event.
- **Verification (M4).** Profiles, the checkpoint locator, state and trace diffs, slices, reproducer bundles, and minimization.
- **Timing exploration (M5).**
  - Tie order C: a rank-major `sequence = rank << 40 | counter` (`runtime/src/ties.rs`). An event scheduled into the `(tick, phase)` being dispatched takes a rank at or above the current one, so the scheduler still dispatches in strict key order.
  - Jitter B: per-message delays on FIFO lanes, one lane per direction of a link, with a per-lane ordinal (`runtime/src/jitter.rs`). A listed jitter (`Jitter::only`) delays only the messages it lists.
  - `m5-reference` with its preemptive kernel, the `outcome` and `schedule` streams, `m5.outcome`, planted races R1–R3 and R5, minimization, coverage reports, and `perturbation.bin` in reproducers.
  - **The finding that shapes M6:** C reorders thousands of tie groups and changes no stream (m5-design §7.3). M6 explains it with the conflict relation (§8): a reorder of events at different components cannot change a result, and M6.2 measures how many tie groups hold two events at one component.

## 5. Terminology

| Term | Meaning |
|---|---|
| **Reference ordering** | The order of the run with no deviation: the M5 run with the empty perturbation |
| **Event identity** | A name for an event that stays the same across schedules until its sender's behavior changes (§7.1) |
| **Deviation** | One change to the reference ordering: a tie choice or a delay (§7) |
| **Schedule** | A canonical set of deviations, at most one per site. The empty schedule is the reference ordering |
| **Site** | The event at which a deviation takes effect: the event that sends the delayed message, or the dispatch of the tie group |
| **Choice point** | A site where the run admits a deviation that changes the order of two conflicting events (§8.2) |
| **Conflict** | Two events conflict when they are delivered to the same component (§8) |
| **Bounds** | `k`, `p`, `D`, and the link scope (§9.1) |
| **Exhausted** | Every schedule within the bounds was explored or cut short by a rule of §9 |
| **Narrow window** | A race that only a small fraction of the run's choice points can trigger (§10) |

## 6. Comparison Model

Each explored schedule is compared with the reference ordering on `m5.outcome`, as an M5 perturbed run is compared with the unperturbed one. Both sides are SystemScope, and every M4 and M5 piece of evidence applies: the outcome divergence, the first differing schedule decision, the per-process trace, and the shared-page reads.

`m2-reference` and `m3-reference` are not compared on `outcome`. Under the search, each of their runs must pass the scenario's own judge, as under M5's B and C.

## 7. Scripted Schedules

### 7.1 Event Identity (D3)

`sequence` cannot name an event across schedules: a delay changes the order of every later scheduling, and with it every later counter. M6 names events by what sent them:

- a message by `(link, dir, ordinal)`, the `n`-th message sent in one direction of a link, as M5's lanes already count;
- a self-wake by `(component, ordinal)`, the `n`-th wake the component scheduled;
- an `Init` event by its component.

An identity is stable across two schedules up to the point where its sender behaves differently, and that is exactly the part of a run a deviation can rely on. A deviation whose identity a run never produces is reported as **unreached**, never as an error, because an earlier deviation can legitimately change what is sent later.

In a scripted session, the runtime counts messages on every directed link and wakes on every component. The counts are runtime state, stored in the session's snapshot (O3).

### 7.2 Tie Choices

A tie choice orders two events with equal `(tick, phase)` that are delivered to the same component, against their reference order. It is carried by C's mechanism: the script assigns a rank to an event identity, and the runtime uses it, instead of a drawn rank, when the event is scheduled. Every unlisted event takes rank 0, so a script with no tie choice dispatches ties in ascending counter order, as M0–M4 do.

The runtime's floor rule stays: an event scheduled into the `(tick, phase)` being dispatched takes a rank at or above the current one. So a tie choice can only reorder events that were all queued before their group began, and the search only offers such choices. A script that asks for anything else is rejected at the site, as a construction error of the schedule (§15).

### 7.3 Delays (D5)

A delay delivers one message later than the reference. It is carried by jitter's mechanism: the script lists `(link, dir, ordinal, cycles)`, and the runtime adds `cycles` in `Ctx::try_send`, as it adds a drawn jitter. The lane stays FIFO: a later message on the same lane is delivered at `max(its tick, the lane's latest tick)`, as in M5.

The search does not choose cycles. It chooses a **position**: "after the `i`-th event delivered to the target after the message's reference delivery", for `1 ≤ i ≤ p`. The cycles are the smallest that put the delivery in the tick after that event, and a position more than `D` cycles away is out of bounds. Every delay between two consecutive events at the target leaves the target's order unchanged, so M6 explores one of them (§8.3).

`Cycles` links only, as for jitter: a delay of `j` cycles is defined only on a link whose latency is a whole number of cycles.

### 7.4 Encoding and Identity (D1)

A schedule is a perturbation. `perturbation.bin` gains a scripted jitter (jitter tag 3: the list of delays, sorted by identity) and scripted ranks (tie tag 2: the list of `(identity, rank)`, sorted by identity). A schedule's identity is its encoding's BLAKE3, as for any perturbation, and it is part of the session identity: a snapshot from a scripted run never restores into another session, except by §9.3's branching rule.

The empty schedule encodes as the empty perturbation. So the reference run is the M5 unperturbed run, byte for byte.

## 8. Conflict Relation (D4)

### 8.1 Why Component Level

A component's handler reads and writes only its own state, and acts on the rest of the system only by sending messages and scheduling wakes. Two events delivered to different components therefore commute: dispatching them in either order leaves every component in the same state and sends the same messages. The only difference is the counters of what they schedule, which decide later ties among those messages. Every such later tie is a choice point of its own, so the search covers it there.

Two events delivered to the same component may not commute: the second sees the state the first left. M6 explores those orders and no others.

The relation is coarse. A bus read and a bus write to unrelated addresses conflict, because both reach the bus. That costs runs, never findings: a coarser relation explores more orders, not fewer. Address-level refinement is a non-goal (O7).

### 8.2 Choice Points

The reference run's trace gives every choice point within the scope:

- a **tie point**: a `(tick, phase)` group holding two or more events delivered to one component;
- a **delay point**: a message on a scoped link, with each position `i ≤ p` whose cycles are at most `D`.

A run under a schedule has its own choice points after its first deviation, found from its own trace in the same way. M6.2 reports the number of each kind on `m2-`, `m3-`, and `m5-reference`, which also tests the explanation of M5's finding: if no tie group holds two events at one component, C could not have changed a result.

### 8.3 The Delay Approximation (D5)

Two delays that land between the same two events at the target give the target the same order of events. They are not always equivalent: a handler that reads the current tick, such as the timer's arming, sees a different `now`. M6 explores the smallest delay of each interval and states the approximation:

- the coverage report lists, per target, how many delay points it represented this way;
- M6.2 checks the approximation on a sample: for sampled delay points, every delay of the interval is run and compared with its representative, and the share of intervals whose delays disagree is reported. If it is not small, `p` gains a second representative per interval (the largest) before the bounds are frozen (O1).

## 9. Search (D6)

### 9.1 Bounds

| Bound | Meaning |
|---|---|
| `k` | the most deviations in one schedule |
| `p` | the most positions per delay |
| `D` | the most cycles per delay, as jitter's `max_cycles` |
| scope | the links whose messages may be delayed, as jitter's scope: device, DMA, and interrupt links by default |

A search **exhausts** its bounds when every schedule within them was run or cut short by a rule of this section. Its report always states the bounds it exhausted, or the frontier it left when its budget ran out. The CI and weekly bounds are frozen at M6.3 from measured costs (O1).

### 9.2 Enumeration

The search is deviation-bounded: it explores the schedules with 0, then 1, up to `k` deviations. A schedule of `n + 1` deviations extends one of `n` by a deviation at a choice point after its last site, taken from the extended run's own trace (§8.2). So each schedule is reached from exactly one parent, sites in ascending event order, and is enumerated once.

A deviation that commutes with the rest of its schedule is never enumerated twice under two orders, because a schedule is a set ordered by site. A deviation whose identity the extended run never produces is unreached (§7.1), and its schedule is not run.

### 9.3 Checkpoint Branching (D7)

A schedule differs from its parent only after its new site. So the search restores the parent's latest checkpoint before that site, instead of running from the start. Restoring a snapshot into a session with a longer script is the **branching restore**: it is accepted when the snapshot's script is the session's script without entries whose sites are after the snapshot, and refused otherwise. It is a new restore rule for scripted sessions only. Every other restore is unchanged (m0-design §7).

Checkpoints are taken at fixed tick multiples, in the reference run and in every run the search may extend, and kept within a memory budget that counts bytes.

### 9.4 Convergence (D7)

Many deviations change nothing: a delayed completion that the kernel polls a little longer for, or a tie of two messages that the target handles alike. After its last deviation, a run whose behavioral state equals the reference's at the same checkpoint tick has rejoined the reference, and its future is the reference's. It is cut short and counted as `Agree` and **converged**.

The behavioral state is every component's snapshot and the queued events, each as its tick, phase, identity, and payload. It leaves out the counters, the lanes' bookkeeping, and the script, which name events but do not change what they do (O4). The search checks convergence only after the last deviation, so no remaining script entry can make the futures differ. A converged run's outcome is not recomputed. The validation (§16) runs a sample of converged runs to the end and requires them to agree.

### 9.5 Order

Within each deviation count, the search takes choice points in the order coverage guidance gives (§11.2), and canonically within equal scores: by site, then by kind, then by position. Guidance changes how soon a finding or new coverage is reached, never the set an exhausted search explores (D9).

### 9.6 Rounds and Budget (D8)

The search advances in rounds. A round takes the next `B` schedules from the frontier in §9.5's order, runs them on any number of host threads, and merges their results in the frontier's order. The order of the next round depends only on merged results. So a search's report is the same for any thread count and on any host, and two hosts produce the same bytes.

A budget counts runs and events, never wall time. When it runs out, the report lists the explored schedules and the frontier, so a later search can resume from it.

## 10. Planted Order-Dependent Bugs (D10)

M5's races have wide windows: each diverged for 14 to 32 of 32 seeds under A or B (m5-design §17). M6 adds races that sampling should rarely find. Each is an input variant of `m5-reference` with its correct counterpart, as in M5, and found by pattern, so a rebuilt program fails the variant instead of patching the wrong bytes.

| Variant | Bug | Window |
|---|---|---|
| R6 `dma-irq-race` | a kernel defect: the block-completion handler reads `STATUS`, then acknowledges the controller's interrupt. A second completion that lands between the read and the acknowledgement is acknowledged and never handled, so its reader never wakes | the few instructions between the read and the acknowledgement, while both readers have requests outstanding |
| R7 `peterson-order` | `counter` sets `turn` before its own `flag`, the textbook ordering bug of Peterson's algorithm | a preemption between those two stores, while the other instance is between its own |
| R8 `tie-defect` | a test-only defect flag in one device whose result depends on the order of two deliveries in one `(tick, phase)` (O6) | a tie choice. No delay within the bounds produces the failing order without one |

R6 and R7 are the narrow windows; R8 is the first planted bug only a tie choice finds, which M5's C could not have shown. R6 and R8 are defect flags, appended to their configuration's encoding only when set, so every golden is unchanged (m5-design §12.3).

For every race, R1–R3 and R5–R8, the report gives:
- the search's runs to its first finding and the bounds that found it;
- M5's seeded campaign over the same races: how many of seeds 0–31 diverge under A and B.

The comparison is a report. A gate on it would make M6's exit depend on M5's seeds missing something.

## 11. Schedule Coverage (D9)

### 11.1 Report

Coverage is a reviewed JSON file, blessed and verified byte for byte as M5's reports are (m5-design §17). For each subject and bounds:

- **Choice points**: tie points and delay points by target component, and the delay points represented by the approximation of §8.3.
- **Conflicting pairs**: for each component, each pair of incoming sources (a directed link or the component's own wakes) that ever arrive within one position of each other, and whether each order of the pair was explored.
- **Preemption points**: per program, the distinct `sepc` of its `preempt` decisions, as M5 reports them, across every explored schedule.
- **Schedules**: run, converged, unreached, diverged, and failed; the distinct `schedule` streams by BLAKE3; and whether the bounds were exhausted or the frontier left.

### 11.2 Coverage-Guided Choice

A choice point's score is the number of conflicting-pair orders and preemption points it could add that the explored set has not covered, read from the trace of the run it extends. Higher scores go first. The score is a function of the merged results only, so it keeps the search deterministic (§9.6). Its exact form is frozen at M6.5 (O5).

## 12. Minimization

A failing schedule has at most `k` deviations, so its minimization is small:
1. Delta debugging removes deviations while the failure keeps M4's failure signature (m4-design §17.2), down to a 1-minimal set.
2. Each remaining delay moves to its smallest failing position.

Each candidate is a full deterministic run, branched from a checkpoint when it can be. The result is a schedule from which no deviation can be removed, and no delay moved earlier, without losing the failure. It usually names one or two events at one component, which explains the race on its own.

## 13. Reproducer

The minimized schedule is the side's `perturbation.bin` (§7.4), so the M4 bundle is unchanged and the bundle format version stays (m5-design §18). Every M4 and M5 bundle and corpus entry stays valid. Each race's minimized reproducers are a pure function of the committed fixtures and equal byte for byte on Linux and Windows, as M5's are.

## 14. Determinism and Compatibility

- **Identity.** A session without a script takes the M0–M5 path. Every M0–M5 golden, expected stream, corpus entry, coverage report, and reproducer is byte-identical, and CI keeps all their jobs.
- **Snapshots.** A scripted session writes a new snapshot format version with its script and counts (O3), and restores at every event. `m5-reference` gets the every-event resume check under one schedule of each race.
- **Cross-OS.** Search reports, coverage reports, findings, minimized schedules, and reproducers are byte-identical on Linux and Windows.
- **Observation.** Observing a scripted run does not change it (m4-design §28.2).

## 15. Failure Model

- An invalid schedule fails construction: an undeclared link, a delay on a link that is not `Cycles` or out of scope, a duplicate identity, or bounds out of range. A tie choice the floor rule forbids fails at its site with a schedule error, which the search never produces and reproducers never hold.
- A scripted run that faults the session is a finding, reported as M4's `BackendFailed` with the schedule, as in M5. A model must tolerate every order its protocol allows.
- A run that never shuts down stops at the instruction limit, and the `outcome` projection treats the limit as a stop rule, as for R5 (m5-design §20).

## 16. Validation

- **Commutation.** A property test reorders random ties of events at different components and requires every component's sequence of event identities and its final snapshot to be unchanged. It checks the conflict relation's premise on every reference platform.
- **Scripts.** Encoding round-trips. The empty script is the identity. A script is FIFO per lane. The floor rule holds. The branching restore accepts exactly the scripts of §9.3.
- **Convergence.** A sample of converged runs, run to the end, agrees with the reference.
- **Enumeration.** On a small synthetic topology, the search's schedules within small bounds equal a brute-force enumeration, each once.
- **Approximation.** §8.3's sample.
- **Planted bugs.** Every race is found within the CI bounds, and the unpatched scenario agrees on every schedule within them.
- **Mutation.** Source mutants of the script layer, the conflict relation, the choice-point finder, convergence, and minimization are judged against committed expectations: each race's pinned finding and minimized schedule, and the unpatched scenario's schedule count within the CI bounds. Each mutant is detected or listed as equivalent with its reason.

## 17. Contracts (D11)

Scripted ranks and delays are runtime-internal, like C and B. Event identities are computed by the runtime from links and components it already has. `EventKey`, `TraceHeader`, and every contracts type are unchanged, and the contracts pin does not move.

## 18. Architecture Risks

1. **State explosion.** Choice points grow with the run, and schedules with `k`. Mitigation: component-level conflicts, positions instead of cycles, convergence, checkpoint branching, small CI bounds, and budgets that report their frontier.
2. **The approximation hides a race.** A handler that reads the tick could fail only at an unexplored delay of an interval. Mitigation: §8.3's measurement before the bounds are frozen, and a second representative if needed.
3. **Convergence merges runs that differ.** If the behavioral state leaves out something that changes behavior, a divergence is missed. Mitigation: convergence is checked only after the last deviation, and §16 runs a sample to the end.
4. **Tie choices stay ineffective.** If no reference platform has two events at one component in one tie group, tie choices only find R8. Mitigation: that is a measurement M6.2 reports, and R8 checks that tie choices can find a race at all.
5. **Counting every link costs time.** A scripted session counts messages on every link. Mitigation: only scripted sessions count, and M6.1 measures the cost.

## 19. Open Questions

| # | Question | Decided at |
|---|---|---|
| O1 | The CI and weekly bounds `k`, `p`, `D`, scope, and budget, and whether `p` takes a second representative per interval | M6.3 **(open)** |
| O2 | Event identity of a wake scheduled at `Init`, and of an event a restored snapshot already holds | M6.1 **(open)** |
| O3 | The scripted session's snapshot format version and layout, and whether counts cover every link or only scoped ones | M6.1 **(open)** |
| O4 | What the behavioral state leaves out (§9.4) | M6.3 **(open)** |
| O5 | The coverage score's exact form (§11.2) | M6.5 **(open)** |
| O6 | R8's device and defect | M6.4 **(open)** |
| O7 | Whether address-level conflicts are needed | M6.8 exit audit **(open)** |
| O8 | Where the search lives: a library module beside the M4 engine, since M7 will display its results, or the acceptance crate with M5's campaigns | M6.3 **(open)** |

## 20. Roadmap

| Step | Status | Scope | Changes |
|---|---|---|---|
| **M6.0** | in progress | Design freeze (this document) | docs |
| **M6.1** | planned | Scripted schedules: event identities, scripted ranks and delays, the `perturbation.bin` tags, the snapshot format (O2, O3), the branching restore; identity proven on every M0–M5 golden | code, tests |
| **M6.2** | planned | Choice points and the conflict relation: the choice-point finder, the commutation property, the approximation sample, the counts on `m2-`, `m3-`, and `m5-reference` | code, tests |
| **M6.3** | planned | The search: enumeration, checkpoint branching, convergence, rounds and budgets; the unpatched scenario exhausted within the CI bounds; bounds frozen (O1, O4, O8) | code, CI |
| **M6.4** | planned | Planted bugs R6–R8 and their correct counterparts; findings pinned; the comparison with M5's campaigns (O6) | code, fixtures |
| **M6.5** | planned | Schedule coverage and coverage-guided choice; the report blessed (O5) | code, CI |
| **M6.6** | planned | Schedule minimization, evidence, and cross-OS reproducers | code, CI |
| **M6.7** | planned | Source mutants of the M6 paths; weekly search on the wider bounds | code, CI |
| **M6.8** | planned | Exit audit (O7) | docs |

## 21. M6 Exit Criteria

- [ ] **Identity.** Without a script, every M0–M5 golden, expected stream, corpus entry, coverage report, and reproducer is byte-identical, and every M0–M5 CI job passes.
- [ ] **Determinism.** Every search report, coverage report, finding, minimized schedule, and reproducer is byte-identical on Linux and Windows and for every thread count, and every scripted run restores at every event.
- [ ] **Detection.** Every planted order-dependent bug, R1–R3 and R5–R8, is found within the stated CI bounds and located by M4 at a divergence in `outcome`.
- [ ] **No false positives.** The unpatched `m5-reference` agrees on `m5.outcome` on every schedule within the CI bounds, which the search exhausts, and `m2-` and `m3-reference` pass their judges within theirs.
- [ ] **Minimization.** Each finding's schedule is minimized to a 1-minimal set that keeps its failure signature, and its reproducer replays byte for byte.
- [ ] **Coverage.** The coverage report states, for each subject, the bounds, the choice points, the conflicting-pair orders and preemption points explored, and whether the bounds were exhausted.
- [ ] **Comparison.** For every race, the report gives the search's runs to first finding beside M5's seeded campaign.
- [ ] **Mutation.** Every source mutant of the M6 paths is detected, or listed as equivalent with its reason.
