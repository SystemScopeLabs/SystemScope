# M5 Design: Timing Exploration

> Status: Frozen at M5.0 · Implementation status: M5.2 complete, M5.3 next · Parent: [plan.md](../plan.md) · Builds on: [m4-design.md](m4-design.md), [m3-design.md](m3-design.md), [m2-design.md](m2-design.md), [m1-design.md](m1-design.md), [m0-design.md](m0-design.md)

This document is the design of M5, frozen at M5.0. The items still marked **(open)** are decided at the step §24 names; none of them changes a frozen decision.

---

## 1. Goal and Shape

M5 makes timing a controlled input. Then it uses the M4 engine to show which behavior depends on timing.

SystemScope is deterministic: one seed and one topology give one execution. That is what makes every M0–M4 result reproducible. It also means a model run explores exactly one interleaving of the CPU, its interrupts, the DMA engine, the storage, and the operating system. Real systems do not have that luxury. Device latency varies, interrupts arrive at different instructions, and a preemptive scheduler switches processes at different points. Bugs that depend on that variation are among the hardest to find and to reproduce.

M5 keeps determinism and adds **perturbation**: a seeded, recorded change to the timing of one run. A perturbed run is as deterministic as any other. It is defined by the scenario plus its perturbation, and it replays byte for byte. So a timing-dependent failure that M5 finds is never flaky. It comes with its perturbation, the M4 locator and reports, a reproducer, and a minimized perturbation.

```text
                  scenario  +  perturbation (seeded, recorded)
                          │
             ┌────────────┴────────────┐
             ▼                         ▼
     unperturbed run              perturbed run
             │                         │
             └──── timing-invariant ───┘
                   profile (M4 engine)
                          │
              Agree ──────┴────── Diverged
                                     │
                     first divergence, schedule evidence,
                     reproducer, minimal perturbation
```

M5 has three parts:

1. **Perturbation** (§7): three seeded mechanisms, which change constant latencies, delay individual messages, and reorder simultaneous events.
2. **A concurrent workload** (§9–§12): an S-level external interrupt, a timer, DMA completion by interrupt, preemptive scheduling, and memory shared between processes. Without them, timing cannot change any interleaving that matters.
3. **Exploration** (§13–§17): timing-invariant profiles, seeded campaigns, schedule evidence, perturbation minimization, and coverage. Each one is built on the M4 engine.

## 2. Scope and Non-goals

### 2.1 In Scope

- the three perturbation mechanisms (§7) and their recording in snapshots and reproducers;
- the `M5` CPU profile: delegated S-level external interrupts (§9);
- a timer device, routed through the interrupt controller (§10);
- `ModeledKernel` additions: preemption by the timer, a blocking block-read syscall completed by the DMA interrupt, and a shared page (§11);
- the `m5-reference` platform and its workload, with deliberately racy input variants (§12);
- timing-invariant comparison profiles (§13), campaigns (§14), schedule evidence (§15), perturbation minimization (§16), and coverage (§17);
- Spike-directed tests for the `M5` profile (§21).

### 2.2 Non-goals

- **No change to M0–M4 behavior.** An unperturbed run of any existing scenario is byte-identical to its golden files (§19).
- No multiple harts, SMP, caches, TLB, atomics (the A extension), or memory-model relaxation. There is one hart, so memory is sequentially consistent.
- No CLINT, `mtime`/`mtimecmp`, Sstc, or machine timer interrupt (§10, D4).
- No new comparison engine. M5 adds profiles, inputs, and evidence to M4. The engine's frozen semantics stay as they are.
- No RTL, native kernel, or UI. Those are M8, M6, and M7.
- No unbounded search. A campaign is an explicit seed list, as in M4 (§18 of m4-design). Systematic enumeration of schedules is outside M5. M5 only measures the schedules a campaign reached (§17).

## 3. Frozen M5 Decisions

| # | Decision | Section |
|---|---|---|
| D1 | Timing becomes an input. A perturbation is part of a run's identity, and it is recorded, replayed, and minimized like any other input | §7 |
| D2 | Three mechanisms: **A** a constant-latency sweep, **B** seeded per-message jitter, and **C** a seeded order of simultaneous events. With no perturbation, all three are the identity | §7 |
| D3 | Interrupts reach the kernel as delegated S-level external interrupts (`mideleg.SEI`), so `sstatus.SIE` masks them atomically in the trampoline | §9 |
| D4 | The timer is a memory-mapped device on the interrupt controller, not a CLINT. The modeled kernel works only through memory, and it can program and acknowledge this timer | §10 |
| D5 | The kernel preempts on the timer, completes block reads by interrupt, and maps one shared page into every process | §11 |
| D6 | M3's scenario and golden files stay frozen. M5 adds `m5-reference` | §12 |
| D7 | Primary comparisons use timing-invariant streams: per-process outcomes, declared final memory, and per-process syscall results. They never use a global interleaving | §13 |
| D8 | Each racy variant is a planted bug with a known correct counterpart. M5's detection is measured on those pairs | §12.3 |
| D9 | Minimization shrinks the perturbation itself, besides M4's prefix and window | §16 |
| D10 | Tie order uses a rank-major `sequence`, so contracts and `EventKey` do not change. `WFI` stays illegal, and an idle hart spins in the trampoline | §7.3, §11.3 |

## 4. What M0–M4 Provide

- **Deterministic DES (M0).** Events are ordered by `(tick, phase, sequence)`, and the runtime assigns `sequence` (contracts `EventKey`). Every link carries a constant `LinkLatency`: either `After(Duration)` or `Cycles { domain, k }`. Snapshots restore at every event.
- **Devices (M2).** `SimpleIrqController`, a level-sensitive controller with `sources` inputs. `DmaBlockController` with its completion line and `IRQ_ENABLE`, and `SimpleBlockMedia` with a constant latency (`m3-reference`: 16 cycles). Round-robin bus arbitration among masters.
- **OS (M3).** `ModeledKernel` behind `kgate`, cooperative FIFO scheduling, and DMA completion by **polling** with `IRQ_ENABLE = 0` (m3-design §8.2, decision 7). It has no timer and no preemption (decision 9).
- **Verification (M4).** Profiles with a primary stream and stop rules, three locator strategies, state and trace diffs, slices, reproducer bundles, and minimization. Components carry per-component RNG streams (m0-design §5.2), but no component uses them yet.

So in `m3-reference`, changing a latency changes only how long the kernel polls. That shows up immediately in the `arch` stream as a different number of `STATUS` loads. No user-visible interleaving can change. That is why M5 needs both perturbation and a concurrent workload.

## 5. Terminology

| Term | Meaning |
|---|---|
| **Perturbation** | A canonical value that changes a run's timing: a set of latency overrides (A), a jitter seed and bound (B), and a tie-order seed (C). The empty perturbation changes nothing |
| **Perturbed run** | A scenario run under a nonempty perturbation. It is as deterministic as an unperturbed run |
| **Timing-invariant stream** | A stream that a race-free program produces identically under every perturbation (§13) |
| **Timing-dependent behavior** | A difference in a timing-invariant stream between two runs that differ only in perturbation |
| **Planted race** | An input variant of `m5-reference` with a deliberate synchronization bug, paired with its correct counterpart (§12.3) |
| **Schedule** | The sequence of the kernel's dispatch decisions, with the event at which each was made (§15) |
| **Minimal perturbation** | A perturbation that still produces the failure, from which no single element can be removed without losing it (§16) |

## 6. Comparison Model

M5 compares two runs of the same scenario that differ only in their perturbation. Usually one side is unperturbed. Both sides are SystemScope, so all the same-engine evidence of M4 applies (m4-design D1, D2).

- **Primary stream.** A timing-invariant stream, chosen by profile (§13). Divergence there is a finding.
- **Auxiliary streams.** `arch`, `os`, and the schedule stream (§15). Under perturbation they are expected to differ. They are evidence, never verdicts.
- **Locator.** M4's checkpoint search works unchanged, because both sides take snapshots. A primary divergence is located at the first differing outcome record. The schedule evidence then points back to the first differing dispatch decision (§15).

## 7. Perturbation Model

A perturbation `P` has three independent parts. A run's perturbation is fixed at construction, enters the topology identity only when nonempty, and is stored in snapshots and reproducers (§18).

### 7.1 A: Constant-Latency Sweep

`P.latency` maps link and component paths to a replacement `LinkLatency`. Examples are the disk's `Cycles { cpu, 16 }` becoming `Cycles { cpu, 3 }`, or a bus link becoming `Cycles { cpu, 2 }`.

- It changes the configuration only, so the components need no code change. It reaches two places, and only the platform builder knows both:
  - link latencies, which are in the topology: the trace header and `topology_hash`;
  - targets' response latencies, such as `RamConfig::latency` or `BlockMediaConfig::latency`, which are component configuration and are stored in snapshots.

  So A is an input of the platform builder (`m5ref::Config`), not of the runtime. An A-perturbed run is a different topology or configuration by construction.
- Every override must keep the component's own invariants (§20). For example, a target's response latency must stay a whole number of cycles of its domain.
- It is the coarsest mechanism and the cheapest to explain. A campaign samples overrides from declared ranges per path.

### 7.2 B: Per-Message Jitter

`P.jitter = { seed, max, scope }` adds `j` extra cycles to a message's delivery, with `0 ≤ j ≤ max`. Here `j` is drawn from a stream derived from `seed`, the link, and the message's per-link ordinal. So it does not depend on the order in which links are used.

- The runtime applies it in `Ctx::try_send`, after `add_latency`, when a link delivery becomes a tick. Components see only a later delivery. Self-wakes (`wake_self`) do not cross a link and are never jittered.
- `scope` lists the links it applies to. Device and DMA links are the default. Bus-to-RAM links are left out unless named, because they multiply the campaign cost without adding new interleavings.
- Jitter only delays. It never reorders two messages on the same link (FIFO per link), because `mem.v1` and `irq.v0` assume in-order delivery. A jittered delivery tick is therefore `max(tick + j, last)`, where `last` is the link's latest delivery tick. Equal ticks keep sequence order.
- Each link's message ordinal and latest delivery tick are runtime state. They are stored in the snapshot of a perturbed session, so a restore continues the same jitter stream. An unperturbed session's snapshot layout is unchanged (§19).

### 7.3 C: Tie Order

`P.ties = { seed }` replaces the order of events with equal `(tick, phase)`. Unperturbed, their order is ascending `sequence`. Perturbed, it is a seeded permutation.

- It exposes components that rely on the order of simultaneous events: for example, a device's completion and a CPU fetch in the same `Request` phase.
- It must keep every runtime rule: an event is never scheduled before the one being dispatched, and `Init` and `Observe` keep their positions.
- **Mechanism: a rank-major `sequence`.** In a C-perturbed session, the runtime assigns `sequence = rank << 40 | counter`:
  - `counter` is the existing counter (`next_sequence`), so sequences stay unique and are assigned in scheduling order in their low bits;
  - `rank` is a 24-bit draw from the tie seed and the counter;
  - for an event scheduled into the `(tick, phase)` being dispatched, `rank` is drawn from `[current rank, 2^24)`, so its key is always after the current event's.

  The scheduler still dispatches in strict `EventKey` order, and every key is after the last dispatched one. `EventKey`, its encoding, and the scheduler's heap are unchanged, and so is the contracts pin. An unperturbed session has `rank = 0`, so `sequence = counter` exactly, as in M0–M4.
- **What it revises.** m0-design S4 ("assigned in scheduling order") holds for `counter`, not for the whole `sequence`, in a C-perturbed session. S6 applies to `counter` below `2^40`. Restore's checks (m0-design §7) compare `counter` with `next_sequence`. The contracts doc comment "Global scheduling order" stays true of the low bits, and it can be clarified without a code change.
- **Rejected alternatives.**
  - Popping ties out of key order would break S4's dispatch rule and restore's `EventBeforeCursor` check.
  - A rank field in `EventKey` would change its canonical encoding, and with it every M0–M4 trace and golden digest.

### 7.4 Identity and Recording

- The empty perturbation is the identity. A run built with it is the same run, byte for byte, as one built without M5 (§19).
- `P` has a canonical encoding (`perturbation.bin`) and a BLAKE3 identity. A nonempty `P` is part of the session identity, so a snapshot from a perturbed run never restores into an unperturbed session.
- A perturbed run's trace records the perturbation once, in its header. A message's jitter is recorded with its delivery, so evidence can show where timing changed.

## 8. What Can Change, and What Must Not

Under any perturbation, a correct `m5-reference` program may legitimately change:
- the number of instructions and kernel accesses spent waiting (polling, spinning);
- where interrupts and preemptions land, and so the global interleaving of processes, the `os` record order, and the UART byte order across processes.

It must not change:
- each process's own output (its `write` bytes, in order);
- each process's exit status or fault;
- each process's syscall results, except those that observe time or scheduling (§13.2);
- the final value of every memory word that the scenario declares invariant (§12.2).

A planted race breaks at least one of these under some perturbation. Its correct counterpart breaks none under every perturbation in the campaign.

## 9. CPU: The `M5` Profile

The `M5` profile is `M3` plus delegated S-level external interrupts. Every other rule of `M3` is unchanged.

| CSR | `M3` | `M5` |
|---|---|---|
| `mideleg` | read-only 0 | bit 9 (SEI) writable; every other bit read-only 0 |
| `sie` | read-only 0 | `SEIE` (bit 9) writable; a view of `mie.SEIE` |
| `sip` | read-only 0 | `SEIP` (bit 9) read-only in S: the OR of the external S-level line and the M-writable `mip.SEIP` bit |
| `mie` | `MEIE` | `MEIE`, `SEIE` |
| `mip` | `MEIP` read-only | `MEIP` read-only; `SEIP` writable by M, ORed with the line |

- **Eligibility.** Interrupts are sampled after a retirement, as in M2. SEI is taken when `SEIP & SEIE` and, if delegated, `priv < S || (priv = S && SIE)`. If not delegated, it is taken in M under the M-level rule. MEI keeps priority over SEI.
- **Delivery to S.** `sepc ← pc`, `scause ← 0x8000_0009`, `stval ← 0`, `SPP ← priv`, `SPIE ← SIE`, `SIE ← 0`, `priv ← S`, `pc ← stvec.BASE`. It emits `rv32.interrupt` with `to = S`.
- **Why delegation (D3).** The trampoline runs in S with `SIE = 0`. A delegated SEI that becomes pending there waits until `SRET` sets `SIE ← SPIE` and drops to U. It is then taken at the first U boundary, atomically, with no window. An M-mode firmware forwarding the interrupt in software has a window between unmasking and `SRET` where an interrupt is lost. The modeled kernel cannot touch CSRs to close that window.
- **Lines.** The `M5` profile adds a second `irq.v0` target port, `sirq`, which drives the external part of `SEIP`. The interrupt controller is unchanged: in `m5-reference`, its `cpu` output is linked to `sirq` instead of `irq`. `m2-reference` and `m3-reference` keep driving `irq` (MEI).
- **Snapshot.** The `M5` profile adds `mideleg.SEI`, `mie.SEIE`, and the M-writable `mip.SEIP` to the CPU snapshot as a new schema. Schemas for `M1`–`M3` are unchanged.

## 10. Timer Device

`SimpleTimer` (`components/platform`) is a memory-mapped down-counter that raises an `irq.v0` line.

| Offset | Register | Access |
|---|---|---|
| `0x0` | `COMPARE`: cycles until expiry, counted on its clock domain | RW |
| `0x4` | `CONTROL`: bit 0 `ENABLE`, bit 1 `PERIODIC` | RW |
| `0x8` | `STATUS`: bit 0 `EXPIRED`; writing 1 clears it and drops the line | RW1C |

- It expires by scheduling one wake for `COMPARE` cycles ahead. It does not tick every cycle, so an idle timer costs nothing.
- Its expiry is a self-wake, which is never jittered. Its line to the interrupt controller is an `irq.v0` link, so jitter (B) on that link and tie order (C) move it. So does A on its `COMPARE` default. Perturbing those is how a campaign moves preemption points.
- **Why not CLINT (D4).** `mtime`/`mtimecmp` interrupt in M, and only M-mode software or Sstc can rearm them. The modeled kernel works only through memory (m3-design §6.1), so it would need firmware help on every tick, and that firmware path brings back the forwarding window of §9. A memory-mapped timer on the interrupt controller needs no CPU change beyond §9, and the M6 native kernel can use the same device.

## 11. Kernel Additions

The kernel's execution model is unchanged (m3-design §6.3). It still runs one operation per `ENTER`, while the CPU is held, and works only through memory. What changes:

### 11.1 Interrupt Traps

- A trap with `scause = 0x8000_0009` is an interrupt operation. The kernel reads the interrupt controller's pending register to find the sources, and handles each in source order:
  - **Timer:** acknowledge `STATUS`, then preempt (§11.2);
  - **Block controller:** acknowledge `ACK`, then wake the waiting process (§11.3).
- Boot, done by the firmware stub in M (§12.1), sets `mideleg.SEI` and `sie.SEIE`. Afterwards no CSR changes, and masking is only `sstatus.SIE` in the trampoline.
- The kernel's access whitelist (m3-design §6.3) gains the interrupt controller's and the timer's windows. At boot the kernel writes the controller's `ENABLE` for both sources.

### 11.2 Preemption

- **Time slice.** At boot the kernel programs the timer as periodic with `SLICE` cycles, a configuration constant.
- **On a timer interrupt** the running process is switched away exactly as on a `sched_yield` (m3-design §6.6). Its context comes from the trap frame, `sepc` unchanged, and it goes to the tail of the run queue. Then the head is dispatched.
- The run queue stays FIFO. Which process runs next depends only on the order of yields, preemptions, and wakes. Perturbation changes that order through where interrupts land.

### 11.3 Blocking Block Read

A new syscall (the number is chosen in M5.3 from the Linux-numbered range; **(open)**): `blk_read(lba, buf)`.
- It validates `buf` as `write` does, for one 512-byte block with `W`. Then it starts a DMA read into staging with `IRQ_ENABLE = 1`, marks the caller `Blocked { lba }`, and dispatches the next process.
- **On completion** (the interrupt), the kernel copies the block from staging to the caller's buffer with its own accesses, sets `a0 = 0`, and appends the caller to the run queue.
- **Errors** are as in M3's load path: a controller error gives `-EIO`.
- **One request in flight.** A second `blk_read` while the controller is busy blocks the caller in a FIFO wait queue. This is a deliberate shared resource.
- **If every process is blocked**, the kernel sets the trap frame's action to a new `Idle`. The trampoline then sets `SIE` and spins in S until an interrupt. The interrupt enters the trampoline normally, and the kernel ignores the saved spin context. `WFI` stays illegal, as in `M3` (D10).

### 11.4 Shared Page

- Every process maps one 4 KiB shared frame at the fixed VA `SHARED_VA`, with `U R W`. The kernel allocates it once at boot and frees it at shutdown.
- It is the only memory that processes share. With one hart and no caches, it is sequentially consistent, so Peterson-style mutual exclusion is correct, and the workload can use it without atomics.
- A shutdown dumps the declared invariant words of the shared page into the `os` stream (§12.2). That makes final memory a comparable stream, not just a state diff.

### 11.5 Snapshot and Trace

- The kernel's snapshot schema grows: the `Blocked` state, the block wait queue, the timer configuration, and the shared frame. M3's schema is unchanged and still restores.
- New records, in the existing naming: `os.process.preempt { pid }`, `os.process.block { pid, lba }`, `os.process.wake { pid }`, `os.interrupt { sources }`, and `os.final { word, value }` at shutdown. Dispatch stays `os.process.switch`.

## 12. `m5-reference`

### 12.1 Platform

It is `m3-reference` with:
- the CPU on the `M5` profile;
- `soc.timer`, a `SimpleTimer` at `0x1000_4000`, on interrupt source 1;
- the block controller on source 0, with the interrupt used;
- the interrupt controller with 2 sources, driving the CPU's `seip`;
- a firmware stub that also sets `mideleg.SEI` and `sie.SEIE`;
- its own disk image.

The physical layout and seed are otherwise those of m3-design §11. The exact layout is frozen at M5.3.

### 12.2 Workload

| Entry | Program | Exercises | Invariant |
|---|---|---|---|
| 0 | `counter` | Two instances. Each adds 1 to a shared counter `N` times inside a Peterson critical section | final counter = 2N |
| 1 | `counter` | the second instance | |
| 2 | `reader` | `blk_read`s a known block, checks it, and prints a digest of it | its output |
| 3 | `reader` | a second reader, which contends for the controller | its output |
| 4 | `spinner` | Computes without syscalls. Only preemption interleaves it | its output and status |
| 5 | `handoff` | A one-slot mailbox with a flag, sent to `spinner` through the shared page | the sequence it receives |

Each program prints only its own result. Its output is compared per process (§13).

### 12.3 Planted Races (D8)

Each planted race is an input variant (m4-design §6) that patches one program on the disk image. Each has a correct counterpart, which is the unpatched scenario.

| Variant | Bug | Expected under perturbation |
|---|---|---|
| R1 `counter-unlocked` | the increment is outside the critical section | final counter < 2N for some perturbation |
| R2 `peterson-order` | `flag` and `turn` written in the wrong order | mutual exclusion lost for some perturbation |
| R3 `handoff-flag-first` | the flag is set before the data | a stale value is received for some perturbation |
| R4 `reader-early` | the buffer is read before `blk_read` returns, through a second mapping | a stale block for some perturbation |
| R5 `kernel-wake-lost` | a kernel variant that checks `Blocked` before acknowledging the interrupt | a reader never wakes; the run halts at `max_instructions` |

R5 is a kernel input variant, which is a configuration flag of the modeled kernel used only by tests. It shows that M5 finds OS-level races, not only user-level ones.

## 13. Timing-Invariant Profiles (D7)

### 13.1 The `outcome` Stream

`outcome` is a new stream projected from the `os` and UART records. Its observations are grouped per process in pid order, and in event order within each process. Each observation is one of:
- `out { pid, bytes }`, one per `write`. The bytes are the `platform.uart.tx` records emitted between that syscall's `os.syscall.enter` and `os.syscall.exit`. A kernel operation runs while the CPU is held, so nothing else can write the UART in between;
- `ret { pid, nr, value }`, one per syscall in the time-invariant set (§13.2);
- `end { pid, status | fault }`;
- `final { word, value }`, from the shutdown dump.

The ordinal is per process. So two runs that interleave processes differently, and do nothing else differently, produce the same `outcome` stream.

### 13.2 Syscalls Whose Results Are Invariant

`write`, `blk_read`, `getpid`, `exit`, and `exit_group` are in the invariant set. `sched_yield` always returns 0, so it is in too. **(open)** A syscall that reports time or scheduling, if M5 adds one, is excluded.

### 13.3 Profiles

| Profile | Primary | Auxiliary | Use |
|---|---|---|---|
| `m5.outcome` | `outcome` | `os`, `arch`, `schedule` | every campaign |
| `m5.lockstep` | `arch` | `os`, `schedule` | the empty perturbation only: an unperturbed run against itself. It checks that M5's platform agrees with M4's model of observation |

Both profiles use M4's stop rules and report window.

## 14. Campaigns

- **Seeds.** A campaign is an explicit list of perturbation seeds per mechanism. Each seed derives `P` from declared ranges: override ranges per path for A, `max` and `scope` for B, and a tie seed for C. The CI list is fixed. A nightly run adds a printed seed, as in M4.
- **Runs.** For each seed, the scenario (or a variant) runs perturbed and unperturbed, compared with `m5.outcome`.
- **Verdict.**
  - On the unpatched scenario, every seed must `Agree`. A divergence is a bug in the model or the workload, and it fails CI with a reproducer.
  - On each planted race, the campaign must find at least one diverging seed. The first such seed, with its reproducer and minimal perturbation, is pinned in a test.
- **Budget.** The CI campaign is sized to finish within the existing test budget. Larger campaigns run nightly. Seed counts are frozen in M5.6.

## 15. Schedule Evidence

A divergence in `outcome` is a symptom. The cause is an interleaving. M5 adds evidence to connect them. It is evidence, never a verdict (m4-design §15).

- **The schedule stream.** Each `os.process.switch`, `os.process.preempt`, `os.process.block`, and `os.process.wake`, with the event index and the preempted process's saved `sepc`. It is projected from the `os` records.
- **First differing decision.** The engine compares the two sides' schedule streams and reports the first decision that differs. It reports both sides' records around it and the perturbation elements that applied up to that event (§7.4).
- **Per-process trace.** For the diverging process, M4's state diff at the outcome divergence is shown, together with the shared-page words it read since its last dispatch. Those words come from the trace's memory records, through M4's slice links.

## 16. Perturbation Minimization (D9)

M4 minimizes the prefix and the replay window (m4-design §17). M5 also minimizes the perturbation:
- **A:** remove overrides one at a time, then move each remaining override toward its default value by bisection.
- **B:** replace the jitter stream by its list of nonzero draws up to the divergence, then delta-debug that list to a 1-minimal subset.
- **C:** replace the tie permutation by its list of swapped pairs up to the divergence, then delta-debug that list likewise.

Every candidate is a full deterministic run. It is accepted only if its `outcome` divergence keeps M4's failure signature (m4-design §17.2). The result is a perturbation from which no single element can be removed. It is usually a handful of delayed messages, which explains the race on its own. The minimized perturbation is written into the reproducer.

## 17. Coverage

Coverage measures what a workload and a campaign exercised. It is a report, never a gate.
- **Architectural coverage** of the `arch` stream: the instructions retired, the exception and interrupt causes and the modes they were taken in, and the Sv32 PTE cases walked. It is the measurement that M4.7 had to do by hand (m4-design §30.7). `m3-reference` gets the same report, for comparison.
- **Schedule coverage** across a campaign: the number of distinct schedule streams, and the preemption points per program (the `pc` of each preempted instruction).
- **Perturbation coverage**: per mechanism, the fraction of seeds whose schedule differed from the unperturbed one. A mechanism that never changes the schedule is reported as ineffective for that workload.

## 18. Reproducer

M4's bundle (m4-design §16) gains one entry, `perturbation.bin`, per side. It is not a new `InputVariant`: a planted race's side carries a media patch and a perturbation at once, and `InputVariant` holds one input. The manifest lists it with its identity, and replay builds each side with it. An M4 bundle without it means the empty perturbation, so every M4 bundle and corpus entry stays valid unchanged. The bundle format version is bumped only if the manifest schema needs it. **(open)**

## 19. Determinism and Compatibility

- **Identity.** With the empty perturbation, every runtime path is the M0–M4 path. Every M0–M4 golden, expected stream, corpus entry, and reproducer is byte-identical, and CI keeps all their jobs.
- **Cross-OS.** Perturbed runs, campaign results, minimized perturbations, and reproducers are byte-identical on Linux and Windows, as in M4.
- **Snapshots.** A perturbed run restores at every event, with its jitter ordinals and tie state, like any other run. `m5-reference` gets the every-event resume check of M2 and M3.
- **Observation.** Observing a perturbed run does not change it (m4-design §28.2 **Observation invariance**).

## 20. Failure Model

- An invalid perturbation fails construction: an unknown path, an override that breaks a component invariant, or a jitter scope naming a non-link. It never fails mid-run.
- A perturbed run that faults the session (a component fault, a protocol violation) is a finding, reported as M4's `BackendFailed` with the perturbation. A model must tolerate every delivery time its protocol allows. A failure there is a model bug that M5 has found. No M0–M4 test has ever delivered a response later than its configured latency. M5.1 therefore runs every existing reference scenario under B before any new workload exists.
- Planted race R5 halts at `max_instructions`. M4 reports that as the stop rule's outcome, a divergence in `end`.

## 21. Validation

- **Spike.** Directed programs for the `M5` profile's CSRs and delivery. They make SEI pending with the M-writable `mip.SEIP`, so Spike and SystemScope take it at the same instruction without any device. They are compared through M4's stream compare. The timer and the interrupt controller are SystemScope devices that Spike does not model, so they are not compared with Spike.
- **Unit and property tests.** Perturbation encoding round-trips. The empty perturbation is the identity. Jitter is FIFO per link. The tie permutation keeps runtime rules. Every perturbed run restores at every event.
- **Mutation.** Source mutants of the perturbation layer, and of the M5 kernel paths (preemption, wake, block queue), are judged by the campaign. Each must be detected or listed as equivalent with its reason, as in M4.7.
- **Planted races.** §14: every planted race is found, and every correct counterpart agrees.

## 22. Contracts

- The `M5` profile, `SimpleTimer`, and the kernel additions are systemscope components. They need no contract change.
- `irq.v0` already carries a level line. `seip` is a new CPU port using it.
- A is a builder input. B and C are runtime-internal (§7.3). The contracts pin does not move in M5.

## 23. Architecture Risks

1. **Campaign cost.** Each seed is two full runs, plus minimization on a divergence. Mitigation: checkpoint search, small CI seed lists, and minimization only on findings.
2. **Ineffective perturbation.** A mechanism may never move an interrupt or preemption. Mitigation: perturbation coverage (§17) measures it per mechanism.
3. **False positives from the workload.** A program whose output legitimately depends on timing would look like a race. Mitigation: the `outcome` stream groups by process, and §8 lists exactly what is invariant. Every program is reviewed against it, and the unpatched campaign must agree.
4. **Kernel complexity.** Preemption and blocking add states to a kernel that M3 kept small. Mitigation: the pure core (m3-design §6.3) is extended and tested without a runtime first. R5 checks that the tests can see a kernel race.
5. **Tie order and runtime invariants.** C changes how the scheduler assigns `sequence`. Mitigation: it is the last mechanism (M5.6), and a property test checks the scheduler's rules S1–S6 and restore under random C seeds.

## 24. Open Questions

| # | Question | Answer |
|---|---|---|
| O1 | Tie-order mechanism | **Decided (D10):** a rank-major `sequence` (§7.3) |
| O2 | `blk_read` syscall number | from the Linux-numbered range, chosen at M5.3 |
| O3 | Idle when every process is blocked | **Decided (D10):** a spin loop in the trampoline. `WFI` would need a new CPU wait state and snapshot field, and Spike can be compared on it only with an interrupt already pending |
| O4 | Reproducer bundle version bump | only if the manifest schema changes |
| O5 | Seed counts and ranges per mechanism | frozen in M5.6 from measured cost |
| O6 | How a trace records the perturbation (§7.4). `TraceHeader` is a contracts type, so a header field would move the contracts pin, which M5 keeps | at M5.4, with the evidence that reads it. Candidates: a runtime-origin record at the start of a perturbed trace, or the bundle's `perturbation.bin` alone (§27.1) |
| O7 | How a message's jitter is recorded with its delivery (§7.4) | at M5.4, with O6 |

## 25. Roadmap

| Step | Status | Scope | Changes |
|---|---|---|---|
| **M5.0** | complete | Design freeze (this document) | docs |
| **M5.1** | complete | Perturbation core: A in the builders, B in the runtime, the encoding, identity, and snapshot state; identity proven on every M0–M4 golden; every existing reference scenario run under B | code, tests |
| **M5.2** | complete | `M5` CPU profile and `SimpleTimer`; Spike-directed SEI tests | code, CI |
| **M5.3** | next | Kernel: interrupt traps, preemption, `blk_read`, shared page; `m5-reference` platform and golden | code, fixtures |
| **M5.4** | planned | `outcome` and `schedule` streams, the `m5.*` profiles, schedule evidence | code, tests |
| **M5.5** | planned | Workload programs and planted races R1–R5; campaigns over A and B | code, CI |
| **M5.6** | planned | Tie order (C), perturbation minimization, coverage reports, reproducer support | code, CI |
| **M5.7** | planned | Campaign hardening: source mutants of the M5 paths, cross-OS, nightly seeds | CI |
| **M5.8** | planned | Exit audit | docs |

## 26. M5 Exit Criteria

- [ ] **Identity.** With the empty perturbation, every M0–M4 golden, expected stream, corpus entry, and reproducer is byte-identical, and every M0–M4 CI job passes.
- [ ] **Determinism.** Every perturbed run, campaign result, minimized perturbation, and reproducer is byte-identical on Linux and Windows, and restores at every event.
- [ ] **Detection.** Each planted race R1–R5 is found by the CI campaign, by at least one mechanism, and located by M4 at a divergence in `outcome`.
- [ ] **No false positives.** The unpatched `m5-reference` agrees on `m5.outcome` for every CI seed of every mechanism.
- [ ] **Evidence.** Each finding reports the first differing schedule decision and the perturbation elements before it.
- [ ] **Minimization.** Each finding's perturbation is minimized to a 1-minimal set that reproduces the same failure signature, and its reproducer replays byte for byte.
- [ ] **Coverage.** Architectural, schedule, and perturbation coverage are reported for `m5-reference` and `m3-reference`.
- [ ] **Spike.** The `M5` profile's directed SEI tests agree with the pinned Spike.
- [ ] **Mutation.** Every source mutant of the M5 paths is detected by a campaign, or listed as equivalent with its reason.

## 27. Revisions

### 27.1 M5.1

M5.1 implemented A and B and their snapshot state. Every M0–M4 golden, expected stream, corpus entry, and reproducer passes unchanged: an unperturbed session takes the M0–M4 path and writes the same snapshot bytes. The M2 and M3 reference scenarios pass under B on every link and under A overrides, and no component faulted on a late delivery (§23).

| Section | Revision | Why |
|---|---|---|
| §7.1 | A is `perturb::Latencies` in the `m2-reference` and `m3-reference` builders' `Config`: link cycles by declaration index (`1..=64`) and response cycles by target path (`0..=64`). The builder rejects an unknown link or path, or a value out of range | links have no path of their own, and the builders already declare them in a fixed order. The bounds keep every override a whole number of cycles (§20) and campaigns finite |
| §7.2 | B is `runtime::jitter::Jitter { seed, max_cycles, links }`, a field of `SessionConfig`. `links` is a set of declaration indices. Elaboration rejects `max_cycles = 0`, an empty set, an undeclared link, and a link without a `Cycles` latency. `j` is the first 8 bytes of `blake3::derive_key("SystemScope 2026-10 jitter v1", seed ‖ link ‖ dir ‖ ordinal)`, little-endian, modulo `max_cycles + 1`. Each direction of a link is its own lane, with its own ordinal and latest tick | a jitter of `j` cycles is defined only for a `Cycles` link. Directions are separate FIFO channels, so one end's traffic does not shift the other's draws |
| §7.2, §7.4, §19 | A jittered session writes snapshot format version 2: version 1 with the setting and every lane after `topology_hash`. Restore rejects a snapshot whose setting differs from the session's (`InvalidState`). The setting is not part of `topology_hash`, and an unjittered session still writes version 1 | the snapshot is what makes the setting part of the session identity. Leaving `topology_hash` alone keeps A and B separable in evidence |
| §7.4 | The trace does not record the perturbation yet (O6, O7) | `TraceHeader` is a contracts type. M5.4 decides the record with the evidence that reads it |

`m2ref::components`, the M2 tests' snapshot reader, reads version 2. The other hand-written snapshot readers (`tests/acceptance` layout and portable decoder, the OS test helpers) read only unperturbed snapshots and are unchanged. AT2's malformed-snapshot test now uses version 3 as the unknown version, and checks that a version 1 body labeled version 2 does not decode.

### 27.2 M5.2

M5.2 implemented the `M5` profile (`components/rv32i/src/sei.rs`) and `SimpleTimer` (`components/platform/src/timer.rs`), and the directed SEI programs (`tests/rv32/src/seigen.rs`). They agree with the pinned Spike, and every M3.2 privilege program agrees again under `M5`. Every M0–M4 golden, expected stream, and reproducer passes unchanged.

| Section | Revision | Why |
|---|---|---|
| §9 | `sie` and `sip` read and write only what `mideleg` delegates: `SEIE` and `SEIP` read 0 while SEI is not delegated, and a write of `sie` changes `mie.SEIE` only while it is | the privileged spec's masking rule. Without it, S could enable or see an interrupt that M keeps for itself |
| §9 | `mip.SEIP` reads the OR of the M-writable bit and the `sirq` line. A CSR read-modify-write of `mip` (`csrrs`, `csrrc`) modifies the writable bit only, never the line | as in Spike and the privileged spec: otherwise a `csrrc` of another bit would latch the line's level into the writable bit |
| §9 | An interrupt is sampled at every retirement boundary, so a CSR write or `MRET`/`SRET` that makes one eligible is followed by its entry, before the next instruction | as in M2 and Spike, which takes it at the same boundary |
| §9 | The `M5` `rv32.interrupt` record is `epc, cause, handler, from, to`; the M2 record is unchanged. The M4 projection reads it as `pc = epc`, `cause`, `to` | `to` is what delegation decides, and Spike's log gives `epc` and `cause` |
| §9 | The `sirq` port is `PortId(2)`. A platform without an S-level source links it to a test-only tied-low `irq.v0` source (`soc.sirq_low` in the rv32 runner) | every declared port must be linked |
| §9 | CPU snapshot schema 4 is schema 3 followed by four bools: `mideleg.SEI`, `mie.SEIE`, the writable `mip.SEIP`, and the `sirq` level | the line's level is state the CPU sees between deliveries |
| §10 | `COMPARE` is at least 1: a write of 0 gets an access fault and changes nothing, and `TimerConfig::compare_reset` is a `NonZeroU32`. A `COMPARE` write takes effect at the next arm; a `CONTROL` write that keeps `ENABLE` set does not re-arm | a zero-cycle expiry would wake in the same tick as the arming write. Not re-arming keeps the expiry tick a function of the arming write alone |
| §10 | The self-wake's token is a generation counter that every arm and disarm advances; a stale wake is ignored | the runtime has no cancel. The generation is snapshot state (schema 1) |
| §10 | The line is `EXPIRED`. A one-shot expiry clears `ENABLE`; a periodic one re-arms `COMPARE` cycles from the expiry. The timer emits `platform.timer.arm` {`cycles`, `generation`} and `platform.timer.expire` {`generation`, `periodic`} | the trace shows every preemption point a campaign moves |
| §21 | The Spike profile is `spike-m5-sei` (`cargo xtask spike`, part M5 SEI). Spike logs `interrupt #9` with no `tval`. It logs a write of `mip` with `mvip` and with `MTIP` always set, and a write of `sie` with the `mie` it changes. The parser drops `mvip` and that `mie`, and masks `MTIP` | Spike's CLINT timer is always pending under `--priv`; SystemScope has no `MTIP` |
| §21 | The directed programs write `mip` only with a `csrrs`/`csrrc` of `SEIP`, `mideleg` only with `SEIP` or 0, and never `sip` | Spike also delegates `SSIP` and `STIP`, which `M5` does not have |
