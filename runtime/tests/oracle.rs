//! The oracle of `docs/m6-design.md` §17: on small synthetic topologies, an enumerator
//! that shares no code with the choice-point finder or the scripted session lists every
//! dispatch order within the bounds, and the scripts reached from the empty one through
//! choice points, other than duplicates, must reach exactly those orders, each once.
//!
//! The enumerator is a naive model of the scheduler, after `scheduler_props.rs`: a list
//! of pending events, scanned at every dispatch. It tries every ready event at each pop,
//! every delay of `0..=D` at each scoped send, and every shift of `-E..=E` at each
//! asynchronous wake. The components' behaviors are the workload, and both sides run
//! them.

use std::collections::{BTreeMap, BTreeSet};
use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_runtime::choice::{BranchPoint, Choice, Recording, choice_points};
use systemscope_runtime::jitter::LinkSet;
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::script::{EventId, Script};
use systemscope_runtime::topology::TopologyBuilder;

/// Ticks per cycle of the only clock (1 GHz on the default 1 ps tick, at phase 0).
const CYCLE: u64 = 1000;

const PORTS: [&str; 3] = ["p0", "p1", "p2"];

// ---------------------------------------------------------------------------------------
// The workload.

#[derive(Clone, Copy, Debug)]
enum Input {
    Init,
    Wake(u64),
    Msg { port: u16 },
}

#[derive(Clone, Copy, Debug)]
enum When {
    Now,
    Cycles(u64),
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Send {
        port: u16,
        payload: u64,
        when: When,
        phase: Phase,
    },
    Wake {
        when: When,
        token: u64,
        phase: Phase,
    },
}

/// A component of a synthetic topology: its ports' roles and its reactions.
#[derive(Clone, Debug)]
enum Behavior {
    /// The diamond's sides: a wake at cycle 1, which sends on port 0 now.
    Side,
    /// The diamond's join: each message, on port 0 or 1, is forwarded on port 2 as the
    /// count of messages so far times 2, plus the port.
    Join { count: u64 },
    /// The conditional wake of §7.2: wakes A (1) and B (2) at cycle 1; A sets the flag,
    /// B wakes W (3) now unless it is set, and W sends on port 0.
    Flag { flag: bool },
    /// A periodic timer: wakes at cycles 2, 4, and 6, each from the last, each sending on
    /// port 0 now in its phase: the first in `COMPLETE`, the others in `REQUEST`.
    Timer { count: u64 },
    /// Steps at cycles 1 to 4, each a cycle after the last; a message on port 0 is
    /// answered on port 1, in `COMPLETE`, with the steps taken so far.
    Worker { steps: u64 },
    /// Accepts everything.
    Sink,
}

impl Behavior {
    fn roles(&self) -> Vec<Role> {
        use Role::{Initiator, Target};
        match self {
            Behavior::Side | Behavior::Flag { .. } | Behavior::Timer { .. } => vec![Initiator],
            Behavior::Join { .. } => vec![Target, Target, Initiator],
            Behavior::Worker { .. } => vec![Target, Initiator],
            Behavior::Sink => vec![Target],
        }
    }

    fn react(&mut self, input: Input) -> Vec<Action> {
        use Phase::{Complete, Request};
        let send = |port, payload, phase| Action::Send {
            port,
            payload,
            when: When::Now,
            phase,
        };
        let wake = |k, token, phase| Action::Wake {
            when: When::Cycles(k),
            token,
            phase,
        };
        match (self, input) {
            (Behavior::Side, Input::Init) => vec![wake(1, 0, Request)],
            (Behavior::Side, Input::Wake(_)) => vec![send(0, 0, Request)],
            (Behavior::Join { count }, Input::Msg { port }) => {
                *count += 1;
                vec![send(2, *count * 2 + u64::from(port), Request)]
            }
            (Behavior::Flag { .. }, Input::Init) => vec![wake(1, 1, Request), wake(1, 2, Request)],
            (Behavior::Flag { flag }, Input::Wake(1)) => {
                *flag = true;
                Vec::new()
            }
            (Behavior::Flag { flag: false }, Input::Wake(2)) => vec![Action::Wake {
                when: When::Now,
                token: 3,
                phase: Request,
            }],
            (Behavior::Flag { .. }, Input::Wake(3)) => vec![send(0, 3, Request)],
            (Behavior::Timer { .. }, Input::Init) => vec![wake(2, 0, Complete)],
            (Behavior::Timer { count }, Input::Wake(_)) => {
                *count += 1;
                let phase = if *count == 1 { Complete } else { Request };
                let mut out = vec![send(0, *count, phase)];
                if *count < 3 {
                    out.push(wake(2, *count, Request));
                }
                out
            }
            (Behavior::Worker { .. }, Input::Init) => vec![wake(1, 0, Request)],
            (Behavior::Worker { steps }, Input::Wake(_)) => {
                *steps += 1;
                if *steps < 4 {
                    vec![wake(1, *steps, Request)]
                } else {
                    Vec::new()
                }
            }
            (Behavior::Worker { steps }, Input::Msg { .. }) => vec![send(1, *steps, Complete)],
            _ => Vec::new(),
        }
    }

    fn state(&self) -> u64 {
        match *self {
            Behavior::Join { count } | Behavior::Timer { count } => count,
            Behavior::Flag { flag } => u64::from(flag),
            Behavior::Worker { steps } => steps,
            Behavior::Side | Behavior::Sink => 0,
        }
    }

    fn set_state(&mut self, value: u64) {
        match self {
            Behavior::Join { count } | Behavior::Timer { count } => *count = value,
            Behavior::Flag { flag } => *flag = value != 0,
            Behavior::Worker { steps } => *steps = value,
            Behavior::Side | Behavior::Sink => {}
        }
    }
}

/// A synthetic topology: its components, its links `(a, port, b, port, cycles)` with no
/// latency for `None`, the delay scope, and the asynchronous set.
struct Topology {
    components: Vec<Behavior>,
    links: Vec<(u32, u16, u32, u16, Option<u64>)>,
    scope: Vec<u32>,
    asynchronous: Vec<u32>,
}

/// §8.3's generated-event diamond: two sides whose zero-latency sends reach the join in
/// the current phase, and the join's forwards on a scoped link.
fn diamond() -> Topology {
    Topology {
        components: vec![
            Behavior::Side,
            Behavior::Side,
            Behavior::Join { count: 0 },
            Behavior::Sink,
        ],
        links: vec![
            (0, 0, 2, 0, None),
            (1, 0, 2, 1, None),
            (2, 2, 3, 0, Some(1)),
        ],
        scope: vec![2],
        asynchronous: Vec::new(),
    }
}

/// §7.2's conditional wake: a reordering adds an event to its group, or removes it.
fn conditional() -> Topology {
    Topology {
        components: vec![Behavior::Flag { flag: false }, Behavior::Sink],
        links: vec![(0, 0, 1, 0, Some(1))],
        scope: vec![0],
        asynchronous: Vec::new(),
    }
}

/// §7.6's periodic asynchronous wake, preempting a worker between two of its steps.
fn preemption() -> Topology {
    Topology {
        components: vec![
            Behavior::Timer { count: 0 },
            Behavior::Worker { steps: 0 },
            Behavior::Sink,
        ],
        links: vec![(0, 0, 1, 0, None), (1, 1, 2, 0, Some(1))],
        scope: vec![1],
        asynchronous: vec![0],
    }
}

// ---------------------------------------------------------------------------------------
// The scripted session's side.

struct Actor(Behavior);

fn message(payload: u64) -> Message {
    MemMsg::ReadReq {
        txn: TxnId(payload),
        addr: 0,
        len: 4,
    }
    .into()
}

fn when(w: When) -> ScheduleWhen {
    match w {
        When::Now => ScheduleWhen::Now,
        When::Cycles(k) => ScheduleWhen::Cycles {
            domain: ClockDomainId(0),
            k,
        },
    }
}

fn act(actions: Vec<Action>, ctx: &mut dyn InitContext) -> Result<(), SimError> {
    for a in actions {
        match a {
            Action::Send {
                port,
                payload,
                when: w,
                phase,
            } => ctx.send(PortId(port), message(payload), when(w), phase)?,
            Action::Wake {
                when: w,
                token,
                phase,
            } => ctx.wake_self(when(w), phase, token)?,
        }
    }
    Ok(())
}

impl Component for Actor {
    fn type_name(&self) -> &'static str {
        "test.actor"
    }
    fn ports(&self) -> Vec<PortSpec> {
        let roles = self.0.roles();
        roles
            .into_iter()
            .zip(PORTS)
            .map(|(role, name)| PortSpec {
                name,
                protocol: mem::PROTOCOL,
                role,
            })
            .collect()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        act(self.0.react(Input::Init), ctx)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let input = match ev {
            Delivered::Wake { token } => Input::Wake(*token),
            Delivered::Message { port, msg } => {
                assert!(matches!(msg, Message::Mem(MemMsg::ReadReq { .. })));
                Input::Msg { port: port.0 }
            }
        };
        act(self.0.react(input), ctx)
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        w.u64(self.0.state());
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        self.0.set_state(r.u64()?);
        Ok(())
    }
}

fn platform(topology: &Topology, script: &Script) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    )
    .unwrap();
    let ids: Vec<_> = topology
        .components
        .iter()
        .enumerate()
        .map(|(i, b)| t.add_component(format!("soc.c{i}"), Box::new(Actor(b.clone()))))
        .collect();
    for &(a, pa, b, pb, cycles) in &topology.links {
        let latency = cycles.map(|k| LinkLatency::Cycles {
            domain: ClockDomainId(0),
            k,
        });
        t.connect(
            (ids[a as usize], PORTS[usize::from(pa)]),
            (ids[b as usize], PORTS[usize::from(pb)]),
            latency,
        );
    }
    t.elaborate(SessionConfig {
        script: Some(script.clone()),
        ..SessionConfig::default()
    })
    .unwrap()
}

/// A dispatch order: every dispatched event's tick, phase, and identity.
type Order = Vec<(u64, Phase, EventId)>;

fn order_of(steps: &[Dispatched]) -> Order {
    steps
        .iter()
        .map(|d| (d.key.tick.0, d.key.phase, d.id.unwrap()))
        .collect()
}

fn record(topology: &Topology, script: &Script) -> Recording {
    let mut rt = platform(topology, script);
    rt.init().unwrap();
    let mut rec = Recording::after_init(&rt);
    while rec.step(&mut rt).unwrap().is_some() {}
    rec
}

/// The tick `id` is dispatched at in `rec`.
fn tick_of(rec: &Recording, id: EventId) -> u64 {
    let step = rec.steps.iter().find(|s| s.event.id == Some(id));
    step.unwrap().event.key.tick.0
}

/// A script the search reached, with its branch point of latest order.
struct Reached {
    script: Script,
    last: Option<BranchPoint>,
    order: Order,
}

/// Every script reachable from `script`, with at most `k` deviations more, by one
/// deviation at a time at a choice point after `last` (§9.2): duplicates are counted in
/// `duplicates` and not extended.
fn search(
    topology: &Topology,
    bounds: (u64, i64),
    script: Script,
    last: Option<BranchPoint>,
    k: usize,
    out: &mut Vec<Reached>,
    duplicates: &mut usize,
) {
    let (d_max, e_max) = bounds;
    let rec = record(topology, &script);
    let mut children: Vec<(Script, BranchPoint)> = Vec::new();
    if k > 0 {
        let scope = LinkSet::of(&topology.scope);
        for point in choice_points(&rec, scope, &topology.asynchronous, last).unwrap() {
            match point.choice {
                Choice::Pick { default, ready } => {
                    for event in ready {
                        let mut child = script.clone();
                        match child.picks.iter_mut().find(|(d, _)| *d == default) {
                            Some((_, events)) => events.push(event),
                            None => {
                                child.picks.push((default, vec![event]));
                                child.picks.sort();
                            }
                        }
                        children.push((child, point.branch));
                    }
                }
                Choice::Delay(id) => {
                    let (link, dir, ordinal) = id;
                    let msg = EventId::Msg { link, dir, ordinal };
                    let mut seen = vec![tick_of(&rec, msg)];
                    for d in 1..=d_max {
                        let mut child = script.clone();
                        child.delays.push((id, d));
                        child.delays.sort();
                        let tick = tick_of(&record(topology, &child), msg);
                        if seen.contains(&tick) {
                            *duplicates += 1;
                        } else {
                            seen.push(tick);
                            children.push((child, point.branch));
                        }
                    }
                }
                Choice::Shift(id) => {
                    let (component, ordinal) = id;
                    let wake = EventId::Wake { component, ordinal };
                    for sign in [-1, 1] {
                        let mut seen = vec![tick_of(&rec, wake)];
                        for m in 1..=e_max {
                            let mut child = script.clone();
                            child.shifts.push((id, sign * m));
                            child.shifts.sort();
                            let tick = tick_of(&record(topology, &child), wake);
                            if seen.contains(&tick) {
                                *duplicates += 1;
                            } else {
                                seen.push(tick);
                                children.push((child, point.branch));
                            }
                        }
                    }
                }
            }
        }
    }
    out.push(Reached {
        order: order_of(
            &rec.steps
                .iter()
                .map(|s| s.event.clone())
                .collect::<Vec<_>>(),
        ),
        script,
        last,
    });
    for (child, branch) in children {
        search(
            topology,
            bounds,
            child,
            Some(branch),
            k - 1,
            out,
            duplicates,
        );
    }
}

// ---------------------------------------------------------------------------------------
// The naive model.

#[derive(Clone, Copy, Debug)]
struct Pending {
    tick: u64,
    phase: Phase,
    counter: u64,
    id: EventId,
    target: u32,
    input: Input,
}

/// The next clock edge at or after `tick`, plus `k` cycles.
fn cycles_after(tick: u64, k: u64) -> u64 {
    tick.div_ceil(CYCLE) * CYCLE + k * CYCLE
}

/// One run of the model under `choices`, which picks the option of each decision in
/// turn, taking option 0, the scheduler's own, past its end.
struct Model<'a> {
    topology: &'a Topology,
    bounds: (u64, i64),
    behaviors: Vec<Behavior>,
    pending: Vec<Pending>,
    now: (u64, Phase),
    counter: u64,
    /// Per directed link: messages sent and the latest delivery tick.
    lanes: BTreeMap<(u32, u8), (u64, u64)>,
    wakes: Vec<u64>,
    choices: &'a [usize],
    /// Each decision's option count and the option taken.
    decisions: Vec<(usize, usize)>,
}

impl Model<'_> {
    fn decide(&mut self, options: usize) -> usize {
        let taken = self.choices.get(self.decisions.len()).copied().unwrap_or(0);
        assert!(taken < options);
        self.decisions.push((options, taken));
        taken
    }

    fn schedule(&mut self, (tick, phase): (u64, Phase), id: EventId, target: u32, input: Input) {
        // S1 and S2.
        assert!(
            (tick, phase) >= self.now,
            "{id:?} at {tick} before {:?}",
            self.now
        );
        self.pending.push(Pending {
            tick,
            phase,
            counter: self.counter,
            id,
            target,
            input,
        });
        self.counter += 1;
    }

    fn resolve(&self, when: When) -> u64 {
        match when {
            When::Now => self.now.0,
            When::Cycles(k) => cycles_after(self.now.0, k),
        }
    }

    fn react(&mut self, me: u32, input: Input) {
        let actions = self.behaviors[me as usize].react(input);
        for a in actions {
            match a {
                Action::Send {
                    when, port, phase, ..
                } => self.send(me, port, when, phase),
                Action::Wake { when, token, phase } => self.wake(me, when, token, phase),
            }
        }
    }

    fn send(&mut self, me: u32, port: u16, when: When, phase: Phase) {
        let (index, &(a, pa, b, pb, cycles)) = self
            .topology
            .links
            .iter()
            .enumerate()
            .find(|(_, l)| (l.0 == me && l.1 == port) || (l.2 == me && l.3 == port))
            .unwrap();
        let (dir, target, at) = if a == me && pa == port {
            (0u8, b, pb)
        } else {
            (1, a, pa)
        };
        let link = u32::try_from(index).unwrap();
        let sent = self.resolve(when);
        let mut tick = cycles.map_or(sent, |k| cycles_after(sent, k));
        if let Some(k) = cycles
            && self.topology.scope.contains(&link)
        {
            let d = self.decide(usize::try_from(self.bounds.0).unwrap() + 1) as u64;
            tick = cycles_after(sent, k + d);
        }
        let lane = self.lanes.entry((link, dir)).or_insert((0, 0));
        let tick = tick.max(lane.1);
        let ordinal = lane.0;
        *lane = (ordinal + 1, tick);
        let id = EventId::Msg { link, dir, ordinal };
        self.schedule((tick, phase), id, target, Input::Msg { port: at });
    }

    fn wake(&mut self, me: u32, when: When, token: u64, phase: Phase) {
        let mut tick = self.resolve(when);
        if self.topology.asynchronous.contains(&me) {
            let e = self.bounds.1;
            let options = usize::try_from(2 * e).unwrap() + 1;
            // Option 0 is no shift; then -E..=-1 and 1..=E.
            let taken = i64::try_from(self.decide(options)).unwrap();
            let s = match taken {
                0 => 0,
                t if t <= e => -t,
                t => t - e,
            };
            if s != 0 {
                let When::Cycles(k) = when else {
                    panic!("a shifted wake is not in cycles");
                };
                let k = k.saturating_add_signed(s);
                tick = cycles_after(self.now.0, k);
                if tick == self.now.0 && phase < self.now.1 {
                    tick += 1;
                }
            }
        }
        let ordinal = self.wakes[me as usize];
        self.wakes[me as usize] += 1;
        let id = EventId::Wake {
            component: me,
            ordinal,
        };
        self.schedule((tick, phase), id, me, Input::Wake(token));
    }

    /// The events that may be dispatched next: those of the least `(tick, phase)` with
    /// no earlier message of their lane pending, the scheduler's own first.
    fn ready(&self) -> Vec<usize> {
        let Some(least) = self.pending.iter().map(|p| (p.tick, p.phase)).min() else {
            return Vec::new();
        };
        let lane = |id: EventId| match id {
            EventId::Msg { link, dir, ordinal } => Some((link, dir, ordinal)),
            EventId::Wake { .. } => None,
        };
        let mut ready: Vec<usize> = (0..self.pending.len())
            .filter(|&i| {
                let p = self.pending[i];
                (p.tick, p.phase) == least
                    && lane(p.id).is_none_or(|(link, dir, ordinal)| {
                        !self.pending.iter().any(|q| {
                            lane(q.id).is_some_and(|(l, d, o)| (l, d) == (link, dir) && o < ordinal)
                        })
                    })
            })
            .collect();
        ready.sort_by_key(|&i| self.pending[i].counter);
        ready
    }

    fn run(
        topology: &Topology,
        bounds: (u64, i64),
        choices: &[usize],
    ) -> (Order, Vec<(usize, usize)>) {
        let mut m = Model {
            topology,
            bounds,
            behaviors: topology.components.clone(),
            pending: Vec::new(),
            now: (0, Phase::Request),
            counter: 0,
            lanes: BTreeMap::new(),
            wakes: vec![0; topology.components.len()],
            choices,
            decisions: Vec::new(),
        };
        for me in 0..topology.components.len() {
            m.react(u32::try_from(me).unwrap(), Input::Init);
        }
        let mut order = Vec::new();
        loop {
            let ready = m.ready();
            if ready.is_empty() {
                break;
            }
            let taken = if ready.len() > 1 {
                m.decide(ready.len())
            } else {
                0
            };
            let p = m.pending.remove(ready[taken]);
            m.now = (p.tick, p.phase);
            order.push((p.tick, p.phase, p.id));
            m.react(p.target, p.input);
        }
        (order, m.decisions)
    }
}

/// Every distinct dispatch order of the model with at most `k` deviations: each
/// decision's option other than 0 is one.
fn enumerate(topology: &Topology, bounds: (u64, i64), k: usize) -> BTreeSet<Order> {
    let mut orders = BTreeSet::new();
    let mut stack: Vec<Vec<usize>> = vec![Vec::new()];
    while let Some(choices) = stack.pop() {
        let (order, decisions) = Model::run(topology, bounds, &choices);
        orders.insert(order);
        let used = choices.iter().filter(|&&c| c != 0).count();
        if used == k {
            continue;
        }
        for i in choices.len()..decisions.len() {
            for alt in 1..decisions[i].0 {
                let mut child: Vec<usize> = decisions[..i].iter().map(|d| d.1).collect();
                child.push(alt);
                stack.push(child);
            }
        }
    }
    orders
}

// ---------------------------------------------------------------------------------------
// The checks.

fn check(topology: &Topology, bounds: (u64, i64), k: usize) -> (usize, usize) {
    let mut reached = Vec::new();
    let mut duplicates = 0;
    search(
        topology,
        bounds,
        Script::default(),
        None,
        k,
        &mut reached,
        &mut duplicates,
    );
    let expected = enumerate(topology, bounds, k);
    let mut by_order: BTreeMap<&Order, Vec<&Script>> = BTreeMap::new();
    for r in &reached {
        by_order.entry(&r.order).or_default().push(&r.script);
    }
    for (order, scripts) in &by_order {
        assert_eq!(scripts.len(), 1, "{order:?} by {scripts:?}");
    }
    let orders: BTreeSet<Order> = by_order.keys().map(|&o| o.clone()).collect();
    let missing: Vec<_> = expected.difference(&orders).collect();
    let extra: Vec<_> = orders.difference(&expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing {missing:?}, extra {extra:?}"
    );
    // A fork of each script's parent just before its last branch point runs on as the
    // script does from init.
    for r in &reached {
        let Some(last) = r.last else { continue };
        let Some(before) = last.at.checked_sub(1) else {
            continue;
        };
        let parent = reached
            .iter()
            .find(|p| p.script != r.script && is_parent(&p.script, &r.script))
            .unwrap();
        let mut rt = platform(topology, &parent.script);
        rt.init().unwrap();
        for _ in 0..before {
            rt.step().unwrap().unwrap();
        }
        let snapshot = rt.snapshot().unwrap();
        let mut forked = platform(topology, &r.script);
        forked.fork(&snapshot).unwrap();
        let rest: Vec<Dispatched> = std::iter::from_fn(|| forked.step().unwrap()).collect();
        assert_eq!(
            order_of(&rest),
            r.order[usize::try_from(before).unwrap()..],
            "{:?}",
            r.script
        );
    }
    (reached.len(), duplicates)
}

/// Whether `child` is `parent` with one deviation more.
fn is_parent(parent: &Script, child: &Script) -> bool {
    let entries = |s: &Script| {
        s.delays.len() + s.shifts.len() + s.picks.iter().map(|p| p.1.len()).sum::<usize>()
    };
    if entries(parent) + 1 != entries(child) {
        return false;
    }
    parent.delays.iter().all(|d| child.delays.contains(d))
        && parent.shifts.iter().all(|s| child.shifts.contains(s))
        && parent.picks.iter().all(|(d, events)| {
            child
                .picks
                .iter()
                .any(|(c, more)| c == d && more.starts_with(events))
        })
}

#[test]
fn the_diamond_reaches_every_order_once() {
    // Scripts reached and duplicates, a committed expectation for mutants (§17).
    assert_eq!(check(&diamond(), (2, 0), 3), (34, 12));
}

#[test]
fn the_conditional_wake_reaches_every_order_once() {
    // B, A, W and B, W, A, each with W's message delayed 0, 1, or 2 cycles; and A, B.
    assert_eq!(check(&conditional(), (2, 0), 3), (7, 0));
}

#[test]
fn a_periodic_wake_preempting_a_worker_reaches_every_order_once() {
    // Shifts of 3 cycles earlier go no earlier than 2 do: duplicates.
    assert_eq!(check(&preemption(), (1, 3), 2), (169, 20));
}
