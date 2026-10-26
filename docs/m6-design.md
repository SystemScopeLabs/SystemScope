# M6 Design: Systematic Schedule Exploration

> Status: Draft for the M6.0 freeze · Parent: [plan.md](../plan.md) · Builds on: [m5-design.md](m5-design.md), [m4-design.md](m4-design.md), [m0-design.md](m0-design.md)

This document is the design of M6. Items marked **(open)** are decided at the step §21 names, without changing a frozen decision. Revisions 2 and 3 of the draft answer the M6.0 design audit and its review, revision 4 adds shifts of asynchronous wakes after R6's witness search, and revision 5 replaces group orders with picks after the oracle's design (§23).

---

## 1. Goal and Shape

M6 replaces sampling with search. It explores the orderings the single-core M5 system admits, within stated bounds and budgets, and says exactly what it explored.

M5 made timing an input and drew it from seeds. A seeded campaign finds a race when some seed happens to move an interrupt or a completion into the race's window. M5's races have wide windows, so 32 seeds found each of them. M5 also recorded races it could not find: swapping Peterson's two entry stores, or publishing the mailbox's flag before its data, leaves a window of one instruction, and no seed hit either (m5-design §27.7). A campaign that finds nothing says nothing about what it missed.

M6 keeps everything that made M5 findings reproducible: a run is deterministic, its timing is a recorded input, and every finding comes with a reproducer. What changes is how runs are chosen:

```text
              reference run (empty script)
                         │
         choice points: same-tick dispatches to pick,
         messages to delay by 1..D cycles,
         asynchronous wakes to shift by ±1..E cycles
                         │
        ┌────────────────┼────────────────┐
        ▼                ▼                ▼
   1 deviation      1 deviation   …   k deviations      ← bounded, budgeted
   (forked from a checkpoint before its branch point)
        │                │                │
        └── each run ↔ reference (M4 engine, m5.outcome)
                         │
           Agree ────────┴──────── Diverged
                                      │
                    1-minimal schedule → M4 reproducer
```

M6 has four parts:

1. **Scripted schedules** (§7): an explicit list of deviations from the reference ordering, carried by M5's tie and jitter mechanisms, naming events by stable identities.
2. **An exact search domain** (§8): every admissible order of each tie group, chosen one dispatch at a time, every delay of `1..=D` cycles of each scoped message, and every shift of `±1..=E` cycles of each asynchronous wake, such as a timer expiry. M6 prunes nothing from it. A reduction may come later only with proof against an exhaustive small-model oracle.
3. **A bounded, budgeted search** (§9): every script with at most `k` deviations, enumerated once, each run forked from a checkpoint, deterministic for any thread count.
4. **Schedule coverage, coverage-guided order, and minimization** (§11–§13), checked against planted bugs whose windows are one instruction wide or need a tie choice (§10).

## 2. Scope and Non-goals

### 2.1 In Scope

- scripted schedules: picks, message delays, and shifts of asynchronous wakes, named by event identities (§7), in `perturbation.bin`, snapshots, and reproducers;
- the search domain, its branch points, and the fork operation (§8, §9.3);
- the bounded search, deterministic rounds, budgets, and resumable search state (§9);
- planted order-dependent bugs R6–R8, each with a hand-authored failing script, and a measured comparison with M5's seeded campaigns (§10);
- schedule coverage and coverage-guided order (§11);
- schedule minimization and M4 reproducers (§13–§14);
- the search on `m5-reference`, on `m2-reference` and `m3-reference` with their own judges, and exhaustively on small synthetic topologies against an independent oracle (§17).

### 2.2 Non-goals

- **No change to M0–M5 behavior.** A session without a script is byte-identical to its golden files (§15).
- **No multiple harts.** The system stays the single-core M5 system. Atomics, inter-processor interrupts, and memory-model relaxation are plan.md §13 candidates after M9.
- **No pruning of the search domain.** Component-level conflicts order the search and its coverage, and remove nothing (§8.3). Address-level dependencies are not planned.
- **No convergence cut.** Every run goes to its stop rule (§9.5).
- **No new comparison engine, profile, or stream.** M6 reuses `m5.outcome`, the `schedule` stream, M4's locator, and M5's evidence.
- **No completeness claim beyond the domain.** "Exhausted" always names its bounds and domain, and a budget-limited search reports its frontier (§9.1).
- **No wall-clock budgets.** A budget counts runs and events, so a search's result never depends on host speed (§9.6).
- **No Perfetto export or UI.** M7 shows M6 schedules.

## 3. Frozen M6 Decisions

| # | Decision | Section |
|---|---|---|
| D1 | A schedule is a perturbation: a script of deviations from the reference ordering, encoded in `perturbation.bin` and replayed like any other perturbation | §7 |
| D2 | Three kinds of deviation only: a **pick** (at one dispatch, another ready event of the current `(tick, phase)` than the one the scheduler would dispatch), a **delay** (one message delivered `1..=D` cycles later), and a **shift** (one asynchronous wake dispatched `1..=E` cycles earlier or later). Each counts as one deviation | §7.2, §7.3, §7.6 |
| D3 | Deviations name events by tagged identities: `Msg(link, dir, ordinal)` and `Wake(component, ordinal)`. An identity means "the `n`-th such event in this run", whatever its payload. An entry whose identity does not occur is vacuous, and the run is still compared | §7.1, §9.2 |
| D4 | The domain is exact: every admissible order of each tie group, as a sequence of picks, every delay of `1..=D` cycles of a scoped message, and every shift of `±1..=E` cycles of an asynchronous wake. Admissible means causal and FIFO per directed lane, and a scripted session enforces both on every event it dispatches. Nothing is pruned | §8 |
| D5 | Every deviation has a branch point: the earliest scheduling or dispatch it changes. A child is forked from a checkpoint of its parent strictly before that point | §8.4, §9.3 |
| D6 | The search enumerates every script within `k` deviations exactly once: a script's parent is the script without its deviation of latest branch point | §9.2 |
| D7 | The search is a pure function of the scenario, the bounds, the budget, and the batch size `B`. It advances in rounds of `B` runs, so its result is the same for every thread count and on every host, and its state can be saved and resumed | §9.6 |
| D8 | Coverage orders the search but never gates it. An exhausted search explores the same set in any order | §11 |
| D9 | Planted bugs R6–R8 each have a hand-authored failing script before the bounds are frozen. Each is measured against M5's seeded campaigns, and the comparison is reported, not judged | §10 |
| D10 | Minimization reaches a fixed point: no deviation can be removed, and no delay or shift shortened without losing the failure | §13 |
| D11 | Scripts are exclusive with seeded jitter (B) and ties (C), and compose with latency and slice overrides (A), which define the reference | §7.5 |
| D12 | The contracts pin does not move. `EventKey` and every contracts type are unchanged | §18 |

## 4. What M0–M5 Provide

- **Deterministic DES (M0).** Events are ordered by `(tick, phase, sequence)`. Components interact only by messages on links and their own self-wakes, but they share the scheduler: every scheduling takes the next global counter (rule S6), and may target the current `(tick, phase)` (rule S2). A session snapshots and restores at every event.
- **Verification (M4).** Profiles, the checkpoint locator, state and trace diffs, slices, reproducer bundles, and minimization to a 1-minimal set that keeps the failure signature (m4-design §17).
- **Timing exploration (M5).**
  - Tie order C: a rank-major `sequence = rank << 40 | counter` (`runtime/src/ties.rs`). The rank is fixed when the event is scheduled. An event scheduled into the `(tick, phase)` being dispatched takes a rank at or above the current one, so the scheduler still dispatches in strict key order.
  - Jitter B: per-message delays on FIFO lanes, one lane per direction of a link, with a per-lane ordinal, clamped to the lane's latest delivery tick (`runtime/src/jitter.rs`). A listed jitter (`Jitter::only`) delays only the messages it lists. B clamps ticks, but C ranks each event on its own, so B and C together do not keep dispatch FIFO within a tick; M6 must (§8.2).
  - `m5-reference` with its preemptive kernel, the `outcome` and `schedule` streams, `m5.outcome`, planted races R1–R3 and R5, minimization, coverage reports, and `perturbation.bin` in reproducers.
  - **C changed no result** on any reference workload (m5-design §7.3). M6 measures why before it relies on tie choices: how many tie groups hold events whose order a handler can observe (§8.3).

## 5. Terminology

| Term | Meaning |
|---|---|
| **Reference** | The run of the scenario, with its A overrides if any, under the empty script |
| **Event identity** | A tagged name for an event that does not depend on `sequence` (§7.1) |
| **Tie group** | The events a run dispatches with one `(tick, phase)` |
| **Deviation** | A pick, a delay, or a shift (§7) |
| **Asynchronous wake** | A self-wake of a component in the asynchronous set, whose timing models a clock outside the system, such as a timer expiry (§7.6) |
| **Script** | A canonical set of deviations, at most one per dispatch, message, or wake. The empty script is the reference |
| **Branch point** | The earliest scheduling or dispatch a deviation changes (§8.4) |
| **Fork** | Restoring a parent's checkpoint before a branch point and attaching a child's script (§9.3) |
| **Bounds** | `k`, `D`, the delay scope, `E`, and the asynchronous set (§9.1) |
| **Budget** | A count of runs, and an event limit per run (§9.6) |
| **Exhausted** | Every script within the bounds was run and compared, or classified a duplicate of one that was (§9.2) |
| **Vacuous entry** | A script entry whose identity does not occur in the run; the run is still compared (§7.1) |
| **Default event** | The event the scheduler would dispatch next: the pending event of the next `(tick, phase)` with the least counter (§7.2) |
| **Pick** | At one dispatch, a ready event other than the default dispatched in its place (§7.2) |
| **Witness** | A hand-authored script that makes a planted bug fail (§10) |

## 6. Comparison Model

Each explored script is compared with the reference on `m5.outcome`, as an M5 perturbed run is compared with the unperturbed one. Both sides are SystemScope, and every M4 and M5 piece of evidence applies: the outcome divergence, the first differing schedule decision, the per-process trace, and the shared-page reads.

`m2-reference` and `m3-reference` are not compared on `outcome`. Under the search, each of their runs must pass the scenario's own judge, as under M5's B and C.

## 7. Scripted Schedules

### 7.1 Event Identity (D3)

`sequence` cannot name an event across schedules: a deviation changes the order of later schedulings, and with them every later counter. M6 names events by what scheduled them:

- `Msg(link, dir, ordinal)`: the `n`-th message accepted for one direction of a link;
- `Wake(component, ordinal)`: the `n`-th self-wake a component scheduled, including those it scheduled from its `Init` callback. `Init` is a callback, not a queued event, and has no identity.

The two namespaces are tagged, so no message and wake share an identity. A counter advances only when a scheduling is accepted. A failed scheduling faults the session, so its count is never observed.

An identity means "the `n`-th such event in this run", whatever its payload. A deviation applies to whichever event holds its identity in the child run. After an earlier deviation changes a sender's behavior, the same identity may name a different payload; the deviation still applies to it, and that is deterministic. An entry whose identity the run never produces is **vacuous**: it changes nothing, and the run is still compared (§9.2).

A scripted session keeps every directed link's count, every component's wake count, and every queued event's identity as runtime state, stored in its snapshot (§15). A session without a script keeps none of it.

### 7.2 Picks

A pick dispatches, at one dispatch, another event of the current `(tick, phase)` than the scheduler would. A scripted session schedules every event with its plain counter as its sequence, so the **default event** of a dispatch is the pending event of the next `(tick, phase)` with the least counter, the event M0–M4 dispatch. A pick names the default event `Y` and the event `X` dispatched in its place. `X` must be **ready**: pending in the same `(tick, phase)`, with no earlier message of its lane pending (§8.2). An event scheduled during the group is pending only once the event that scheduled it has run, so a pick is causal by construction.

`Y` stays the default after a pick, since every event scheduled later takes a larger counter. So several picks can come ahead of one default event, at successive dispatches. A script lists them per default event, in dispatch order, `Y: [X1, X2, …]`, and each is one deviation. The `c`-th entry, counted from 0, applies at the `c`-th dispatch while `Y` is the default. An entry whose `X` is not ready there, or whose `Y` is never the default, is **vacuous**: the default is dispatched, and the entries after it in its list are vacuous too.

Picks decide an order one dispatch at a time, so they also order the events that a reordering itself creates. For example, at one component, A sets a flag and B schedules a wake W into the group unless the flag is set. The parent order is A, B, and has no W. A pick of B ahead of A schedules W, and the child's next dispatch offers A, its default, and W. With no further pick the child dispatches B, A, W, and a pick of W ahead of A gives B, W, A. Ranks fixed when events are scheduled, as revision 3's group orders were, reach only one of the two (§23.5).

A run's picks are read off its dispatches: at each dispatch, the event dispatched and the default. So a run's order of every group comes from exactly one canonical list of picks, and a script without vacuous entries is that list. A run whose script holds a vacuous entry records it, and the evidence shows it (§12). It is still run to its stop rule and compared.

### 7.3 Delays

A delay delivers one message `d` cycles later than its parent run, `1 ≤ d ≤ D`. It is carried by jitter's mechanism: the script lists `(identity, d)`, and the runtime adds `d` cycles in `Ctx::try_send`, as it adds a drawn jitter. The lane stays FIFO: the delivery tick is `max(tick + d, the lane's latest tick)`, as in M5.

`D` bounds the requested cycles, not the effective displacement. When the lane's clamp makes two delays of one message deliver at the same tick, the later one is a **duplicate** (§9.2). Delays apply only to `Cycles` links in the scope, as jitter does.

### 7.4 Encoding and Identity (D1)

`perturbation.bin` gains a script: tag 3 of the jitter field holds the delays, ascending by message, then the shifts, ascending by wake; tag 2 of the tie field holds the picks, one list per default event, ascending by the default event's identity. Each tag requires the other. A schedule's identity is its encoding's BLAKE3, as for any perturbation, and it is part of the session identity.

A scripted session's snapshot is format version 5: version 1 with the script, its identity counts, and the picks taken ahead of the current default event after the topology hash, and each queued event followed by its identity.

A scripted session with the empty script is not an unscripted session: it keeps the counts and identities of §7.1, and writes the scripted snapshot format. Its trace, every stream, its execution digest, and its component states equal the unscripted run's, which a test checks on every reference platform (§17); only its state digest, which hashes the snapshot, differs. The reference that the search forks from is this run, so every M0–M5 golden snapshot, which comes from an unscripted session, is unchanged.

### 7.5 Composition with M5 Perturbations (D11)

- **A (latency and slice overrides)** are builder inputs. They compose with a script: the reference is the run under the overrides, and the search explores around it. A campaign may give the search several references, one per override set, to place preemptions where the empty override set does not.
- **B and C** share runtime mechanisms with a script: B its delays, and C the scheduler its picks select from. A session with a script and a seeded jitter or tie seed fails construction. Every M5 perturbation decodes and runs as before.

### 7.6 Shifts of Asynchronous Wakes

A shift dispatches one asynchronous wake `s` cycles away from where its parent run dispatches it, `1 ≤ |s| ≤ E`: earlier for a negative `s`, later for a positive one. The script lists `(identity, s)`, and the runtime applies it when the wake is scheduled, as it applies a delay when a message is sent: a wake scheduled `k` cycles of a clock domain ahead is scheduled `k + s` cycles ahead instead. The wake keeps its phase and token. An earlier wake goes no earlier than 0 cycles, the domain's next edge, and if that edge is the current tick and the wake's phase is before the current one, to the next tick (rules S1 and S2), so a shift is always causal. A shifted wake scheduled other than in cycles faults the run.

The **asynchronous set** names the components whose wakes model a clock outside the system. It is a bound, as the delay scope is (§9.1): on `m5-reference` it is the timer, whose wakes are scheduled in cycles. A component outside it is never shifted.

Shifts are what reach a preemption at one instruction. A delay moves an event only after the event that sends it, and on `m5-reference` the interrupt that preempts a process is the timer's expiry, a self-wake that no delay moves. A shift moves the expiry itself. The timer is periodic and re-arms from each expiry, so a shift moves that expiry and every later one by `s`, as a timer started at another phase would. With `E` at least the slice, every instruction boundary of a slice is some shift's preemption point.

## 8. Search Domain (D4)

### 8.1 Choice Points

A run's choice points are:

- each dispatch with a ready event other than its default, with each such event;
- each message on a scoped `Cycles` link, with the delays `1..=D`;
- each wake of a component in the asynchronous set, with the shifts `±1..=E`.

A child run has its own choice points after its last branch point, found from its own trace.

### 8.2 Admissible Orders

An order of a tie group is admissible when it keeps two kinds of edge:

- **causal:** an event scheduled during the group comes after the event that scheduled it;
- **FIFO:** two messages of one directed lane come in the order they were sent. `mem.v1` and `irq.v0` assume in-order delivery (m5-design §7.2).

Self-wakes have no FIFO edge to each other or to messages. An order of a group keeps both edges exactly when each of its events is ready at its dispatch, so the sequences of picks reach exactly the admissible orders, including those of events a pick adds. A group of `n` unconstrained events has `n!` orders, and its reversal takes `n - 1` picks. M6.2 measures the picks each dispatch offers on the reference platforms before any bound is frozen (§9.1).

The legacy C mechanism is unchanged: a session with a tie seed keeps M5's behavior, which does not keep these edges.

### 8.3 No Pruning

Two events delivered to different components do not commute in this runtime. Each handler schedules through the shared scheduler, so the order of two handlers decides the counters of what they schedule, and with them the order of later ties. When both schedule into the current `(tick, phase)` (rule S2), the reversal they cause may also be one that no single later deviation reproduces. So destination equality is not an independence proof, and M6 explores every admissible order.

Component-level conflicts still matter as a measure: a group whose events all reach different components is less likely to change a result than one with two events at one component. M6 uses that for coverage (§11) and to order the search (§11.2), never to remove anything.

A reduction may be added later only as a separate, named domain, with an independence theorem for this runtime and an exhaustive small-model oracle that it passes (§17). Ordering untied events by identity instead of counter is a candidate route, because it would make the order of later ties independent of earlier handlers (O7).

### 8.4 Branch Points (D5)

A deviation's branch point is the earliest scheduling or dispatch it changes:

- for a delay, the acceptance of the delayed message's send;
- for a shift, the scheduling of the shifted wake;
- for a pick, its dispatch.

Branch points are totally ordered by position, then kind (a pick before a delay or a shift, since a dispatch precedes what its handler schedules), then identity: the default event for a pick. A delay and a shift never share an identity, since messages and wakes are tagged (§7.1). Picks of different events at one dispatch share a branch point, and are siblings: no script holds two. A child's deviations take effect only at or after their branch points, so its run equals its parent's before its new branch point.

## 9. Search

### 9.1 Bounds and Domain Claims

| Bound | Meaning |
|---|---|
| `k` | the most deviations in one script |
| `D` | the most cycles per delay |
| scope | the links whose messages may be delayed, as jitter's scope: device, DMA, and interrupt links by default |
| `E` | the most cycles per shift |
| asynchronous set | the components whose wakes may be shifted: the timer on `m5-reference` (§7.6) |

A search **exhausts** its bounds when every script within them has been run and compared, or classified a duplicate. That claim covers this domain and nothing else: no other delay or shift, no address-level ordering, and no ordering the model does not admit. A budget-limited search reports the frontier it left instead.

The numerical bounds are frozen after the witnesses (§10) and the measured costs (§9.7) are known. Full exhaustion of `m5-reference` is required only where it is measured to fit. Exhaustion is always required on the small synthetic topologies (§17).

### 9.2 Enumeration (D6)

The search explores scripts by deviation count, from 0 to `k`. From a parent run, it extends the parent's script by one deviation at a choice point whose branch point is after the parent's last one. So a script's parent is the script without its deviation of latest branch point, and each script is enumerated once.

Every child is run to its stop rule and compared, with one exception: a **duplicate**, whose effective schedule is identical to its parent's or an earlier sibling's, and so whose run is identical too. Its result is that run's, so no comparison is lost. Duplicates are:

- a delay whose effective delivery tick equals its parent's or a sibling's with a smaller `d`, as the lane's clamp can make it;
- a shift whose effective dispatch point equals its parent's or a sibling's of the same sign and a smaller `|s|`, as the raise of §7.6 can make it.

Two picks never duplicate each other or their parent: each dispatches another event at its branch point.

A duplicate is reported and not extended. A vacuous entry (§7.1) never stops a run or its comparison: it is counted, and the run's other deviations take effect as usual.

Every other child is extended whatever its comparison: the enumeration never reads a result. R7 needs it, since its witness diverges only with both of its shifts, and either alone agrees (§10). A run's choice points are those of the events it dispatches up to its stop rule. A run that faults the session or reaches its event limit ends there, and is extended only at the choice points of the events it dispatched.

### 9.3 Fork (D5)

A child differs from its parent only from its new branch point. So the search restores the parent's latest checkpoint strictly before that branch point and attaches the child's script, instead of running from the start.

Fork is an internal operation, distinct from restore:
- it checks that the checkpoint is the parent's, by the parent script's identity and the checkpoint's event index;
- it keeps the checkpoint's queue, counts, and identities;
- it attaches the child script, whose only entry beyond the parent's has a branch point after the checkpoint.

Ordinary restore (m0-design §7) is unchanged and still requires the session's own script. A child's checkpoints are its own: a reproducer's checkpoint is derived by replaying the child from its initial state, as M4 requires (m4-design §16).

### 9.4 Checkpoints

The search takes a checkpoint at the first event boundary at or after every multiple of a fixed tick interval, in every run it may extend. Their memory is counted in encoded snapshot bytes, and a fixed budget is kept by a deterministic eviction rule: the checkpoint farthest from any pending branch point goes first, ties by run and position. Replay work that eviction causes is physical cost, never counted in the budget.

Every event before such a checkpoint is at an earlier tick, so no `(tick, phase)` straddles it, and each event scheduled before it has a branch position at or before it. A child forked there is recorded from the checkpoint on (`Recording::after_checkpoint`): its choice points after its branch point are the whole run's, since none of the events before can deviate after it. Its observations before the checkpoint are its parent's, which the prefix digests check, so the driver keeps a parent's observations while children remain to be forked from it, and compares the child's run with the parent's prefix put in front. A checkpoint inside a group would not do: the recording would miss the group's earlier dispatches, which decide what is ready at its later ones. The oracle forks every reached script from each of its parent's tick boundaries before its branch point (§17).

### 9.5 Every Run Goes to Its Stop Rule

M6 does not cut a run short when its state seems to rejoin the reference. Deciding that soundly needs every runtime and projection state that decides the future and the comparison: the queue order and the picks taken, the RNG states, the FIFO lanes, the counts, the remaining budget, and the `outcome` projection's held observations, and it must leave room for the child's later choice points. If measurements show the cost needs it, a convergence cut comes as a design revision with that definition and its own oracle check.

### 9.6 Rounds, Budgets, and Resumption (D7)

- **Rounds.** A round takes the next `B` scripts from the frontier in §11.2's order, runs them on any number of host threads, and merges their results in frontier order. `B` is a search parameter, independent of the thread count, and recorded in the report.
- **Budgets.** Each run has its own event limit, the backend's, as in M5. A run that reaches it is reported as truncated. The global budget counts runs and is checked between rounds, so a round always completes, and the overshoot is at most `B - 1` runs, reported.
- **Failures.** A run that faults the session is `BackendFailed`, a finding of its own (§16). It never stops the search.
- **Resumption.** The search state holds the frontier, the explored scripts and their classes, the coverage history, the round cursor, and the remaining budget. It is serialized at round boundaries, and a resumed search produces the same report as an uninterrupted one.

### 9.7 Feasibility Gate

Before any bound is frozen, M6.2 measures on `m5-reference` and its races:
- the scoped sends, the tie groups, and the picks each dispatch offers;
- the distinct effective delays per message within candidate `D`, and the asynchronous wakes;
- the events after each branch point, and the checkpoint and trace bytes.

Measured at M6.2 on the reference run under the empty script (`tests/acceptance/tests/m6_feasibility.rs`): 1,332,749 events in about 0.6 seconds of a release build, a final snapshot of 146,862 bytes, and 86,626 scoped sends, 81,090 of them on link 4 (bus to `blk`'s registers). 5,371 dispatches offer a pick, each exactly one other event. No `(tick, phase)` holds both a self-wake of a component and a message to it. Every scoped message runs at a later tick than its lane's previous message, so no lane clamps one: its base tick is the tick it runs at, and each delay of `1..=D` puts it at a distinct later tick, `D` effective delays per message and no duplicate. The timer's 99 wakes are each scheduled 80,000 cycles ahead, so no shift with `E` below that is raised, and each of `±1..=E` gives a distinct tick. The branch points lie early in the run. The events after a delay's branch point are 1,161,410 on average (172 to 1,332,575), 1.006 × 10^11 over all 86,626; after a pick's, 1,132,857 on average, 6.08 × 10^9 over all 5,371; after a timer wake's, 4.97 × 10^7 over all 99. At about 2.26 million events per second, running every depth-one child to its end takes about 12.4 hours of one core per unit of `D`, 45 minutes for the picks, and 22 seconds per shift value. Snapshots taken every 100,000 events are 19,079 to 147,424 bytes. The run's trace is 1,594,037 records, 242,690,394 bytes in its canonical encoding.

Measured again to the stop rule of `m5.outcome`, the CPU's trap taken in M or its instruction limit, on the unpatched run and on every race (`measure_every_races_choice_points`). The unpatched run and R6–R8 stop at event 1,332,748, one before the queue drains; R1 at 1,335,307, R2 at 1,328,788, R3 at 1,355,393, and R5, whose kernel never shuts down, at its instruction limit at event 1,473,536. Each run offers 5,371 to 5,410 picks, each of exactly one other event, 86,626 to 86,722 scoped sends, none clamped by its lane, and 98 to 111 timer wakes, each scheduled 80,000 cycles ahead. R5's are the largest sums of events after branch points: 1.128 × 10^11 over its delays, 6.84 × 10^9 over its picks, and 6.45 × 10^7 over its timer wakes.

Every single pick of each run was then judged against its unperturbed run under `m5.outcome` (`judge_every_single_pick`, about 21 minutes per run on 10 threads). No pick changes `outcome` on the unpatched run or on R1–R7. R8 diverges under 358 of its 5,371 picks, among them picks between events at components other than the bus, which change when its requests arrive. So at depth one, picks find only R8 (§19, risk 3).

For one illustration, 1,000 scoped messages with `D = 2` make 2,000 depth-one scripts, about 50 minutes at M5's 1.5 seconds per full run, and depth two makes two million. Forks save the prefix, not the count. So CI exhausts small bounds on synthetic topologies, and searches `m5-reference` with a fixed budget unless its exhaustion is measured to fit.

### 9.8 Placement (O3)

The search is split by what each part depends on:
- **The search tree, in `systemscope-runtime`** beside `runtime::choice` (`runtime::search`): a child's script from its parent's and a choice point, the classification of duplicates (§9.2), and fork (§9.3). It reads the scripted session and its recording only. The runtime classifies a delay or shift when it applies it, by the rule that applies it: a delay's tick, `max(t + d, last)` with the lane's last tick, grows with `d`, so it is a duplicate exactly when it equals the undelayed tick; a shift's tick moves with `s` and stops at an earlier wake's floor, so it is a duplicate exactly when it equals the tick one cycle nearer gives. A duplicate then costs a fork up to its branch point, not a run.
- **The driver, in `systemscope-verify`** beside the M4 engine: the frontier, rounds, budgets, the checkpoint store and its eviction, resumption, coverage, and the report. It supplies the three things the tree does not know: where a run stops, from the profile's stop rule; the score of §11.2, from the run's trace; and each run's comparison. M7 reads the search's results from this library.
- **The campaigns, in `systemscope-acceptance`** with M5's: the subjects, the races and their fixtures, and each campaign's bounds.

Dependencies run one way, from acceptance to verification to the runtime. The oracle (§17) checks the tree on synthetic topologies with neither a profile nor a comparison.

## 10. Planted Order-Dependent Bugs (D9)

M5's races have wide windows: each diverged for 14 to 32 of 32 seeds under A or B (m5-design §17). M6 adds races with narrow windows. Each is an input variant of `m5-reference` with its correct counterpart, found by instruction pattern as in M5.

| Variant | Bug | What a failing schedule needs |
|---|---|---|
| R6 `handoff-flag-first` | `handoff` sets the mailbox's flag before it stores the data | a preemption in the one instruction between the two stores, while `spinner` then polls the mailbox before `handoff` runs again: a shift of a timer expiry. M5's seeds never hit it (m5-design §27.7) |
| R7 `peterson-order` | `counter`'s entry stores `turn` before its own `flag`, the textbook ordering bug; both instances run the patched program | one instance preempted between its `turn` and `flag` stores; the other enters the critical section and is preempted between its counter load and store; the first resumes, enters too, and increments; the second stores a stale count: two shifts. M5's seeds never hit it |
| R8 `tie-defect` | a test-only defect flag in the bus (`BusDefect::WriteBehindLaterMaster`): a write that arrives while a later master has a request queued is queued with its first byte inverted. The correct bus never depends on the order of requests within one `Request` phase (`components/platform/src/mmbus.rs`) | a pick of the kernel's request (link 10) ahead of the DMA's write (link 5) in one `Request` phase. Neither is a self-wake. Link 10 is outside the delay scope, and link 5 inside it |

R6 replaces the first draft's DMA race, which cannot occur: the kernel commands only the head of its request queue and issues the next command after completing the current one, so two completions are never outstanding (`components/os/src/interrupts.rs`). R8 is a defect flag, appended to the bus's snapshot only when set, so every golden is unchanged. Its device and transition were frozen at M6.2 from the reference run (§9.7, O4). No `(tick, phase)` there holds a self-wake of a component and a message to it, the first draft's shape. The only pairs of events at one component that some order of their group puts either way round are three at the bus, 384 times each, in one repeating sequence: the DMA's write and the kernel's request in one `Request`, the two arbitration wakes they schedule in `Transfer`, and the two responses in a later `Complete`. The wakes carry one token and one state, so no defect can observe their order, and the responses carry no data a defect could corrupt on the DMA's side. So R8 is planted in the requests: in the reference order the DMA's write arrives first, and the defect never fires.

That R8's two events are not moved directly does not mean no delay or shift can reorder them: moving another event can move when either is scheduled. So "only a pick finds R8" is a measured claim, not a construction. Link 5 is in the delay scope, so a delay of the DMA's write itself is among the scripts checked. Once the CI bounds are frozen (O1), every script of delays and shifts only within them runs on R8, and none may fail. If that domain within the CI bounds does not fit, the claim is stated for the scripts that ran, with the budget, and not beyond.

Before the bounds are frozen, each race has a **witness**: a hand-authored script, pinned with its fixtures, that fails with a stated `outcome` divergence, using the identities of §7.1. The witness shows the race is reachable in the domain, and the bounds are then chosen to include it. A race whose witness needs a reference with A overrides (§7.5) states them.

For every race, R1–R3 and R5–R8, the report gives:
- the search's runs to its first finding, and the bounds and reference that found it;
- M5's seeded campaign over the same races: how many of seeds 0–31 diverge under A and B.

The comparison is a report. A gate on it would make M6's exit depend on M5's seeds missing something, and M5 missing a race is evidence of a narrow window, not proof.

## 11. Schedule Coverage (D8)

### 11.1 Report

Coverage is a reviewed JSON file, blessed and verified byte for byte as M5's reports are (m5-design §17). For each subject, reference, and bounds:

- **Choice points:** tie groups by size, dispatches by the picks they offer, and scoped messages, by target component.
- **Conflicting pairs:** for each component, each pair of incoming sources (a directed link or the component's own wakes) that ever share a tie group or come within `D` cycles of each other, and whether each order of the pair was explored.
- **Preemption points:** per process (PID), as in M5, the distinct `sepc` of its `preempt` decisions across every explored script.
- **Scripts:** run, duplicate, agreed, diverged, truncated, and failed, and those with vacuous entries; the distinct `schedule` streams by BLAKE3; and whether the bounds were exhausted, or the frontier left.

### 11.2 Coverage-Guided Order

Within a deviation count, the search ranks choice points by a score, then by branch point (§8.4). The score counts the conflicting-pair orders and preemption points the choice point could add that the merged results have not covered, read from the trace of the run it extends. Its inputs and total tie-break are frozen with the search at M6.3. Its weights may be tuned at M6.5, and they are versioned and recorded in the report, so a budget-limited report is reproducible (O5).

## 12. Evidence

A finding gets M5's evidence (m5-design §15): the outcome divergence, the first differing schedule decision, the per-process trace, and the shared-page reads. It also names each deviation of its script: the identity, the kind, the branch point, and, for a pick, its default and dispatched events, with any vacuous entry.

## 13. Minimization (D10)

A failing script has at most `k` deviations. Minimization repeats, in a fixed canonical order, until nothing changes:
1. remove each deviation;
2. shorten each delay or shift, trying every smaller `d` or `|s|` of the same sign.

A candidate is kept if it fails with M4's failure signature (m4-design §17.2). Vacuous entries are dropped after each candidate. Each candidate is a full deterministic run, forked when a parent checkpoint precedes its branch point.

The result is 1-minimal under those two operations: a final check confirms that every removal and every shorter delay or shift loses the failure. It is not claimed to be globally smallest.

## 14. Reproducer

The minimized script is the side's `perturbation.bin` (§7.4), so the M4 bundle and its format version are unchanged (m5-design §18). Every M4 and M5 bundle and corpus entry stays valid. Each reproducer replays from the initial state, and each race's reproducers are a pure function of the committed fixtures and equal byte for byte on Linux and Windows, as M5's are.

## 15. Determinism and Compatibility

- **Identity.** A session without a script takes the M0–M5 path. Every M0–M5 golden, expected stream, corpus entry, coverage report, and reproducer is byte-identical, and CI keeps all their jobs.
- **Snapshots.** A scripted session writes a new snapshot format version with its script, counts, and queued identities (O2), and restores at every event. `m5-reference` gets the every-event resume check under each race's witness.
- **Cross-OS and threads.** Search reports, coverage reports, findings, minimized scripts, and reproducers are byte-identical on Linux and Windows, and for the worker counts CI runs (1, 2, and 4).
- **Observation.** Observing a scripted run does not change it (m4-design §28.2).

## 16. Failure Model

- An invalid script fails construction: an undeclared link or component, a delay out of scope, on a non-`Cycles` link, or out of `1..=D`, a shift of a component outside the asynchronous set or out of `±1..=E`, a duplicate identity, a pick that names its own default or one event twice, or a script combined with B or C. The search never produces one.
- A pick whose event is not ready in the child never fails: it is vacuous, and the default is dispatched (§7.2).
- A scripted run that faults the session is a `BackendFailed` finding with its script, as in M5. It is a diagnostic, not a detection: a planted bug passes the detection criterion only through its pinned `outcome` divergence, including a `limit` outcome where its run never shuts down (m5-design §20).

## 17. Validation

- **Oracle.** On small synthetic topologies, including the generated-event diamond of §8.3 (two events at different components whose zero-latency sends reach a third in the current phase) the conditional wake of §7.2 (a reordering that removes or adds an event of its group), and a periodic asynchronous wake that preempts a component between two steps (§7.6), an independent enumerator on a naive model of the scheduler (`runtime/tests/oracle.rs`, after the reference model of `runtime/tests/scheduler_props.rs`) lists every run within small bounds. At each pop it tries every ready event of the next `(tick, phase)`, at each scoped send every delay of `0..=D`, and at each asynchronous wake every shift of `-E..=E`, and it counts each pick, nonzero delay, and nonzero shift as one deviation. The scripts of the search tree (`runtime::search`) other than duplicates must reach exactly the enumerator's distinct dispatch orders within `k`, each by one script, and the runtime's classification of each duplicate must equal one by effective tick from the children's full runs. The enumerator shares no code with the choice-point finder, the search tree, or the scripted session.
- **Scripts.** Encoding round-trips. A scripted session with the empty script produces the unscripted run's trace and streams. A pick dispatches only a ready event, which keeps causal and FIFO edges on one lane, on two lanes, at equal ticks, and for events a pick adds, and an entry that is not ready is vacuous with the rest of its list. A run with a vacuous entry is compared. A delay is FIFO per lane, a shift is raised to the earliest admissible point and moves a periodic wake's later expiries with it, and duplicate classification is exact: a duplicate's run, run anyway, equals the run it duplicates.
- **Fork.** For every script of the oracle's bounds, and for each witness, a forked child's run, report, and snapshots equal the child run from its initial state. In the oracle, a child forked from each of its parent's tick boundaries before its branch point and recorded from there has the children it has from init.
- **Rounds and resumption.** A search at 1, 2, and 4 workers, with a budget that expires mid-round, and interrupted and resumed at each round boundary, produces one report.
- **Minimization.** Each result passes the final neighborhood check of §13.
- **Planted bugs.** Each witness fails as pinned. R8 fails under no script of delays and shifts only within the CI bounds, or within the stated budget (§10). Every race is found within the CI bounds or budget, and the unpatched scenario agrees on every script the CI search runs.
- **Mutation.** Source mutants of the script layer, identities, shifts, admissibility, enumeration, fork, rounds, and minimization are judged against committed expectations: the oracle's orderings, each race's pinned finding and minimized script, and the unpatched scenario's script count within the CI bounds. Each mutant is detected or listed as equivalent with its reason.

## 18. Contracts (D12)

Picks, delays, shifts, and identities are runtime-internal, like C and B. Identities are computed by the runtime from links and components it already has. `EventKey`, `TraceHeader`, and every contracts type are unchanged, and the contracts pin does not move.

## 19. Architecture Risks

1. **State explosion.** With no pruning and no convergence cut, scripts grow with the run and with `k`. A pick decides one dispatch, so reversing a group of `n` events takes `n - 1` deviations. Mitigation: the feasibility gate (§9.7), forks, budgets with a reported frontier, coverage-guided order, and exhaustion required only where it is measured to fit.
2. **A narrow race outside the bounds.** R6 and R7 need a preemption at one instruction. Delays cannot place one: the window sits at a fixed phase after the expiry that dispatched the process (§23.4). Mitigation: shifts of the timer's expiries (§7.6), which reach every instruction boundary of a slice when `E` is at least the slice, and witnesses before the bounds are frozen. The cost is a domain of `2E` shifts per expiry, searched with a budget on `m5-reference` (§9.7).
3. **Tie choices stay ineffective.** If no pick changes a result on the reference workloads, picks find only R8. Mitigation: M6.2 measures it, and R8 checks that a pick can find a race at all. Measured: at depth one, no pick changes `outcome` on the unpatched run or on R1–R7, and 358 picks find R8 (§9.7).
4. **Bookkeeping costs time.** A scripted session counts every link and wake and keeps identities. Mitigation: only scripted sessions do, and M6.1 measures the cost.

## 20. Open Questions

| # | Question | Decided at |
|---|---|---|
| O1 | The numerical bounds `k`, `D`, scope, `E`, the asynchronous set, `B`, budgets, checkpoint interval and memory, and the references (A overrides) of each campaign, from the witnesses and the feasibility gate | M6.3 **(open)** |
| O2 | The scripted session's snapshot format version and byte layout | M6.1: version 5 (§7.4) |
| O3 | Where the search lives: a library module beside the M4 engine, since M7 will display its results, or the acceptance crate with M5's campaigns | M6.3: split by dependency, the tree in the runtime, the driver in verification, and the campaigns in acceptance (§9.8) |
| O4 | R8's device and transition | M6.2: the bus's request queueing (§10) |
| O5 | The coverage score's weights (§11.2) | M6.5 **(open)** |
| O6 | Whether full exhaustion of `m5-reference` within some bounds fits a weekly job | M6.3 **(open)** |
| O7 | Whether a sound reduction (§8.3) or a convergence cut (§9.5) is worth a design revision | M6.8 exit audit **(open)** |

## 21. Roadmap

| Step | Status | Scope | Changes |
|---|---|---|---|
| **M6.0** | complete | Design freeze (this document) | docs |
| **M6.1** | complete | Scripted schedules: identities and counts, scripted ranks and delays, admissibility, the `perturbation.bin` tags, the snapshot format (O2), fork; the empty script checked against the unscripted run; identity proven on every M0–M5 golden | code, tests |
| **M6.2** | complete | Shifts (§7.6) in scripts, `perturbation.bin`, and snapshots; picks in place of M6.1's group orders (§7.2); choice points, the independent oracle on synthetic topologies, the feasibility gate (§9.7); R6–R8 fixtures and their witnesses (O4) | code, tests, fixtures |
| **M6.3** | in progress | The search: enumeration, classification, rounds, budgets, checkpoints, resumption; bounds frozen (O1, O3, O6) | code, CI |
| **M6.4** | planned | Campaigns: every race found within the CI bounds or budget, the unpatched scenario agreeing on every run, findings pinned, the comparison with M5's campaigns | code, CI |
| **M6.5** | planned | Schedule coverage and coverage-guided order; the report blessed (O5) | code, CI |
| **M6.6** | planned | Schedule minimization, evidence, and cross-OS reproducers | code, CI |
| **M6.7** | planned | Source mutants of the M6 paths; the weekly search | code, CI |
| **M6.8** | planned | Exit audit (O7) | docs |

## 22. M6 Exit Criteria

- [ ] **Identity.** Without a script, every M0–M5 golden, expected stream, corpus entry, coverage report, and reproducer is byte-identical, and every M0–M5 CI job passes.
- [ ] **Exactness.** On the synthetic topologies, the search exhausts its bounds and covers exactly the oracle's orderings, each once.
- [ ] **Determinism.** Every search report, coverage report, finding, minimized script, and reproducer is byte-identical on Linux and Windows and at 1, 2, and 4 workers, and a resumed search reports as an uninterrupted one. Every forked child equals its run from the initial state.
- [ ] **Detection.** Every planted order-dependent bug, R1–R3 and R5–R8, is found within the stated CI bounds or budget, through its pinned `outcome` divergence, and located by M4.
- [ ] **No false positives.** The unpatched `m5-reference` agrees on `m5.outcome` on every script the CI search runs, and `m2-` and `m3-reference` pass their judges on every script of theirs. Each report states whether its bounds were exhausted.
- [ ] **Minimization.** Each finding's script is 1-minimal under §13's operations, keeps its failure signature, and its reproducer replays byte for byte.
- [ ] **Coverage.** The coverage report states, for each subject, the bounds, the references, the choice points, the conflicting-pair orders and preemption points explored, and the frontier left.
- [ ] **Comparison.** For every race, the report gives the search's runs to first finding beside M5's seeded campaign.
- [ ] **Mutation.** Every source mutant of the M6 paths is detected, or listed as equivalent with its reason.

## 23. Draft Revisions

### 23.1 Revision 2: the M6.0 Design Audit

An independent audit of the first draft (at `673c8ac`) found the reductions unsound and two planted bugs unspecified or impossible. Revision 2 answers each finding.

| Finding | Severity | Change |
|---|---|---|
| M6A-01 | blocker | Destination equality is not an independence proof, because handlers share the scheduler (§8.3). The domain is exact and nothing is pruned (D4). The diamond counterexample is an oracle case (§17) |
| M6A-02 | blocker | The convergence cut is removed (§9.5). It may return only as a revision with a full continuation definition (O7) |
| M6A-03 | blocker | A deviation's branch point is its earliest scheduling, which for a group order is the earliest scheduling of any of its events (§8.4). Fork is distinct from restore, checks the parent, and never produces a reproducer checkpoint (§9.3). Identity semantics are frozen: tagged namespaces, `Init` wakes, counts on accepted schedulings, ordinal meaning (§7.1). The search forks from a scripted reference, so golden snapshots are unchanged (§7.4) |
| M6A-04 | major | Delays are every `d` in `1..=D`, an exact domain; positions are gone (§7.3). `D` bounds the requested cycles, and clamped delays are duplicates (§9.2) |
| M6A-05 | major | Group orders keep causal and FIFO edges (§8.2). Legacy C is unchanged |
| M6A-06 | major | A group order is a complete admissible permutation with dense ranks, counting one deviation (§7.2). Branch points are totally ordered, and a script's parent is unique (§9.2). The oracle is independent of the finder (§17) |
| M6A-07 | major | R6 is replaced by `handoff-flag-first`, R7's failing interleaving is stated, and R8's construction is frozen. Each race needs a witness before the bounds are frozen, moved to M6.2 (§10) |
| M6A-08 | major | `B` is a fixed parameter, budgets are per run and between rounds with reported overshoot, checkpoint memory counts encoded bytes with deterministic eviction, and the search state is resumable (§9.4, §9.6) |
| M6A-09 | major | A feasibility gate precedes the bounds (§9.7). CI exhausts synthetic topologies and budgets `m5-reference` unless its exhaustion is measured to fit |
| M6A-10 | major | Minimization repeats to a fixed point and checks the final neighborhood (§13). plan.md says 1-minimal |
| M6A-11 | minor | Preemption points are per PID (§11.1). `BackendFailed` cannot satisfy detection (§16). Validation lists each obligation with finite fixtures and worker counts (§17) |
| Open questions | — | The domain, the reduction contract, the composition with A, B, and C, budgets and resumption, and the meaning of minimization are decided here. The bounds wait for the witnesses and the feasibility gate |

### 23.2 Revision 3: the Review of Revision 2

A review of revision 2 accepted its direction and found two gaps.

| Gap | Change |
|---|---|
| A reordering can add or remove events of its own group, and revision 2 dropped a script naming a missing event as unreached, without comparing its run | An entry whose identity does not occur is vacuous, and every run is compared. Effective ranks keep every dispatched order causal and FIFO, including for events a reordering adds, so a changed group never fails a script. Only exact duplicates, whose runs are identical, skip their own run (§7.2, §9.2). The conditional wake is an oracle case (§17) |
| R8's two events are not delayed directly, but a delay of another message can move when they are scheduled | "Only a pick finds R8" is checked by running every delay-only script within the CI bounds, or stated for the budget that ran (§10, §17) |

### 23.4 Revision 4: Shifts of Asynchronous Wakes

The M6.2 witness search for R6 found no script of delays and group orders that reaches its window, within or near any feasible bounds.

- **Measured.** On `m5-reference` under the empty script, R6's window, the retirement of `handoff`'s flag store, sits 6,192 to 6,202 cycles before the next timer interrupt at the default slice of 8,000, and no scoped message falls between the previous interrupt and the window. Across the 301 slices from 4,000 to 10,000 in steps of 20, no interrupt is taken right after `handoff`'s flag load or its branch to the stores. The window sits at a fixed phase after the expiry that dispatched `handoff`, so a delay of `D` cycles, which moves an interrupt only near a reference one, cannot reach it.
- **Outside the bounds.** A witness exists only far outside them: at a slice of 1,800, below A's range, with an event budget above M5's three million, delaying the interrupt controller's `sirq` message 22 by 13 to 16 cycles makes R6 diverge at `outcome[15]`.
- **Why a shift.** The nondeterminism R6 and R7 need is the preemption point, and on `m5-reference` the preemption comes from the timer's expiry, a self-wake. Revision 4 adds a third deviation, the shift of a wake of a component in the asynchronous set (§7.6). It is exact, bounded by `E` and the set like delays by `D` and the scope, and no candidate preemption point is chosen by a heuristic. Shift and delay share the branch-point kind, since their identities are tagged.
- **Encoding.** The script's encoding and snapshot format version 5 of M6.1 gain the shifts. No script is pinned before M6.2, so nothing committed changes.
- **Witnesses.** At the default slice, both races have shift-only witnesses, pinned with `Race::witness` and checked by `m6_witnesses`. They show the races are reachable, and they do not fix `E`. R8's witness is one pick, the kernel's 6,336th request ahead of the DMA's 289th, and diverges at `outcome` ordinal 0. Of the 384 such picks in the reference run, 74 diverge alone, and the others agree.
  - R6 diverges at `outcome[15]` when timer expiry 3 is shifted by −5,020 cycles, into `handoff`'s window. Shifts of −5,015 to −5,029 diverge, as do shifts of expiry 6 by −6,197 to −6,211.
  - R7 diverges at `outcome[19]` when expiry 0 is shifted by −5,308 cycles, preempting one `counter` instance after its `turn` store, and expiry 1 by −4,311 cycles, preempting the other inside its critical section. Every shift of expiry 1 from −4,309 to −4,329 diverges with it. Each shift alone agrees.
  - The unpatched scenario agrees under both witnesses, and under every other timer shift tried.

### 23.5 Revision 5: Picks

Writing the oracle of §17 showed that revision 3's group orders did not reach every admissible order.

- **The gap.** A group order fixed ranks when events were scheduled, and an event that a reordering added took its floor, the rank of the event that scheduled it. In the conditional wake of §7.2, the order B, A of a parent group A, B adds W, and the ranks placed W right after B. No script reached the admissible order B, A, W: no group order could name W, which the parent did not have, and the child's changed group kept the parent's branch point, so no child could reorder it. An oracle of every admissible order would fail there, and narrowing the domain claim to the orders ranks give would leave orders unexplored.
- **The cause.** An order fixed at scheduling time cannot name the events that exist only because of that order.
- **The change.** A pick decides one dispatch, among the events ready at it (§7.2). Every admissible order of a group is a sequence of picks, and a run's picks are read off its dispatches, so each order is reached by exactly one script. The oracle is an independent enumerator of dispatches on the naive model (§17).
- **Costs.** An order of `n` events takes up to `n - 1` deviations instead of one, so `k` buys less reordering. Effective ranks, raised ranks, and minimization's one-inversion move go. M6.1's group orders, their tag 2 encoding, and the group choice point are replaced at M6.2. No script with a group order is pinned, so nothing committed changes.
- **Runtime.** A scripted session schedules with plain counters, and the scheduler takes a named ready event of the next `(tick, phase)` in place of its least one. Keys stay `(tick, phase, sequence)`, and `EventKey` is unchanged (D12).
