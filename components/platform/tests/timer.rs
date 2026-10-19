//! `SimpleTimer` (`docs/m5-design.md` §10): ports, the register window, arming, expiry,
//! disarming and stale wakes, periodic and one-shot modes, the line, inspect, trace, and
//! snapshots, driven directly through a mock context; then in a real runtime, where an
//! expiry lands exactly `COMPARE` cycles after the arming write and restores at every
//! event.

mod common;

use std::num::NonZeroU32;

use common::{IrqSent, MockCtx, Script, Wake, read, restore_into, snapshot_of, write};
use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_platform::timer::{
    ARM_KIND, COMPARE, CONTROL, ENABLE, EXPIRE_KIND, EXPIRED, IRQ_PORT, MEM_PORT, PERIODIC, SIZE,
    SNAPSHOT_SCHEMA, STATUS,
};
use systemscope_platform::{SimpleTimer, TimerConfig};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;

const CLOCK: ClockDomainId = ClockDomainId(0);

const LATENCY: LinkLatency = LinkLatency::Cycles {
    domain: CLOCK,
    k: 2,
};

fn config() -> TimerConfig {
    TimerConfig {
        clock: CLOCK,
        latency: LATENCY,
        compare_reset: NonZeroU32::new(100).unwrap(),
    }
}

fn timer() -> SimpleTimer {
    SimpleTimer::new(config())
}

fn word(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// Delivers `msg` on `mem` in `Request`; returns the context with the response taken out.
fn access(t: &mut SimpleTimer, msg: MemMsg) -> (MemMsg, MockCtx) {
    let mut ctx = MockCtx::new(Phase::Request);
    ctx.deliver(t, MEM_PORT, msg).unwrap();
    let sent = ctx.take_one();
    assert_eq!(sent.port, MEM_PORT);
    assert_eq!(
        (sent.when, sent.phase),
        (
            ScheduleWhen::Cycles {
                domain: CLOCK,
                k: 2
            },
            Phase::Complete
        )
    );
    (sent.msg, ctx)
}

fn read_reg(t: &mut SimpleTimer, offset: u64) -> u32 {
    let (resp, ctx) = access(t, read(1, offset, 4));
    assert!(ctx.irqs.is_empty() && ctx.wakes.is_empty());
    match resp {
        MemMsg::ReadResp {
            txn: TxnId(1),
            outcome: ReadOutcome::Data { data },
        } => u32::from_le_bytes(data.try_into().unwrap()),
        other => panic!("{other:?}"),
    }
}

/// Writes `value` to `offset`, which must succeed; returns what else the write did.
fn write_reg(t: &mut SimpleTimer, offset: u64, value: u32) -> MockCtx {
    let (resp, ctx) = access(t, write(2, offset, &word(value)));
    assert_eq!(
        resp,
        MemMsg::WriteResp {
            txn: TxnId(2),
            outcome: WriteOutcome::Done
        }
    );
    ctx
}

fn fault_of(resp: &MemMsg) -> bool {
    matches!(
        resp,
        MemMsg::ReadResp {
            outcome: ReadOutcome::Fault {
                fault: MemFault::AccessFault
            },
            ..
        } | MemMsg::WriteResp {
            outcome: WriteOutcome::Fault {
                fault: MemFault::AccessFault
            },
            ..
        }
    )
}

fn arm_wake(cycles: u64, token: u64) -> Wake {
    Wake {
        when: ScheduleWhen::Cycles {
            domain: CLOCK,
            k: cycles,
        },
        phase: Phase::Complete,
        token,
    }
}

fn line(asserted: bool) -> IrqSent {
    IrqSent {
        port: IRQ_PORT,
        asserted,
        when: ScheduleWhen::Now,
        phase: Phase::Complete,
    }
}

fn wake(t: &mut SimpleTimer, token: u64) -> MockCtx {
    let mut ctx = MockCtx::new(Phase::Complete);
    t.handle_event(&Delivered::Wake { token }, &mut ctx)
        .unwrap();
    assert!(ctx.sent.is_empty());
    ctx
}

fn u(v: u64) -> Value {
    Value::U64(v)
}

// ---------------------------------------------------------------------------------------
// Ports, reset, and the window.

#[test]
fn ports_reset_and_init() {
    let mut t = timer();
    assert_eq!(t.type_name(), "platform.timer");
    assert_eq!(
        t.ports(),
        vec![
            PortSpec {
                name: "mem",
                protocol: mem_v1::PROTOCOL,
                role: Role::Target,
            },
            PortSpec {
                name: "irq",
                protocol: irq_v0::PROTOCOL,
                role: Role::Initiator,
            },
        ]
    );
    assert_eq!((MEM_PORT, IRQ_PORT), (PortId(0), PortId(1)));
    assert_eq!(SIZE, 12);
    let mut ctx = MockCtx::new(Phase::Request);
    t.init(&mut ctx).unwrap();
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    assert_eq!(
        (t.compare(), t.control(), t.expired(), t.generation()),
        (100, 0, false, 0)
    );
    assert_eq!(read_reg(&mut t, COMPARE), 100);
    assert_eq!(read_reg(&mut t, CONTROL), 0);
    assert_eq!(read_reg(&mut t, STATUS), 0);
}

#[test]
fn compare_keeps_any_nonzero_value_and_faults_on_zero() {
    let mut t = timer();
    for v in [1, 7, u32::MAX] {
        let ctx = write_reg(&mut t, COMPARE, v);
        assert!(ctx.irqs.is_empty() && ctx.wakes.is_empty() && ctx.traced.is_empty());
        assert_eq!(read_reg(&mut t, COMPARE), v);
    }
    let (resp, _) = access(&mut t, write(3, COMPARE, &word(0)));
    assert!(fault_of(&resp));
    assert_eq!(t.compare(), u32::MAX);
}

#[test]
fn control_keeps_enable_and_periodic_only() {
    let mut t = timer();
    write_reg(&mut t, CONTROL, u32::MAX & !ENABLE);
    assert_eq!(read_reg(&mut t, CONTROL), PERIODIC);
    write_reg(&mut t, CONTROL, u32::MAX);
    assert_eq!(read_reg(&mut t, CONTROL), PERIODIC | ENABLE);
}

#[test]
fn malformed_requests_fault_and_change_nothing() {
    let mut t = timer();
    write_reg(&mut t, COMPARE, 5);
    let before = snapshot_of(&t);
    let cases = [
        read(1, COMPARE, 2),
        read(1, 0x2, 4),
        read(1, SIZE, 4),
        read(1, u64::MAX - 1, 4),
        write(1, CONTROL, &[1, 0]),
        write(1, CONTROL, &[1, 0, 0, 0, 0, 0, 0, 0]),
        write(1, 0x6, &word(1)),
        write(1, SIZE, &word(1)),
        write(1, u64::MAX, &word(1)),
    ];
    for msg in cases {
        let (resp, ctx) = access(&mut t, msg.clone());
        assert!(fault_of(&resp), "{msg:?}");
        assert!(ctx.irqs.is_empty() && ctx.wakes.is_empty(), "{msg:?}");
        assert_eq!(snapshot_of(&t), before, "{msg:?}");
    }
}

#[test]
fn protocol_violations_fault_the_session() {
    let mut t = timer();
    let mut ctx = MockCtx::new(Phase::Request);
    let errors = [
        ctx.deliver(&mut t, MEM_PORT, read(1, COMPARE, 0)),
        ctx.deliver(&mut t, MEM_PORT, write(1, COMPARE, &[])),
        ctx.deliver(
            &mut t,
            MEM_PORT,
            MemMsg::ReadResp {
                txn: TxnId(1),
                outcome: ReadOutcome::Data { data: word(0) },
            },
        ),
        ctx.deliver_msg(
            &mut t,
            MEM_PORT,
            Message::Irq(IrqMsg::Level { asserted: true }),
        ),
        ctx.deliver_msg(
            &mut t,
            IRQ_PORT,
            Message::Irq(IrqMsg::Level { asserted: true }),
        ),
        ctx.deliver(&mut t, PortId(2), read(1, COMPARE, 4)),
    ];
    for e in errors {
        assert!(matches!(e, Err(SimError::ComponentFault(_))), "{e:?}");
    }
    assert!(ctx.order.is_empty());
}

// ---------------------------------------------------------------------------------------
// Arming, expiry, and the line.

#[test]
fn enabling_arms_one_wake_compare_cycles_ahead() {
    let mut t = timer();
    write_reg(&mut t, COMPARE, 40);
    let ctx = write_reg(&mut t, CONTROL, ENABLE);
    assert_eq!(ctx.wakes, [arm_wake(40, 1)]);
    assert!(ctx.irqs.is_empty());
    assert_eq!(
        ctx.traced,
        [(ARM_KIND, vec![("cycles", u(40)), ("generation", u(1))])]
    );
    // Rewriting ENABLE, or COMPARE, while counting does not re-arm.
    assert!(write_reg(&mut t, CONTROL, ENABLE).wakes.is_empty());
    assert!(write_reg(&mut t, COMPARE, 9).wakes.is_empty());
    assert_eq!(t.generation(), 1);
}

#[test]
fn a_one_shot_expiry_raises_the_line_and_clears_enable() {
    let mut t = timer();
    write_reg(&mut t, CONTROL, ENABLE);
    let ctx = wake(&mut t, 1);
    assert_eq!(ctx.irqs, [line(true)]);
    assert!(ctx.wakes.is_empty());
    assert_eq!(
        ctx.traced,
        [(
            EXPIRE_KIND,
            vec![("generation", u(1)), ("periodic", Value::Bool(false))]
        )]
    );
    assert_eq!(read_reg(&mut t, CONTROL), 0);
    assert_eq!(read_reg(&mut t, STATUS), EXPIRED);
    // The same token again is stale: the timer is disabled.
    let ctx = wake(&mut t, 1);
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
}

#[test]
fn a_periodic_expiry_rearms_from_the_expiry_with_the_current_compare() {
    let mut t = timer();
    write_reg(&mut t, COMPARE, 10);
    write_reg(&mut t, CONTROL, ENABLE | PERIODIC);
    write_reg(&mut t, COMPARE, 25);
    let ctx = wake(&mut t, 1);
    assert_eq!(ctx.irqs, [line(true)]);
    assert_eq!(ctx.wakes, [arm_wake(25, 2)]);
    assert_eq!(ctx.traced.len(), 2);
    assert_eq!(ctx.traced[1].0, ARM_KIND);
    // A second expiry with the line still high sends nothing on irq.
    let ctx = wake(&mut t, 2);
    assert!(ctx.irqs.is_empty());
    assert_eq!(ctx.wakes, [arm_wake(25, 3)]);
    assert_eq!(read_reg(&mut t, CONTROL), ENABLE | PERIODIC);
}

#[test]
fn writing_one_to_status_drops_the_line_and_zero_does_nothing() {
    let mut t = timer();
    write_reg(&mut t, CONTROL, ENABLE);
    wake(&mut t, 1);
    let ctx = write_reg(&mut t, STATUS, u32::MAX & !EXPIRED);
    assert!(ctx.irqs.is_empty());
    assert!(t.expired());
    let ctx = write_reg(&mut t, STATUS, EXPIRED);
    assert_eq!(ctx.irqs, [line(false)]);
    assert_eq!(read_reg(&mut t, STATUS), 0);
    // Clearing an already clear STATUS sends nothing.
    assert!(write_reg(&mut t, STATUS, EXPIRED).irqs.is_empty());
}

#[test]
fn disabling_cancels_the_pending_expiry() {
    let mut t = timer();
    write_reg(&mut t, CONTROL, ENABLE | PERIODIC);
    let ctx = write_reg(&mut t, CONTROL, PERIODIC);
    assert!(ctx.irqs.is_empty() && ctx.wakes.is_empty() && ctx.traced.is_empty());
    assert_eq!(t.generation(), 2);
    let ctx = wake(&mut t, 1);
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    assert!(!t.expired());
    // Re-enabling arms a fresh generation; only its wake expires.
    assert_eq!(write_reg(&mut t, CONTROL, ENABLE).wakes, [arm_wake(100, 3)]);
    assert!(wake(&mut t, 1).order.is_empty());
    assert!(wake(&mut t, 2).order.is_empty());
    assert_eq!(wake(&mut t, 3).irqs, [line(true)]);
}

#[test]
fn inspect_shows_the_registers_and_the_generation() {
    let mut t = timer();
    write_reg(&mut t, COMPARE, 3);
    write_reg(&mut t, CONTROL, ENABLE);
    wake(&mut t, 1);
    assert_eq!(
        t.inspect().fields,
        vec![
            ("compare", u(3)),
            ("control", u(0)),
            ("expired", Value::Bool(true)),
            ("generation", u(1)),
        ]
    );
}

// ---------------------------------------------------------------------------------------
// Snapshots.

#[test]
fn snapshots_round_trip_and_reject_bad_state() {
    let mut t = timer();
    write_reg(&mut t, COMPARE, 7);
    write_reg(&mut t, CONTROL, ENABLE | PERIODIC);
    wake(&mut t, 1);
    assert_eq!(t.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    let bytes = snapshot_of(&t);
    let mut restored = timer();
    restore_into(&mut restored, &bytes).unwrap();
    assert_eq!(snapshot_of(&restored), bytes);
    assert_eq!(restored.inspect(), t.inspect());

    let n = bytes.len();
    // compare (4), control (4), expired (1), generation (8) close the snapshot.
    let mut zero = bytes.clone();
    zero[n - 17..n - 13].copy_from_slice(&[0; 4]);
    let mut reserved = bytes.clone();
    reserved[n - 13] |= 1 << 2;
    let mut flag = bytes.clone();
    flag[n - 9] = 2;
    let other = SimpleTimer::new(TimerConfig {
        compare_reset: NonZeroU32::new(101).unwrap(),
        ..config()
    });
    for bad in [
        zero,
        reserved,
        flag,
        snapshot_of(&other),
        bytes[..n - 1].to_vec(),
    ] {
        let mut fresh = timer();
        let before = snapshot_of(&fresh);
        assert!(restore_into(&mut fresh, &bad).is_err());
        assert_eq!(snapshot_of(&fresh), before);
    }
    let mut fresh = timer();
    let other = snapshot_of(&other);
    let mut r = SnapshotReader::new(&other);
    assert!(matches!(
        fresh.restore(&mut r, SNAPSHOT_SCHEMA),
        Err(RestoreError::InvalidState(_))
    ));
}

// ---------------------------------------------------------------------------------------
// In a runtime.

/// Records nothing: the runtime's dispatch records show each level.
struct Sink;

impl Component for Sink {
    fn type_name(&self) -> &'static str {
        "test.irq_sink"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "irq",
            protocol: irq_v0::PROTOCOL,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Message {
                msg: Message::Irq(_),
                ..
            } if ctx.phase() == Phase::Complete => Ok(()),
            _ => Err(SimError::ComponentFault(
                "irq sink: not a level in COMPLETE",
            )),
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// The script's traffic: arm a periodic timer of 20 cycles at cycle 2, acknowledge at
/// cycle 30, disable at cycle 50, and read STATUS at cycle 90.
fn traffic() -> Vec<(u64, MemMsg)> {
    vec![
        (0, write(1, COMPARE, &word(20))),
        (2, write(2, CONTROL, &word(ENABLE | PERIODIC))),
        (30, write(3, STATUS, &word(EXPIRED))),
        (50, write(4, CONTROL, &word(0))),
        (90, read(5, STATUS, 4)),
    ]
}

fn build() -> Runtime {
    let mut b = TopologyBuilder::new(SimulationClock::default());
    let clock = b
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
    let script = b.add_component(
        "soc.script",
        Box::new(Script {
            clock,
            requests: traffic(),
        }),
    );
    let t = SimpleTimer::new(TimerConfig {
        clock,
        latency: cycles(0),
        compare_reset: NonZeroU32::new(1).unwrap(),
    });
    let t = b.add_component("soc.timer", Box::new(t));
    let sink = b.add_component("soc.sink", Box::new(Sink));
    b.connect((script, "mem"), (t, "mem"), Some(cycles(1)));
    b.connect((t, "irq"), (sink, "irq"), Some(cycles(1)));
    b.elaborate(SessionConfig::default()).unwrap()
}

fn level_of(ev: &Dispatched) -> Option<bool> {
    match ev.delivery {
        Delivered::Message {
            msg: Message::Irq(IrqMsg::Level { asserted }),
            ..
        } => Some(asserted),
        _ => None,
    }
}

#[test]
fn expiries_land_compare_cycles_after_arming_and_stop_when_disabled() {
    let mut rt = build();
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    let period = {
        // Requests sent at cycles 0 and 2 arrive one cycle later each.
        let requests: Vec<Tick> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.delivery,
                    Delivered::Message {
                        msg: Message::MemV1(MemMsg::WriteReq { .. }),
                        ..
                    }
                )
            })
            .map(|e| e.key.tick)
            .collect();
        (requests[1].0 - requests[0].0) / 2
    };
    let cycle = |e: &Dispatched| e.key.tick.0 / period;
    let wakes: Vec<u64> = events
        .iter()
        .filter(|e| matches!(e.delivery, Delivered::Wake { .. }))
        .map(cycle)
        .collect();
    // Armed at cycle 3 (the write arrives a cycle after it is sent): expiries at 23 and
    // 43; the one due at 63 is stale after the disable at 51.
    assert_eq!(wakes, [23, 43, 63]);
    let levels: Vec<(u64, bool)> = events
        .iter()
        .filter_map(|e| level_of(e).map(|l| (cycle(e), l)))
        .collect();
    // Raised at 23, dropped by the acknowledge at 31, raised again at 43, each one cycle
    // on the link.
    assert_eq!(levels, [(24, true), (32, false), (44, true)]);
    let last = events.last().unwrap();
    assert!(matches!(
        &last.delivery,
        Delivered::Message {
            msg: Message::MemV1(MemMsg::ReadResp {
                outcome: ReadOutcome::Data { data },
                ..
            }),
            ..
        } if data == &word(EXPIRED)
    ));
}

#[test]
fn a_timer_run_restores_at_every_event() {
    let mut reference = build();
    reference.init().unwrap();
    let all: Vec<Dispatched> = std::iter::from_fn(|| reference.step().unwrap()).collect();
    let end = reference.snapshot().unwrap();
    for k in 0..all.len() {
        let mut rt = build();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let snapshot = rt.snapshot().unwrap();
        let mut restored = build();
        restored.restore(&snapshot).unwrap();
        let rest: Vec<Dispatched> = std::iter::from_fn(|| restored.step().unwrap()).collect();
        assert_eq!(rest, all[k..], "{k}");
        assert_eq!(restored.snapshot().unwrap(), end, "{k}");
    }
}
