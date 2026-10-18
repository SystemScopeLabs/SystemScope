//! Locator self-tests on scripted executions (`docs/m4-design.md` §11.2, §25.3): for random
//! pairs of streams, including divergences that reconverge, streams of different lengths,
//! failures, and incomparable observations, every strategy at every checkpoint density
//! gives the naive comparison's primary result and the same report window.

mod common;

use proptest::prelude::*;
use systemscope_contracts::trace::Value;
use systemscope_verify::backend::{Backend, Capabilities, Capability, Side, run};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario, execution_id};
use systemscope_verify::checkpoint::{Checkpoint, StreamMark};
use systemscope_verify::compare::{Verdict, compare_recordings};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{
    Located, Window, lockstep, record_index, recorded, search, stream_compare,
};
use systemscope_verify::observation::{Observation, ObservedValue, PrefixDigest};
use systemscope_verify::profile::{ComparisonProfile, Plan, Role, Support, plan};
use systemscope_verify::runtime::Projection;

const STREAMS: [&str; 2] = ["arch", "effect"];

/// One boundary a scripted event emits: its stream, and a value that names its kind and
/// fields. Value 0 has an absent `rd_value`, which a strict profile refuses.
type Emit = (usize, u8);

fn observation(stream: usize, ordinal: u64, v: u8) -> Observation {
    let u = |v: u8| ObservedValue::Present(Value::U64(u64::from(v)));
    let (kind, fields) = match (stream, v % 4) {
        (0, 3) => ("exception", vec![("cause", u(v))]),
        (0, _) => {
            let rd = if v == 0 { ObservedValue::Absent } else { u(v) };
            (
                "retire",
                vec![("pc", u(v)), ("rd_value", rd), ("paddr", u(v / 2))],
            )
        }
        _ => ("uart_tx", vec![("byte", u(v))]),
    };
    Observation {
        stream: STREAMS[stream].to_owned(),
        ordinal,
        kind: kind.to_owned(),
        fields: fields.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
    }
}

/// A scripted execution: each event emits its boundaries; after the last event the stop
/// rule holds, unless the side fails when it reaches event `fail_at`.
#[derive(Clone, Debug)]
struct Script {
    events: Vec<Vec<Emit>>,
    fail_at: Option<usize>,
}

/// A backend replaying a [`Script`] with checkpoints at event boundaries.
struct Scripted {
    script: Script,
    name: &'static str,
    execution: [u8; 32],
    at: usize,
    queues: [Vec<Observation>; 2],
    prefixes: [PrefixDigest; 2],
}

impl Scripted {
    fn new(script: Script, name: &'static str) -> Scripted {
        Scripted {
            script,
            name,
            execution: [0; 32],
            at: 0,
            queues: [Vec::new(), Vec::new()],
            prefixes: [PrefixDigest::new(); 2],
        }
    }

    fn stream(name: &str) -> Result<usize, EngineError> {
        STREAMS
            .iter()
            .position(|s| *s == name)
            .ok_or_else(|| EngineError::Internal(format!("no stream {name}")))
    }
}

impl Backend for Scripted {
    fn describe(&self) -> BackendId {
        BackendId {
            name: "script".to_owned(),
            version: "1".to_owned(),
            build: self.name.to_owned(),
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::from([
            Capability::ArchStream,
            Capability::EffectStream,
            Capability::StepBoundary,
            Capability::Snapshot,
            Capability::Restore,
        ])
    }
    fn support(&self) -> Support {
        common::CounterProjection::default().support()
    }
    fn start(
        &mut self,
        scenario: &Scenario,
        input: &InputVariant,
        _: &BackendRunConfig,
        _: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        self.execution = execution_id(scenario, input, &self.describe());
        self.at = 0;
        self.queues = [Vec::new(), Vec::new()];
        self.prefixes = [PrefixDigest::new(); 2];
        Ok(())
    }
    fn next(&mut self, stream: &str) -> Result<Option<Observation>, EngineError> {
        let s = Scripted::stream(stream)?;
        loop {
            if !self.queues[s].is_empty() {
                return Ok(Some(self.queues[s].remove(0)));
            }
            if self.script.fail_at == Some(self.at) {
                return Err(EngineError::BackendFailed(format!(
                    "{} failed at event {}",
                    self.name, self.at
                )));
            }
            let Some(event) = self.script.events.get(self.at) else {
                return Ok(None);
            };
            for &(t, v) in event {
                let o = observation(t, self.prefixes[t].count(), v);
                self.prefixes[t].absorb(&o);
                self.queues[t].push(o);
            }
            self.at += 1;
        }
    }
    fn emitted(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        Ok(std::mem::take(&mut self.queues[Scripted::stream(stream)?]))
    }
    fn checkpoint(&self) -> Result<Checkpoint, EngineError> {
        Ok(Checkpoint {
            execution: self.execution,
            event_index: self.at as u64,
            streams: (0..2)
                .map(|t| StreamMark {
                    stream: STREAMS[t].to_owned(),
                    covered: self.prefixes[t].count(),
                    prefix: self.prefixes[t].digest(),
                })
                .collect(),
            state: (self.at as u64).to_le_bytes().to_vec(),
        })
    }
    fn restore(&mut self, c: &Checkpoint) -> Result<(), EngineError> {
        assert_eq!(c.execution, self.execution, "another side's checkpoint");
        self.at = u64::from_le_bytes(c.state[..].try_into().unwrap()) as usize;
        self.queues = [Vec::new(), Vec::new()];
        for (t, m) in c.streams.iter().enumerate() {
            self.prefixes[t] = PrefixDigest::resume(m.prefix, m.covered);
        }
        Ok(())
    }
}

fn scenario() -> Scenario {
    Scenario {
        platform: "script".to_owned(),
        topology_hash: [0; 32],
        cpu_profile: "none".to_owned(),
        program: [0; 32],
        locator: String::new(),
        seed: 0,
    }
}

fn side(b: &mut Scripted) -> Side<'_> {
    Side {
        backend: b,
        input: InputVariant::None,
        config: BackendRunConfig { budget: 0 },
    }
}

/// Every strategy at every density, each on fresh backends.
fn every_strategy(plan: &Plan, e: &Script, a: &Script) -> Vec<Result<Located, EngineError>> {
    let s = scenario();
    let fresh = || (Scripted::new(e.clone(), "e"), Scripted::new(a.clone(), "a"));
    let mut out = Vec::new();
    let (mut x, mut y) = fresh();
    out.push(lockstep(plan, &s, side(&mut x), side(&mut y)));
    for role in [Role::Expected, Role::Actual] {
        let (mut x, mut y) = fresh();
        out.push(stream_compare(plan, &s, role, side(&mut x), side(&mut y)));
    }
    let (mut x, mut y) = fresh();
    out.push(recorded(plan, &s, side(&mut x), side(&mut y)));
    for every in 0..5 {
        let (mut x, mut y) = fresh();
        let ie = record_index(plan, &s, side(&mut x), Role::Expected, every).unwrap();
        let ia = record_index(plan, &s, side(&mut y), Role::Actual, every).unwrap();
        let (mut x, mut y) = fresh();
        out.push(search(plan, &s, (side(&mut x), &ie), (side(&mut y), &ia)));
    }
    out
}

fn primary(v: &Verdict) -> Verdict {
    match v {
        Verdict::Compared(c) => {
            let mut c = c.clone();
            c.auxiliary.clear();
            c.failures.clear();
            Verdict::Compared(c)
        }
        other => other.clone(),
    }
}

fn emit() -> impl Strategy<Value = Emit> {
    (0..2usize, 0..12u8)
}

fn events() -> impl Strategy<Value = Vec<Vec<Emit>>> {
    prop::collection::vec(prop::collection::vec(emit(), 0..3), 0..24)
}

/// An expected script, and an actual one made from it by changing a few boundaries
/// (which may reconverge), cutting or extending it, and failing either side.
fn pair() -> impl Strategy<Value = (Script, Script)> {
    (
        events(),
        prop::collection::vec((any::<prop::sample::Index>(), emit()), 0..4),
        prop::option::of(0..4usize),
        prop::collection::vec(prop::collection::vec(emit(), 0..3), 0..3),
        prop::option::of(any::<prop::sample::Index>()),
        prop::option::of(any::<prop::sample::Index>()),
    )
        .prop_map(|(events, changes, cut, extra, fail_e, fail_a)| {
            let mut actual = events.clone();
            for (at, emit) in changes {
                if !actual.is_empty() {
                    let i = at.index(actual.len());
                    match actual[i].first_mut() {
                        Some(first) => *first = emit,
                        None => actual[i].push(emit),
                    }
                }
            }
            if let Some(cut) = cut {
                actual.truncate(actual.len().saturating_sub(cut));
            }
            actual.extend(extra);
            let fail = |f: Option<prop::sample::Index>, n: usize| f.map(|f| f.index(n + 1));
            let e = Script {
                fail_at: fail(fail_e, events.len()),
                events,
            };
            let a = Script {
                fail_at: fail(fail_a, actual.len()),
                events: actual,
            };
            (e, a)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn every_strategy_gives_the_naive_primary_result(
        (e, a) in pair(),
        window in 0..4u32,
        strict in any::<bool>(),
    ) {
        let mut profile = common::profile();
        profile.report_window = window;
        profile.primary.kinds[1].fields[1].may_be_absent = !strict;
        let support = Scripted::new(e.clone(), "e").support();
        let plan = plan(&profile, &support, &support).unwrap();
        let s = scenario();
        let (mut x, mut y) = (Scripted::new(e.clone(), "e"), Scripted::new(a.clone(), "a"));
        let naive = compare_recordings(
            &plan,
            &run(&plan, &s, side(&mut x)),
            &run(&plan, &s, side(&mut y)),
        );
        let results = every_strategy(&plan, &e, &a);
        let first: Option<Window> = results[0].as_ref().ok().and_then(|l| l.window.clone());
        for (i, got) in results.iter().enumerate() {
            match (&naive, got) {
                (Ok(want), Ok(l)) => {
                    prop_assert_eq!(primary(&l.verdict), primary(want), "strategy {}", i);
                    prop_assert_eq!(&l.window, &first, "strategy {}", i);
                }
                (Err(want), Err(e)) => prop_assert_eq!(e, want, "strategy {}", i),
                (want, got) => prop_assert!(false, "strategy {}: {:?}, not {:?}", i, got, want),
            }
        }
        // The window is the streams' own observations around the divergence.
        if let (Some(w), Ok(Verdict::Compared(c))) = (&first, &naive) {
            let d = match &c.primary {
                systemscope_verify::compare::StreamResult::Diverged { ordinal, .. } => *ordinal,
                other => panic!("a window with {other:?}"),
            };
            prop_assert_eq!(w.first, d.saturating_sub(u64::from(window)));
            let (mut x, mut y) = (Scripted::new(e, "e"), Scripted::new(a, "a"));
            for (kept, side) in [(&w.expected, run(&plan, &s, side(&mut x))), (&w.actual, run(&plan, &s, side(&mut y)))] {
                let whole = &side.streams[0];
                let end = (d + u64::from(window) + 1).min(whole.len() as u64) as usize;
                prop_assert_eq!(&kept[..], &whole[w.first as usize..end]);
            }
        }
    }
}
