//! Scripted schedules (`docs/m6-design.md` §7): identities, delays, group orders with
//! effective ranks, vacuous entries, snapshots, and forks.

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
use systemscope_runtime::script::{EventId, Script};
use systemscope_runtime::snapshot::{SCRIPTED_FORMAT_VERSION, SNAPSHOT_FORMAT_VERSION};
use systemscope_runtime::ties::Ties;
use systemscope_runtime::topology::{ElaborationError, TopologyBuilder};

/// Ticks per cycle of the test clock (1 GHz on the default 1 ps tick).
const CYCLE: u64 = 1000;

/// At init: reads 0 and 1 now and read 2 a cycle later, all delivered two cycles after
/// they are sent, and wakes 1 and 2 at cycle 2, in the reads' `(tick, phase)`. Wake 1
/// sets a flag; wake 2 wakes 3 now if the flag is set; wake 3 sends read 3.
#[derive(Default)]
struct Sender {
    flag: bool,
}

fn read(txn: u64) -> Message {
    MemMsg::ReadReq {
        txn: TxnId(txn),
        addr: 0,
        len: 4,
    }
    .into()
}

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
        ctx.send(PortId(0), read(0), ScheduleWhen::Now, Phase::Request)?;
        ctx.send(PortId(0), read(1), ScheduleWhen::Now, Phase::Request)?;
        let cycle = ScheduleWhen::After(Duration::from_ps(CYCLE));
        ctx.send(PortId(0), read(2), cycle, Phase::Request)?;
        let two = ScheduleWhen::After(Duration::from_ps(2 * CYCLE));
        ctx.wake_self(two, Phase::Request, 1)?;
        ctx.wake_self(two, Phase::Request, 2)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Wake { token: 1 } => self.flag = true,
            Delivered::Wake { token: 2 } if self.flag => {
                ctx.wake_self(ScheduleWhen::Now, Phase::Request, 3)?;
            }
            Delivered::Wake { token: 3 } => {
                ctx.send(PortId(0), read(3), ScheduleWhen::Now, Phase::Request)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        w.bool(self.flag);
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        self.flag = r.bool()?;
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

fn elaborate(config: SessionConfig, latency: LinkLatency) -> Result<Runtime, ElaborationError> {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    );
    assert_eq!(clock, Ok(ClockDomainId(0)));
    let s = t.add_component("soc.sender", Box::<Sender>::default());
    let r = t.add_component("soc.sink", Box::new(Sink));
    t.connect((s, "mem"), (r, "mem"), Some(latency));
    t.elaborate(config)
}

const TWO_CYCLES: LinkLatency = LinkLatency::Cycles {
    domain: ClockDomainId(0),
    k: 2,
};

fn platform(script: Option<Script>) -> Runtime {
    elaborate(
        SessionConfig {
            script,
            ..SessionConfig::default()
        },
        TWO_CYCLES,
    )
    .unwrap()
}

fn run(script: Option<Script>) -> Vec<Dispatched> {
    let mut rt = platform(script);
    rt.init().unwrap();
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

fn msg(ordinal: u64) -> EventId {
    EventId::Msg {
        link: 0,
        dir: 0,
        ordinal,
    }
}

fn wake(ordinal: u64) -> EventId {
    EventId::Wake {
        component: 0,
        ordinal,
    }
}

fn grouped(group: Vec<EventId>) -> Option<Script> {
    Some(Script {
        delays: Vec::new(),
        groups: vec![group],
    })
}

/// `(cycle, "r<txn>" or "w<token>")` in dispatch order.
fn order(events: &[Dispatched]) -> Vec<(u64, String)> {
    events
        .iter()
        .map(|d| {
            let what = match &d.delivery {
                Delivered::Message {
                    msg: Message::Mem(MemMsg::ReadReq { txn, .. }),
                    ..
                } => format!("r{}", txn.0),
                Delivered::Wake { token } => format!("w{token}"),
                other => panic!("unexpected delivery {other:?}"),
            };
            (d.key.tick.0 / CYCLE, what)
        })
        .collect()
}

fn expect(order: &[(u64, &str)]) -> Vec<(u64, String)> {
    order.iter().map(|&(c, w)| (c, w.to_owned())).collect()
}

fn version(rt: &Runtime) -> u32 {
    let s = rt.snapshot().unwrap();
    u32::from_le_bytes(s[8..12].try_into().unwrap())
}

#[test]
fn an_empty_script_dispatches_as_the_unscripted_session() {
    let plain = run(None);
    let scripted = run(Some(Script::default()));
    assert_eq!(
        order(&plain),
        expect(&[
            (2, "r0"),
            (2, "r1"),
            (2, "w1"),
            (2, "w2"),
            (2, "w3"),
            (3, "r2"),
            (4, "r3"),
        ])
    );
    let strip = |events: &[Dispatched]| -> Vec<Dispatched> {
        events
            .iter()
            .cloned()
            .map(|d| Dispatched { id: None, ..d })
            .collect()
    };
    assert_eq!(strip(&scripted), plain);
    assert!(plain.iter().all(|d| d.id.is_none()));
    let ids: Vec<EventId> = scripted.iter().map(|d| d.id.unwrap()).collect();
    assert_eq!(
        ids,
        [msg(0), msg(1), wake(0), wake(1), wake(2), msg(2), msg(3)]
    );
    let digest = |script| {
        let mut rt = platform(script);
        rt.init().unwrap();
        while rt.step().unwrap().is_some() {}
        rt.execution_digest()
    };
    assert_eq!(digest(Some(Script::default())), digest(None));
}

#[test]
fn a_delay_moves_its_message_and_the_lane_stays_fifo() {
    let delayed = |delays| {
        order(&run(Some(Script {
            delays,
            groups: Vec::new(),
        })))
    };
    // Read 0 two cycles later: reads 1 and 2 follow it on the lane.
    assert_eq!(
        delayed(vec![((0, 0, 0), 2)]),
        expect(&[
            (2, "w1"),
            (2, "w2"),
            (2, "w3"),
            (4, "r0"),
            (4, "r1"),
            (4, "r2"),
            (4, "r3"),
        ])
    );
    // Read 3 a cycle later: nothing follows it.
    assert_eq!(
        delayed(vec![((0, 0, 3), 1)]),
        expect(&[
            (2, "r0"),
            (2, "r1"),
            (2, "w1"),
            (2, "w2"),
            (2, "w3"),
            (3, "r2"),
            (5, "r3"),
        ])
    );
}

#[test]
fn a_group_order_reorders_its_group_under_fifo_and_leaves_a_vacuous_entry() {
    // Ranks w2 0, r1 1, r0 2, w1 3, w3 4: read 1 is raised behind read 0, and wake 2,
    // first, finds no flag, so wake 3 never occurs.
    let script = grouped(vec![wake(1), msg(1), msg(0), wake(0), wake(2)]);
    let mut rt = platform(script.clone());
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(
        order(&events),
        expect(&[(2, "w2"), (2, "r0"), (2, "r1"), (2, "w1"), (3, "r2")])
    );
    assert_eq!(rt.script().unwrap().unscheduled(), [wake(2)]);
    assert_eq!(platform(script).script().unwrap().unscheduled().len(), 5);
}

#[test]
fn an_event_scheduled_into_its_own_group_is_raised_to_the_floor() {
    // Wake 3 is ranked 0 but is scheduled while wake 2, rank 2, is dispatched: it runs
    // right after wake 2 and before read 0, rank 3.
    let script = grouped(vec![wake(2), wake(0), wake(1), msg(0), msg(1)]);
    assert_eq!(
        order(&run(script)),
        expect(&[
            (2, "w1"),
            (2, "w2"),
            (2, "w3"),
            (2, "r0"),
            (2, "r1"),
            (3, "r2"),
            (4, "r3"),
        ])
    );
}

#[test]
fn a_scripted_snapshot_restores_at_every_event_only_into_the_same_script() {
    let script = grouped(vec![wake(1), msg(1), msg(0), wake(0)]);
    let reference = run(script.clone());
    let mut rt = platform(script.clone());
    rt.init().unwrap();
    assert_eq!(version(&rt), SCRIPTED_FORMAT_VERSION);
    let refused = |rt: Result<(), RuntimeError>| {
        matches!(
            rt,
            Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
        )
    };
    for k in 0..reference.len() {
        let snapshot = rt.snapshot().unwrap();
        let mut restored = platform(script.clone());
        restored.restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot, "{k}");
        let rest: Vec<Dispatched> = std::iter::from_fn(|| restored.step().unwrap()).collect();
        assert_eq!(rest, reference[k..], "{k}");
        for other in [None, Some(Script::default())] {
            assert!(refused(platform(other).restore(&snapshot)), "{k}");
        }
        rt.step().unwrap().unwrap();
    }
    let mut plain = platform(None);
    plain.init().unwrap();
    assert_eq!(version(&plain), SNAPSHOT_FORMAT_VERSION);
    let plain = plain.snapshot().unwrap();
    assert!(refused(platform(Some(Script::default())).restore(&plain)));
    assert!(refused(platform(Some(Script::default())).fork(&plain)));
    assert!(refused(platform(None).fork(&plain)));
}

#[test]
fn a_fork_resumes_a_parent_as_the_child_runs_from_init() {
    // The child delays read 3, sent while wake 3 is dispatched: it extends the parent's
    // empty script up to that event.
    let child = Script {
        delays: vec![((0, 0, 3), 1)],
        groups: Vec::new(),
    };
    let reference = run(Some(child.clone()));
    let parent_run = run(Some(Script::default()));
    let mut parent = platform(Some(Script::default()));
    parent.init().unwrap();
    for k in 0..parent_run.len() {
        let snapshot = parent.snapshot().unwrap();
        let mut forked = platform(Some(child.clone()));
        let sent = parent_run[..k].iter().any(|d| d.id == Some(wake(2)));
        if sent {
            assert!(
                matches!(
                    forked.fork(&snapshot),
                    Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
                ),
                "{k}"
            );
        } else {
            forked.fork(&snapshot).unwrap();
            let rest: Vec<Dispatched> = std::iter::from_fn(|| forked.step().unwrap()).collect();
            assert_eq!(rest, reference[k..], "{k}");
            assert_eq!(forked.script().unwrap().script, child);
        }
        parent.step().unwrap().unwrap();
    }
    // A child that drops the parent's entry is no extension.
    let mut parent = platform(Some(child));
    parent.init().unwrap();
    assert!(matches!(
        platform(Some(Script::default())).fork(&parent.snapshot().unwrap()),
        Err(RuntimeError::Restore(RestoreError::InvalidState(_)))
    ));
}

#[test]
fn elaboration_refuses_a_script_it_cannot_honor() {
    let refusal = |script: Script, ties, jitter, latency| match elaborate(
        SessionConfig {
            scheduler: SchedulerConfig {
                ties,
                ..SchedulerConfig::default()
            },
            jitter,
            script: Some(script),
            ..SessionConfig::default()
        },
        latency,
    ) {
        Err(ElaborationError::InvalidScript(why)) => why,
        other => panic!("{:?}", other.map(|_| ())),
    };
    let delay = |id, cycles| Script {
        delays: vec![(id, cycles)],
        groups: Vec::new(),
    };
    let jitter = Jitter {
        seed: 1,
        max_cycles: 1,
        links: LinkSet::of(&[0]),
        only: None,
    };
    let ticks = LinkLatency::After(Duration::from_ps(5));
    let cases = [
        (Script::default(), Some(Ties { seed: 1 }), None, TWO_CYCLES),
        (Script::default(), None, Some(jitter), TWO_CYCLES),
        (delay((0, 0, 0), 0), None, None, TWO_CYCLES),
        (delay((1, 0, 0), 1), None, None, TWO_CYCLES),
        (delay((0, 2, 0), 1), None, None, TWO_CYCLES),
        (delay((0, 0, 0), 1), None, None, ticks),
        (
            grouped(vec![msg(0), wake(0)])
                .map(|mut s| {
                    s.groups[0][1] = EventId::Wake {
                        component: 2,
                        ordinal: 0,
                    };
                    s
                })
                .unwrap(),
            None,
            None,
            TWO_CYCLES,
        ),
        (
            grouped(vec![msg(0), msg(0)]).unwrap(),
            None,
            None,
            TWO_CYCLES,
        ),
    ];
    let reasons: Vec<&str> = cases
        .into_iter()
        .map(|(s, t, j, l)| refusal(s, t, j, l))
        .collect();
    assert_eq!(
        reasons,
        [
            "a script comes with jitter or ties",
            "a script comes with jitter or ties",
            "a delay is 0 cycles",
            "a delayed message is on no declared link end",
            "a delayed message is on no declared link end",
            "a delayed message's link has no Cycles latency",
            "a group order names an undeclared link end or component",
            "an event is listed twice",
        ]
    );
}
