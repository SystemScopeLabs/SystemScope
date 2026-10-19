//! Perturbation B, per-message jitter (`docs/m5-design.md` §7.2): draws, FIFO delivery,
//! elaboration checks, snapshots, and the minimized setting's list of messages.

use systemscope_contracts::canonical::{Decoder, Encoder};
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
use systemscope_runtime::jitter::{Jitter, JitterState, Lane, LinkSet};
use systemscope_runtime::runtime::{Dispatched, Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::snapshot::{
    JITTERED_FORMAT_VERSION, LISTED_FORMAT_VERSION, SNAPSHOT_FORMAT_VERSION,
};
use systemscope_runtime::topology::{ElaborationError, TopologyBuilder};

/// Messages the sender sends.
const BURST: u64 = 32;
/// Ticks per cycle of the test clock (1 GHz on the default 1 ps tick).
const CYCLE: u64 = 1000;

/// Sends `BURST` reads at init, one per cycle-aligned tick apart from the first few,
/// which share tick 0.
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
                0..4 => ScheduleWhen::Now,
                _ => ScheduleWhen::After(Duration::from_ps(txn * CYCLE / 2)),
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

fn platform(
    latency: Option<LinkLatency>,
    jitter: Option<Jitter>,
) -> Result<Runtime, ElaborationError> {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    );
    assert_eq!(clock, Ok(ClockDomainId(0)));
    let s = t.add_component("soc.sender", Box::new(Sender));
    let r = t.add_component("soc.sink", Box::new(Sink));
    t.connect((s, "mem"), (r, "mem"), latency);
    t.elaborate(SessionConfig {
        jitter,
        ..SessionConfig::default()
    })
}

fn cycles(k: u64) -> Option<LinkLatency> {
    Some(LinkLatency::Cycles {
        domain: ClockDomainId(0),
        k,
    })
}

fn jitter(seed: u64, max_cycles: u64) -> Jitter {
    Jitter {
        seed,
        max_cycles,
        links: LinkSet::of(&[0]),
        only: None,
    }
}

fn run(jitter: Option<Jitter>) -> Vec<Dispatched> {
    let mut rt = platform(cycles(2), jitter).unwrap();
    rt.init().unwrap();
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

/// `(txn, delivery tick)` in dispatch order.
fn deliveries(events: &[Dispatched]) -> Vec<(u64, u64)> {
    events
        .iter()
        .map(|d| match &d.delivery {
            Delivered::Message {
                msg: Message::Mem(MemMsg::ReadReq { txn, .. }),
                ..
            } => (txn.0, d.key.tick.0),
            other => panic!("unexpected delivery {other:?}"),
        })
        .collect()
}

#[test]
fn draws_are_deterministic_bounded_and_keyed_by_every_input() {
    let j = jitter(42, 5);
    let draws: Vec<u64> = (0..1000).map(|n| j.draw(0, 0, n)).collect();
    assert_eq!(
        draws,
        (0..1000).map(|n| j.draw(0, 0, n)).collect::<Vec<_>>()
    );
    assert!(draws.iter().all(|&d| d <= 5));
    for v in 0..=5 {
        assert!(draws.contains(&v), "{v} is drawn");
    }
    let differs = |f: &dyn Fn(u64) -> u64| (0..64).any(|n| f(n) != j.draw(0, 0, n));
    assert!(differs(&|n| jitter(43, 5).draw(0, 0, n)), "seed");
    assert!(differs(&|n| j.draw(1, 0, n)), "link");
    assert!(differs(&|n| j.draw(0, 1, n)), "direction");
    assert!(differs(&|n| j.draw(0, 0, n + 1)), "ordinal");
    assert!((0..64).all(|n| jitter(9, 1).draw(3, 1, n) <= 1));
}

#[test]
fn the_context_and_material_are_pinned() {
    let j = jitter(0x0123_4567_89ab_cdef, u64::MAX - 1);
    let mut material = Vec::new();
    material.extend_from_slice(&0x0123_4567_89ab_cdef_u64.to_le_bytes());
    material.extend_from_slice(&7u32.to_le_bytes());
    material.push(1);
    material.extend_from_slice(&9u64.to_le_bytes());
    let out = blake3::derive_key("SystemScope 2026-10 jitter v1", &material);
    let word = u64::from_le_bytes(out[..8].try_into().unwrap());
    assert_eq!(j.draw(7, 1, 9), word % u64::MAX);
}

#[test]
fn link_sets_hold_exactly_their_links() {
    let set = LinkSet::of(&[0, 3, 127]);
    assert_eq!(set.0, 1 | 1 << 3 | 1 << 127);
    assert_eq!(set.iter().collect::<Vec<_>>(), [0, 3, 127]);
    assert!(set.contains(3) && !set.contains(2) && !set.contains(128));
    assert_eq!(LinkSet::default().iter().count(), 0);
}

#[test]
fn state_round_trips_and_checks_its_lane_count() {
    let j = Jitter {
        seed: 5,
        max_cycles: 3,
        links: LinkSet::of(&[1, 4]),
        only: None,
    };
    let mut state = JitterState::new(j.clone());
    assert_eq!(state.lanes.iter().map(|l| l.0).collect::<Vec<_>>(), [1, 4]);
    assert!(state.lane(0, 0).is_none());
    *state.lane(4, 1).unwrap() = Lane {
        sent: 9,
        last: Tick(77),
    };
    let mut e = Encoder::new();
    state.encode(&mut e);
    let bytes = e.into_bytes();
    let mut d = Decoder::new(&bytes);
    let (found, lanes) = JitterState::decode(&mut d).unwrap();
    d.finish().unwrap();
    assert_eq!(found, j);
    assert_eq!(
        JitterState::with_lanes(found.clone(), lanes.clone()),
        Some(state)
    );
    assert_eq!(
        JitterState::with_lanes(found.clone(), lanes[..1].to_vec()),
        None
    );
    assert_eq!(JitterState::with_lanes(found, vec![lanes[0]; 3]), None);
}

#[test]
fn jittered_delivery_is_late_bounded_and_fifo() {
    let frozen = deliveries(&run(None));
    let max = 4;
    let jittered = deliveries(&run(Some(jitter(1, max))));
    assert_eq!(frozen.len(), BURST as usize);
    assert_eq!(jittered.len(), BURST as usize);
    // FIFO: the link delivers in send order, at non-decreasing ticks.
    assert_eq!(
        jittered.iter().map(|d| d.0).collect::<Vec<_>>(),
        (0..BURST).collect::<Vec<_>>()
    );
    assert!(jittered.windows(2).all(|w| w[0].1 <= w[1].1));
    // Never early; late by the draw or held back behind an earlier message.
    let mut late = 0;
    let mut held = 0;
    let mut last = 0;
    for (n, (&(txn, at), &(_, base))) in jittered.iter().zip(&frozen).enumerate() {
        let drawn = base + jitter(1, max).draw(0, 0, n as u64) * CYCLE;
        assert_eq!(at, drawn.max(last), "txn {txn}");
        assert!(at >= base);
        late += usize::from(at > base);
        held += usize::from(last > drawn);
        last = at;
    }
    assert!(late > 0);
    assert!(held > 0, "some message waits behind an earlier one");
    assert_eq!(deliveries(&run(Some(jitter(1, max)))), jittered);
    assert_ne!(deliveries(&run(Some(jitter(2, max)))), jittered);
}

#[test]
fn elaboration_rejects_invalid_jitter() {
    let invalid = |latency, j| match platform(latency, Some(j)) {
        Err(ElaborationError::InvalidJitter(why)) => why,
        other => panic!("{:?}", other.map(|_| ())),
    };
    assert_eq!(invalid(cycles(1), jitter(1, 0)), "max_cycles is 0");
    let none = Jitter {
        links: LinkSet::default(),
        ..jitter(1, 1)
    };
    assert_eq!(invalid(cycles(1), none), "no link is jittered");
    let missing = Jitter {
        links: LinkSet::of(&[0, 1]),
        ..jitter(1, 1)
    };
    assert_eq!(
        invalid(cycles(1), missing),
        "a jittered link does not exist"
    );
    for latency in [None, Some(LinkLatency::After(Duration::from_ns(1)))] {
        assert_eq!(
            invalid(latency, jitter(1, 1)),
            "a jittered link has no Cycles latency"
        );
    }
    assert!(platform(cycles(1), jitter(1, 1).into()).is_ok());
    let msg = ElaborationError::InvalidJitter("max_cycles is 0").to_string();
    assert_eq!(msg, "invalid jitter: max_cycles is 0");
}

#[test]
fn snapshots_carry_the_jitter_and_restore_only_into_the_same_setting() {
    let version = |rt: &Runtime| {
        let s = rt.snapshot().unwrap();
        u32::from_le_bytes(s[8..12].try_into().unwrap())
    };
    let mut plain = platform(cycles(2), None).unwrap();
    plain.init().unwrap();
    assert_eq!(version(&plain), SNAPSHOT_FORMAT_VERSION);

    let j = jitter(3, 4);
    let reference = run(Some(j.clone()));
    let mut rt = platform(cycles(2), Some(j.clone())).unwrap();
    rt.init().unwrap();
    assert_eq!(version(&rt), JITTERED_FORMAT_VERSION);
    for k in 0..reference.len() {
        let snapshot = rt.snapshot().unwrap();
        let mut restored = platform(cycles(2), Some(j.clone())).unwrap();
        restored.restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot, "{k}");
        let rest: Vec<Dispatched> = std::iter::from_fn(|| restored.step().unwrap()).collect();
        assert_eq!(rest, reference[k..], "{k}");
        for other in [None, Some(jitter(4, 4)), Some(jitter(3, 5))] {
            assert!(
                matches!(
                    platform(cycles(2), other.clone())
                        .unwrap()
                        .restore(&snapshot),
                    Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
                ),
                "{k} into {other:?}"
            );
        }
        rt.step().unwrap().unwrap();
    }
    // A jittered setting refuses an unjittered snapshot.
    let snapshot = plain.snapshot().unwrap();
    assert!(matches!(
        platform(cycles(2), Some(j)).unwrap().restore(&snapshot),
        Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
    ));
}

fn listed(seed: u64, max_cycles: u64, only: &[u64]) -> Jitter {
    Jitter {
        only: Some(only.iter().map(|&n| (0, 0, n)).collect()),
        ..jitter(seed, max_cycles)
    }
}

/// A listed setting delays exactly the listed messages, by the unlisted setting's draws.
#[test]
fn a_listed_setting_delays_only_its_messages() {
    let full = jitter(1, 4);
    let delayed: Vec<u64> = (0..BURST).filter(|&n| full.draw(0, 0, n) > 0).collect();
    assert!(delayed.len() > 4);
    let keep = [delayed[1], delayed[3]];
    let only = listed(1, 4, &keep);
    for n in 0..BURST {
        let want = if keep.contains(&n) {
            full.draw(0, 0, n)
        } else {
            0
        };
        assert_eq!(only.draw(0, 0, n), want, "{n}");
        assert_eq!(only.draw(0, 1, n), 0, "{n}");
    }
    assert_eq!(listed(1, 4, &[]).draw(0, 0, delayed[0]), 0);
    assert_eq!(
        deliveries(&run(Some(listed(1, 4, &delayed)))),
        deliveries(&run(Some(full)))
    );
    assert_eq!(
        deliveries(&run(Some(listed(1, 4, &[])))),
        deliveries(&run(None))
    );
}

#[test]
fn a_list_round_trips_and_refuses_disorder() {
    let j = listed(1, 4, &[2, 5, 9]);
    let mut e = Encoder::new();
    j.encode(&mut e);
    j.encode_only(&mut e);
    let bytes = e.into_bytes();
    let mut d = Decoder::new(&bytes);
    let mut found = Jitter::decode(&mut d).unwrap();
    found.decode_only(&mut d).unwrap();
    d.finish().unwrap();
    assert_eq!(found, j);

    let mut e = Encoder::new();
    listed(1, 4, &[5, 2]).encode_only(&mut e);
    let bytes = e.into_bytes();
    assert!(jitter(1, 4).decode_only(&mut Decoder::new(&bytes)).is_err());

    let invalid = |j| match platform(cycles(1), Some(j)) {
        Err(ElaborationError::InvalidJitter(why)) => why,
        other => panic!("{:?}", other.map(|_| ())),
    };
    assert_eq!(
        invalid(listed(1, 4, &[5, 5])),
        "the listed messages are not ascending"
    );
    for only in [vec![(1, 0, 0)], vec![(0, 2, 0)]] {
        let j = Jitter {
            only: Some(only.into()),
            ..jitter(1, 4)
        };
        assert_eq!(invalid(j), "a listed message is not on a jittered link");
    }
}

#[test]
fn a_listed_snapshot_restores_only_into_the_same_list() {
    let j = listed(3, 4, &[1, 4, 6]);
    let reference = run(Some(j.clone()));
    let mut rt = platform(cycles(2), Some(j.clone())).unwrap();
    rt.init().unwrap();
    let snapshot = rt.snapshot().unwrap();
    assert_eq!(
        u32::from_le_bytes(snapshot[8..12].try_into().unwrap()),
        LISTED_FORMAT_VERSION
    );
    for k in 0..reference.len() {
        let snapshot = rt.snapshot().unwrap();
        let mut restored = platform(cycles(2), Some(j.clone())).unwrap();
        restored.restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot, "{k}");
        let rest: Vec<Dispatched> = std::iter::from_fn(|| restored.step().unwrap()).collect();
        assert_eq!(rest, reference[k..], "{k}");
        for other in [None, Some(jitter(3, 4)), Some(listed(3, 4, &[1, 4]))] {
            assert!(
                platform(cycles(2), other.clone())
                    .unwrap()
                    .restore(&snapshot)
                    .is_err(),
                "{k} into {other:?}"
            );
        }
        rt.step().unwrap().unwrap();
    }
}
