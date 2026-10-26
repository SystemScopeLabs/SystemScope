//! The counter fixture shared by the runtime adapter and locator tests: a small component
//! with a projection, a profile, and a state patcher. Test-only.

#![allow(dead_code)]

use systemscope_contracts::component::{Component, Delivered, InitContext, PortSpec, SimContext};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::script::Script;
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_verify::backend::{Backend, Capabilities, Capability, Side};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario, StatePatch};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::Observation;
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Requirement, StreamSpec, Support,
};
use systemscope_verify::report::{Builder, Make};
use systemscope_verify::runtime::{Projected, Projection, RuntimeBackend};

/// Marks the counter's state in snapshot bytes, so a test patch can find it.
pub const MARK: u64 = 0x5EED_C0DE_5EED_C0DE;
pub const LIMIT: u64 = 12;

/// Counts wakes up to [`LIMIT`]: each wake traces `t.step`, every fourth `t.out`, every
/// fifth `t.noise`, and the last `t.halt`, with `base` added to every count it traces.
/// `bad_at` adds a record the projection refuses. With `cycles`, it wakes after a cycle
/// of clock domain 0 instead of a nanosecond.
pub struct Counter {
    n: u64,
    base: u64,
    cycles: bool,
    halts: bool,
    bad_at: Option<u64>,
}

impl Counter {
    fn next(&self) -> ScheduleWhen {
        if self.cycles {
            ScheduleWhen::Cycles {
                domain: ClockDomainId(0),
                k: 1,
            }
        } else {
            ScheduleWhen::After(Duration::from_ns(1))
        }
    }
}

impl Component for Counter {
    fn type_name(&self) -> &'static str {
        "test.counter"
    }
    fn ports(&self) -> Vec<PortSpec> {
        Vec::new()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        ctx.trace("t.init", Vec::new());
        ctx.wake_self(self.next(), Phase::Commit, 0)
    }
    fn handle_event(&mut self, _: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.n += 1;
        let n = self.n;
        let traced = Value::U64(n + self.base);
        ctx.trace("t.step", vec![("n", traced.clone())]);
        if n.is_multiple_of(4) {
            ctx.trace("t.out", vec![("byte", traced.clone())]);
        }
        if n.is_multiple_of(5) {
            ctx.trace("t.noise", vec![("n", traced)]);
        }
        if self.bad_at == Some(n) {
            ctx.trace("t.bad", Vec::new());
        }
        if n == LIMIT && self.halts {
            ctx.trace("t.halt", Vec::new());
            return Ok(());
        }
        if n < LIMIT {
            ctx.wake_self(self.next(), Phase::Commit, 0)?;
        }
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        1
    }
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("n", Value::U64(self.n)),
                ("halts", Value::Bool(self.halts)),
            ],
        }
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        w.u64(MARK);
        w.u64(self.n);
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        if r.u64()? != MARK {
            return Err(RestoreError::InvalidState("counter: no mark"));
        }
        self.n = r.u64()?;
        if self.n > LIMIT {
            return Err(RestoreError::InvalidState("counter: past the limit"));
        }
        Ok(())
    }
}

pub fn runtime(halts: bool, bad_at: Option<u64>) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "counter",
        Box::new(Counter {
            n: 0,
            base: 0,
            cycles: false,
            halts,
            bad_at,
        }),
    );
    t.elaborate(SessionConfig::default()).unwrap()
}

/// The count [`pair`]'s second counter adds to every count it traces.
pub const PAIR_BASE: u64 = 100;

/// Two halting counters under `script`, the second tracing counts from [`PAIR_BASE`],
/// each waking every cycle of a 1 GHz clock: their wakes tie at every tick, so each
/// tick's first dispatch offers a pick that swaps their records.
pub fn pair(script: Script) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    )
    .unwrap();
    for (name, base) in [("a", 0), ("b", PAIR_BASE)] {
        let counter = Counter {
            n: 0,
            base,
            cycles: true,
            halts: true,
            bad_at: None,
        };
        t.add_component(name, Box::new(counter));
    }
    let config = SessionConfig {
        script: Some(script),
        ..SessionConfig::default()
    };
    t.elaborate(config).unwrap()
}

/// An adapter over [`pair`] under `script`, whose build names the script.
pub fn pair_backend(script: Script) -> RuntimeBackend {
    let id = BackendId {
        build: format!("pair {script:?}"),
        ..id()
    };
    RuntimeBackend::new(
        id,
        Box::new(move |_, input| match input {
            InputVariant::None => Ok(pair(script.clone())),
            _ => Err("the pair takes no input variant".to_owned()),
        }),
        Box::new(|| Box::new(CounterProjection::default())),
    )
}

/// The scenario of [`pair`].
pub fn pair_scenario() -> Scenario {
    Scenario {
        platform: "pair".to_owned(),
        topology_hash: pair(Script::default()).topology_hash(),
        ..scenario()
    }
}

/// `t.step` is a `retire` (pc `4n`, `rd_value` absent when 3 divides `n`, `paddr` `n`),
/// `t.noise` an `interrupt`, `t.out` a `uart_tx`, and `t.halt` the stop rule.
#[derive(Default)]
pub struct CounterProjection {
    finished: bool,
    /// Added to every `uart_tx` byte.
    pub shift: u64,
}

impl Projection for CounterProjection {
    fn support(&self) -> Support {
        let mut s = Support::default();
        s.streams.extend(["arch", "effect"].map(str::to_owned));
        s.fields.extend(
            [
                "arch/retire/pc",
                "arch/retire/rd_value",
                "arch/retire/paddr",
                "arch/exception/cause",
                "arch/interrupt/cause",
                "effect/uart_tx/byte",
            ]
            .map(str::to_owned),
        );
        s
    }
    fn begin(&mut self, profile: &ComparisonProfile) -> Result<(), String> {
        match profile.stop_rule.as_str() {
            "halt" => Ok(()),
            other => Err(format!("unknown stop rule {other}")),
        }
    }
    fn project(&mut self, record: &TraceRecord, out: &mut Vec<Projected>) -> Result<(), String> {
        let n = match record.fields.first() {
            Some((_, Value::U64(n))) => *n,
            _ => 0,
        };
        let u = |v| ObservedValue::Present(Value::U64(v));
        let p = |stream: &str, kind: &str, fields: Vec<(&str, ObservedValue)>| Projected {
            stream: stream.to_owned(),
            kind: kind.to_owned(),
            fields: fields.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
        };
        match record.kind {
            // The runtime's own records, such as `runtime.dispatch`.
            k if !k.starts_with("t.") => {}
            "t.init" => {}
            "t.step" => {
                let rd = if n.is_multiple_of(3) {
                    ObservedValue::Absent
                } else {
                    u(n)
                };
                // Fields in another order than the profile's: the adapter orders them.
                out.push(p(
                    "arch",
                    "retire",
                    vec![("paddr", u(n)), ("rd_value", rd), ("pc", u(4 * n))],
                ));
            }
            "t.noise" => out.push(p("arch", "interrupt", vec![("cause", u(n))])),
            "t.out" => out.push(p("effect", "uart_tx", vec![("byte", u(n + self.shift))])),
            "t.halt" => self.finished = true,
            other => return Err(format!("unreadable record {other}")),
        }
        Ok(())
    }
    fn finished(&self) -> bool {
        self.finished
    }
}

pub fn field(name: &str, requirement: Requirement, may_be_absent: bool) -> FieldSpec {
    FieldSpec {
        name: name.to_owned(),
        requirement,
        may_be_absent,
    }
}

/// `arch` compares `retire` and `exception`, never `interrupt`; `effect` is auxiliary.
pub fn profile() -> ComparisonProfile {
    ComparisonProfile {
        name: "counter".to_owned(),
        version: 1,
        primary: StreamSpec {
            stream: "arch".to_owned(),
            kinds: vec![
                KindSpec {
                    kind: "exception".to_owned(),
                    fields: vec![field("cause", Requirement::Required, false)],
                    excluded: Vec::new(),
                },
                KindSpec {
                    kind: "retire".to_owned(),
                    fields: vec![
                        field("pc", Requirement::Required, false),
                        field("rd_value", Requirement::Required, true),
                        field("paddr", Requirement::Optional, false),
                    ],
                    excluded: Vec::new(),
                },
            ],
            window: String::new(),
        },
        auxiliary: vec![StreamSpec {
            stream: "effect".to_owned(),
            kinds: vec![KindSpec {
                kind: "uart_tx".to_owned(),
                fields: vec![field("byte", Requirement::Required, false)],
                excluded: Vec::new(),
            }],
            window: String::new(),
        }],
        stop_rule: "halt".to_owned(),
        report_window: 3,
    }
}

pub fn scenario() -> Scenario {
    Scenario {
        platform: "counter".to_owned(),
        topology_hash: runtime(true, None).topology_hash(),
        cpu_profile: "none".to_owned(),
        program: [0; 32],
        locator: String::new(),
        seed: 0,
    }
}

pub fn id() -> BackendId {
    BackendId {
        name: "systemscope".to_owned(),
        version: "test".to_owned(),
        build: "counter".to_owned(),
    }
}

pub fn backend_with(halts: bool, bad_at: Option<u64>) -> RuntimeBackend {
    RuntimeBackend::new(
        id(),
        Box::new(move |_, input| match input {
            InputVariant::Media(_) => Err("the counter has no media".to_owned()),
            _ => Ok(runtime(halts, bad_at)),
        }),
        Box::new(|| Box::new(CounterProjection::default())),
    )
}

/// Patches `counter/n` after checking its old value.
pub fn counter_patch(bytes: &mut [u8], p: &StatePatch) -> Result<(), String> {
    if p.field != "counter/n" {
        return Err(format!("no field {}", p.field));
    }
    let at = bytes
        .windows(8)
        .position(|w| w == MARK.to_le_bytes())
        .ok_or("no counter state")?
        + 8;
    let old = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let (Value::U64(want), Value::U64(new)) = (&p.old, &p.new) else {
        return Err("not a U64 patch".to_owned());
    };
    if old != *want {
        return Err(format!("counter/n is {old}, not {want}"));
    }
    bytes[at..at + 8].copy_from_slice(&new.to_le_bytes());
    Ok(())
}

/// [`backend`] with every `uart_tx` byte one more: only the auxiliary stream differs.
pub fn shifted_backend() -> RuntimeBackend {
    RuntimeBackend::new(
        id(),
        Box::new(|_, _| Ok(runtime(true, None))),
        Box::new(|| {
            Box::new(CounterProjection {
                shift: 1,
                ..CounterProjection::default()
            })
        }),
    )
}

/// A backend that can only record whole streams, as Spike: a [`backend`] that never steps,
/// snapshots, restores, or reports state.
pub struct PostRun(pub RuntimeBackend);

impl Backend for PostRun {
    fn describe(&self) -> BackendId {
        BackendId {
            name: "post-run".to_owned(),
            ..self.0.describe()
        }
    }

    fn capabilities(&self) -> Capabilities {
        let keep = [
            Capability::ArchStream,
            Capability::EffectStream,
            Capability::DeterministicReplay,
        ];
        self.0
            .capabilities()
            .into_iter()
            .filter(|c| keep.contains(c))
            .collect()
    }

    fn support(&self) -> Support {
        self.0.support()
    }

    fn start(
        &mut self,
        scenario: &Scenario,
        input: &InputVariant,
        config: &BackendRunConfig,
        profile: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        self.0.start(scenario, input, config, profile)
    }

    fn next(&mut self, _stream: &str) -> Result<Option<Observation>, EngineError> {
        Err(EngineError::Unsupported("next".to_owned()))
    }

    fn record(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        self.0.record(stream)
    }
}

/// A [`PostRun`] side.
pub fn post_run() -> Box<dyn Backend> {
    Box::new(PostRun(backend()))
}

/// [`backend`] against [`shifted_backend`]: only the auxiliary stream differs.
pub fn shifted_sides() -> [Builder<'static>; 2] {
    [
        Builder {
            make: Make::Runtime(&backend),
            config: config(),
            perturbation: None,
        },
        Builder {
            make: Make::Runtime(&shifted_backend),
            config: config(),
            perturbation: None,
        },
    ]
}

pub fn backend() -> RuntimeBackend {
    backend_with(true, None).with_patch(Box::new(counter_patch))
}

pub fn config() -> BackendRunConfig {
    BackendRunConfig { budget: 1_000 }
}

pub fn side(b: &mut RuntimeBackend, input: InputVariant) -> Side<'_> {
    Side {
        backend: b,
        input,
        config: config(),
    }
}

pub fn patch_n(event_index: u64, old: u64, new: u64) -> InputVariant {
    InputVariant::State(StatePatch {
        event_index,
        field: "counter/n".to_owned(),
        old: Value::U64(old),
        new: Value::U64(new),
    })
}
