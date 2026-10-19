//! Perturbation C, tie order (`docs/m5-design.md` §7.3): permuted ties, untouched tick
//! order, and snapshots.

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_runtime::jitter::{Jitter, LinkSet};
use systemscope_runtime::runtime::{Dispatched, Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::scheduler::SchedulerConfig;
use systemscope_runtime::snapshot::{SNAPSHOT_FORMAT_VERSION, TIED_FORMAT_VERSION};
use systemscope_runtime::ties::{self, Ties};
use systemscope_runtime::topology::TopologyBuilder;

/// Messages the sender sends.
const BURST: u64 = 32;
/// Ticks per cycle of the test clock (1 GHz on the default 1 ps tick).
const CYCLE: u64 = 1000;

/// Sends `BURST` reads at init: the first half at tick 0, the rest in pairs a cycle apart,
/// so deliveries tie in groups.
struct Sender;

impl Component for Sender {
    fn type_name(&self) -> &'static str {
        "test.sender"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Initiator,
        }]
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for txn in 0..BURST {
            let when = match txn {
                0..16 => ScheduleWhen::Now,
                _ => ScheduleWhen::After(Duration::from_ps(txn / 2 * CYCLE)),
            };
            ctx.send(
                PortId(0),
                MemMsg::ReadReq {
                    txn: TxnId(txn),
                    addr: 0,
                    len: 4,
                }
                .into(),
                when,
                Phase::Request,
            )?;
        }
        Ok(())
    }
    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, _: &mut SnapshotWriter) {}
    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// Accepts everything.
struct Sink;

impl Component for Sink {
    fn type_name(&self) -> &'static str {
        "test.sink"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Target,
        }]
    }
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }
    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, _: &mut SnapshotWriter) {}
    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

fn platform(ties: Option<Ties>, jitter: Option<Jitter>) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    );
    assert_eq!(clock, Ok(ClockDomainId(0)));
    let s = t.add_component("soc.sender", Box::new(Sender));
    let r = t.add_component("soc.sink", Box::new(Sink));
    let latency = LinkLatency::Cycles {
        domain: ClockDomainId(0),
        k: 2,
    };
    t.connect((s, "mem"), (r, "mem"), Some(latency));
    t.elaborate(SessionConfig {
        scheduler: SchedulerConfig {
            ties,
            ..SchedulerConfig::default()
        },
        jitter,
        ..SessionConfig::default()
    })
    .unwrap()
}

fn run(ties: Option<Ties>, jitter: Option<Jitter>) -> Vec<Dispatched> {
    let mut rt = platform(ties, jitter);
    rt.init().unwrap();
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

/// `(delivery tick, txn)` in dispatch order.
fn deliveries(events: &[Dispatched]) -> Vec<(u64, u64)> {
    events
        .iter()
        .map(|d| match &d.delivery {
            Delivered::Message {
                msg: Message::Mem(MemMsg::ReadReq { txn, .. }),
                ..
            } => (d.key.tick.0, txn.0),
            other => panic!("unexpected delivery {other:?}"),
        })
        .collect()
}

#[test]
fn the_context_is_pinned() {
    assert_eq!(ties::TIES_CONTEXT, "SystemScope 2026-10 ties v1");
    assert_eq!((ties::COUNTER_BITS, ties::RANK_BITS), (40, 24));
}

#[test]
fn ties_permute_equal_ticks_and_keep_tick_order() {
    let plain = deliveries(&run(None, None));
    assert!(
        plain.windows(2).all(|w| w[0] < w[1]),
        "ascending txn per tick"
    );
    let tied = deliveries(&run(Some(Ties { seed: 1 }), None));
    assert_eq!(tied, deliveries(&run(Some(Ties { seed: 1 }), None)));
    assert_ne!(tied, plain);
    assert!(
        tied.windows(2).all(|w| w[0].0 <= w[1].0),
        "ticks never go back"
    );
    let mut sorted = tied.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, plain);
    assert_ne!(tied, deliveries(&run(Some(Ties { seed: 2 }), None)));
}

#[test]
fn snapshots_carry_the_ties_and_restore_only_into_the_same_setting() {
    let version = |rt: &Runtime| {
        let s = rt.snapshot().unwrap();
        u32::from_le_bytes(s[8..12].try_into().unwrap())
    };
    let jitter = Jitter {
        seed: 3,
        max_cycles: 2,
        links: LinkSet::of(&[0]),
        only: None,
    };
    let t = Ties { seed: 5 };
    for j in [None, Some(jitter.clone())] {
        let reference = run(Some(t), j.clone());
        let mut rt = platform(Some(t), j.clone());
        rt.init().unwrap();
        assert_eq!(version(&rt), TIED_FORMAT_VERSION);
        for k in 0..reference.len() {
            let snapshot = rt.snapshot().unwrap();
            let mut restored = platform(Some(t), j.clone());
            restored.restore(&snapshot).unwrap();
            assert_eq!(restored.snapshot().unwrap(), snapshot, "{k}");
            let rest: Vec<Dispatched> = std::iter::from_fn(|| restored.step().unwrap()).collect();
            assert_eq!(rest, reference[k..], "{k}");
            let other_jitter = match j {
                None => Some(jitter.clone()),
                Some(_) => None,
            };
            for (ties, jitter) in [
                (None, j.clone()),
                (Some(Ties { seed: 6 }), j.clone()),
                (Some(t), other_jitter),
            ] {
                assert!(
                    matches!(
                        platform(ties, jitter.clone()).restore(&snapshot),
                        Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
                    ),
                    "{k} into {ties:?} {jitter:?}"
                );
            }
            rt.step().unwrap().unwrap();
        }
    }
    // A tied setting refuses an untied snapshot.
    let mut plain = platform(None, None);
    plain.init().unwrap();
    assert_eq!(version(&plain), SNAPSHOT_FORMAT_VERSION);
    assert!(matches!(
        platform(Some(t), None).restore(&plain.snapshot().unwrap()),
        Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
    ));
}
