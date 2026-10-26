//! Scripted schedules (`docs/m6-design.md` §7): identities, delays, picks, vacuous
//! entries, snapshots, and forks; and the choice points of a scripted run (§8).

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
use systemscope_runtime::choice::{BranchPoint, Choice, ChoiceError, Recording, choice_points};
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

/// The script whose only picks are `events`, at dispatches whose default is `default`.
fn picked(default: EventId, events: Vec<EventId>) -> Option<Script> {
    Some(Script {
        picks: vec![(default, events)],
        ..Script::default()
    })
}

/// `script` with `event` picked at the next dispatch whose default is `default`.
fn with_pick(script: &Script, default: EventId, event: EventId) -> Script {
    let mut child = script.clone();
    match child.picks.iter_mut().find(|(d, _)| *d == default) {
        Some((_, events)) => events.push(event),
        None => {
            child.picks.push((default, vec![event]));
            child.picks.sort();
        }
    }
    child
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
            ..Script::default()
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
fn a_pick_dispatches_a_ready_event_in_place_of_the_default() {
    // Wake 2 ahead of read 0 finds no flag, so wake 3 never occurs; read 0 is then the
    // default again, and the list is done.
    let script = picked(msg(0), vec![wake(1)]);
    let mut rt = platform(script.clone());
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(
        order(&events),
        expect(&[(2, "w2"), (2, "r0"), (2, "r1"), (2, "w1"), (3, "r2")])
    );
    assert_eq!(rt.script().unwrap().unscheduled(), []);
    assert_eq!(platform(script).script().unwrap().unscheduled().len(), 2);
    // Two picks ahead of read 0, at successive dispatches: wake 2 then finds the flag
    // and schedules wake 3 into the group, after the reads.
    assert_eq!(
        order(&run(picked(msg(0), vec![wake(0), wake(1)]))),
        expect(&[
            (2, "w1"),
            (2, "w2"),
            (2, "r0"),
            (2, "r1"),
            (2, "w3"),
            (3, "r2"),
            (4, "r3"),
        ])
    );
}

#[test]
fn a_pick_that_is_not_ready_is_vacuous_with_the_rest_of_its_list() {
    let reference = order(&run(Some(Script::default())));
    // Read 1 is behind read 0 on its lane, and wake 3 is not scheduled yet: either pick
    // dispatches the default, read 0, and the entry after it never applies.
    for first in [msg(1), wake(2)] {
        assert_eq!(
            order(&run(picked(msg(0), vec![first, wake(0)]))),
            reference,
            "{first:?}"
        );
    }
    // Wake 3, scheduled while wake 2 is dispatched, can be picked ahead of read 0 once it
    // is pending.
    assert_eq!(
        order(&run(picked(msg(0), vec![wake(0), wake(1), wake(2)]))),
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
    let script = picked(msg(0), vec![wake(0), wake(1)]);
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
        ..Script::default()
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
        ..Script::default()
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
            picked(
                msg(0),
                vec![EventId::Wake {
                    component: 2,
                    ordinal: 0,
                }],
            )
            .unwrap(),
            None,
            None,
            TWO_CYCLES,
        ),
        (
            picked(msg(0), vec![msg(0)]).unwrap(),
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
            "a pick names an undeclared link end or component",
            "a pick names an event twice",
        ]
    );
}

fn record(script: Option<Script>) -> Recording {
    let mut rt = platform(script);
    rt.init().unwrap();
    let mut rec = Recording::after_init(&rt);
    while rec.step(&mut rt).unwrap().is_some() {}
    rec
}

#[test]
fn a_recording_yields_its_picks_and_scoped_messages_in_branch_order() {
    let rec = record(Some(Script::default()));
    let points = choice_points(&rec, LinkSet::of(&[0]), &[], None).unwrap();
    let branches: Vec<BranchPoint> = points.iter().map(|c| c.branch).collect();
    let at = |at, kind, id| BranchPoint { at, kind, id };
    // Reads 0-2 and wakes 1 and 2 come from init; wake 3 from wake 2, the fourth event;
    // read 3 from wake 3, the fifth. The first three dispatches have a ready event other
    // than their default; a pick branches before what its dispatch schedules.
    assert_eq!(
        branches,
        [
            at(0, 1, msg(0)),
            at(0, 1, msg(1)),
            at(0, 1, msg(2)),
            at(1, 0, msg(0)),
            at(2, 0, msg(1)),
            at(3, 0, wake(0)),
            at(5, 1, msg(3)),
        ]
    );
    let picks: Vec<&Choice> = points[3..6].iter().map(|p| &p.choice).collect();
    // Read 1 waits behind read 0 (FIFO), and wake 3 is not pending until wake 2 runs.
    assert_eq!(
        picks,
        [
            &Choice::Pick {
                default: msg(0),
                ready: vec![wake(0), wake(1)],
            },
            &Choice::Pick {
                default: msg(1),
                ready: vec![wake(0), wake(1)],
            },
            &Choice::Pick {
                default: wake(0),
                ready: vec![wake(1)],
            },
        ]
    );
    // Only what branches after a point is offered.
    let later = choice_points(&rec, LinkSet::of(&[0]), &[], Some(branches[3])).unwrap();
    assert_eq!(later, points[4..]);
    assert_eq!(
        choice_points(&rec, LinkSet::default(), &[], None).unwrap(),
        points[3..6]
    );
    let mut plain = platform(None);
    plain.init().unwrap();
    let mut unscripted = Recording::after_init(&plain);
    while unscripted.step(&mut plain).unwrap().is_some() {}
    assert_eq!(
        choice_points(&unscripted, LinkSet::of(&[0]), &[], None),
        Err(ChoiceError::Unscripted)
    );
}

#[test]
fn the_queue_lists_every_pending_event_in_dispatch_order() {
    let script = Some(Script::default());
    let whole = run(script.clone());
    let mut rt = platform(script);
    rt.init().unwrap();
    for k in 0..=whole.len() {
        let queued = rt.queued();
        let mut rest: Vec<_> = whole[k..].iter().map(|d| (d.key, d.id)).collect();
        // Every pending event runs later in the run, in key order.
        rest.retain(|r| queued.contains(r));
        assert_eq!(rest, queued, "{k}");
        if k < whole.len() {
            rt.step().unwrap().unwrap();
        }
    }
}

/// The group at cycle 2 of every script reachable from `script` by picks after `after`,
/// with the script that reached it.
fn pick_orders(script: Script, after: Option<BranchPoint>, out: &mut Vec<(Vec<EventId>, Script)>) {
    let rec = record(Some(script.clone()));
    let group: Vec<EventId> = rec
        .steps
        .iter()
        .filter(|s| s.event.key.tick.0 == 2 * CYCLE)
        .map(|s| s.event.id.unwrap())
        .collect();
    out.push((group, script.clone()));
    for point in choice_points(&rec, LinkSet::default(), &[], after).unwrap() {
        let Choice::Pick { default, ready } = &point.choice else {
            panic!("{point:?}");
        };
        for &event in ready {
            pick_orders(with_pick(&script, *default, event), Some(point.branch), out);
        }
    }
}

#[test]
fn every_admissible_order_is_one_list_of_picks() {
    let mut reached = Vec::new();
    pick_orders(Script::default(), None, &mut reached);
    // By hand: read 0 before read 1, and wake 3 exists exactly when wake 1 precedes
    // wake 2, anywhere after wake 2.
    let mut admissible = std::collections::BTreeSet::new();
    let four = [msg(0), msg(1), wake(0), wake(1)];
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    let order = [four[a], four[b], four[c], four[d]];
                    let at = |e| order.iter().position(|&x| x == e).unwrap();
                    if [a, b, c, d]
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        < 4
                        || at(msg(1)) < at(msg(0))
                    {
                        continue;
                    }
                    if at(wake(1)) < at(wake(0)) {
                        admissible.insert(order.to_vec());
                        continue;
                    }
                    for w3 in at(wake(1)) + 1..=4 {
                        let mut with = order.to_vec();
                        with.insert(w3, wake(2));
                        admissible.insert(with);
                    }
                }
            }
        }
    }
    let orders: std::collections::BTreeSet<Vec<EventId>> =
        reached.iter().map(|(o, _)| o.clone()).collect();
    // Each order once, and every admissible one.
    assert_eq!(orders.len(), reached.len());
    assert_eq!(orders, admissible);
    for (order, script) in &reached {
        assert_eq!(&order_of(script), order, "{script:?}");
    }
}

fn order_of(script: &Script) -> Vec<EventId> {
    run(Some(script.clone()))
        .iter()
        .filter(|d| d.key.tick.0 == 2 * CYCLE)
        .map(|d| d.id.unwrap())
        .collect()
}

/// A periodic timer in miniature: its first wake at cycle 4, in `COMPLETE`, then one every
/// 4 cycles from each wake, in `REQUEST`, five in all; the `k`-th carries token `k`.
#[derive(Default)]
struct Ticker {
    count: u64,
}

const PERIOD: ScheduleWhen = ScheduleWhen::Cycles {
    domain: ClockDomainId(0),
    k: 4,
};

impl Component for Ticker {
    fn type_name(&self) -> &'static str {
        "test.ticker"
    }
    fn ports(&self) -> Vec<PortSpec> {
        Vec::new()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        ctx.wake_self(PERIOD, Phase::Complete, 0)
    }
    fn handle_event(&mut self, _: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.count += 1;
        if self.count < 5 {
            ctx.wake_self(PERIOD, Phase::Request, self.count)?;
        }
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        w.u64(self.count);
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        self.count = r.u64()?;
        Ok(())
    }
}

/// The sender and sink with a ticker as component 2.
fn ticking(script: Option<Script>) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    )
    .unwrap();
    let s = t.add_component("soc.sender", Box::<Sender>::default());
    let r = t.add_component("soc.sink", Box::new(Sink));
    t.add_component("soc.ticker", Box::<Ticker>::default());
    t.connect((s, "mem"), (r, "mem"), Some(TWO_CYCLES));
    t.elaborate(SessionConfig {
        script,
        ..SessionConfig::default()
    })
    .unwrap()
}

fn shifted(shifts: Vec<((u32, u64), i64)>) -> Option<Script> {
    Some(Script {
        shifts,
        ..Script::default()
    })
}

/// The ticks of the ticker's wakes.
fn ticks(script: Option<Script>) -> Result<Vec<u64>, RuntimeError> {
    let mut rt = ticking(script);
    rt.init()?;
    let mut out = Vec::new();
    while let Some(d) = rt.step()? {
        if d.target.0 == 2 {
            out.push(d.key.tick.0);
        }
    }
    Ok(out)
}

#[test]
fn a_shift_moves_its_wake_and_a_periodic_timer_follows() {
    let c = |cycles: u64| cycles * CYCLE;
    assert_eq!(ticks(None).unwrap(), [c(4), c(8), c(12), c(16), c(20)]);
    assert_eq!(
        ticks(Some(Script::default())).unwrap(),
        ticks(None).unwrap()
    );
    // Wake 1 two cycles later: every later wake re-arms from it.
    assert_eq!(
        ticks(shifted(vec![((2, 1), 2)])).unwrap(),
        [c(4), c(10), c(14), c(18), c(22)]
    );
    // Wake 2 three cycles earlier, and wake 4 one later.
    assert_eq!(
        ticks(shifted(vec![((2, 2), -3), ((2, 4), 1)])).unwrap(),
        [c(4), c(8), c(9), c(13), c(18)]
    );
    // Wake 0, scheduled by init, ten cycles earlier: it goes no earlier than cycle 0.
    assert_eq!(
        ticks(shifted(vec![((2, 0), -10)])).unwrap(),
        [0, c(4), c(8), c(12), c(16)]
    );
    // Wake 1, in REQUEST, scheduled while wake 0 is dispatched in COMPLETE: four cycles
    // earlier would be now in an earlier phase, so it goes to the next tick, and wake 2
    // to four cycles after the next edge.
    assert_eq!(
        ticks(shifted(vec![((2, 1), -4)])).unwrap(),
        [c(4), c(4) + 1, c(9), c(13), c(17)]
    );
}

#[test]
fn a_shifted_wake_must_be_scheduled_in_cycles() {
    // The sender's wakes are scheduled after a duration.
    assert_eq!(
        ticks(shifted(vec![((0, 0), 1)])),
        Err(RuntimeError::Faulted(SimError::ComponentFault(
            "script: a shifted wake is not scheduled in cycles"
        )))
    );
    let refused = ticking_refusal(shifted(vec![((3, 0), 1)]));
    assert_eq!(refused, "a shifted wake is of no declared component");
}

fn ticking_refusal(script: Option<Script>) -> &'static str {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    )
    .unwrap();
    t.add_component("soc.ticker", Box::<Ticker>::default());
    t.add_component("soc.other", Box::<Ticker>::default());
    t.add_component("soc.third", Box::<Ticker>::default());
    match t.elaborate(SessionConfig {
        script,
        ..SessionConfig::default()
    }) {
        Err(ElaborationError::InvalidScript(why)) => why,
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[test]
fn a_shifted_run_restores_at_every_event_and_yields_its_shift_points() {
    let script = shifted(vec![((2, 1), -3)]);
    let mut rt = ticking(script.clone());
    rt.init().unwrap();
    let mut rec = Recording::after_init(&rt);
    let mut snapshots = vec![rt.snapshot().unwrap()];
    while rec.step(&mut rt).unwrap().is_some() {
        snapshots.push(rt.snapshot().unwrap());
    }
    for (k, snapshot) in snapshots.iter().enumerate() {
        let mut restored = ticking(script.clone());
        restored.restore(snapshot).unwrap();
        let rest: Vec<Dispatched> = std::iter::from_fn(|| restored.step().unwrap()).collect();
        let reference: Vec<Dispatched> = rec.steps[k..].iter().map(|s| s.event.clone()).collect();
        assert_eq!(rest, reference, "{k}");
    }
    // Each ticker wake is a shift point, branching where it was scheduled: wake 0 by
    // init, and each later one by the ticker's previous wake.
    let points = choice_points(&rec, LinkSet::default(), &[2], None).unwrap();
    let shifts: Vec<(u64, Choice)> = points
        .iter()
        .filter(|p| matches!(p.choice, Choice::Shift(_)))
        .map(|p| (p.branch.at, p.choice.clone()))
        .collect();
    let at = |ordinal: u64| {
        if ordinal == 0 {
            0
        } else {
            let i = rec
                .steps
                .iter()
                .position(|s| {
                    s.event.id
                        == Some(EventId::Wake {
                            component: 2,
                            ordinal: ordinal - 1,
                        })
                })
                .unwrap();
            i as u64 + 1
        }
    };
    assert_eq!(
        shifts,
        (0..5)
            .map(|o| (at(o), Choice::Shift((2, o))))
            .collect::<Vec<_>>()
    );
    assert!(
        points
            .iter()
            .all(|p| p.branch.kind == 1 || matches!(p.choice, Choice::Pick { .. }))
    );
}
