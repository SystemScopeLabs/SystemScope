//! The SystemScope runtime adapter on a small counter component: projection, ordinals,
//! the stop rule, state patches, failures, and observer invariance
//! (`docs/m4-design.md` §9.2, §20, §21, §25.1).

use systemscope_contracts::component::{Component, Delivered, InitContext, PortSpec, SimContext};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Duration, SimulationClock};
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_verify::backend::{Backend, Capability, Side, run, verify};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario, StatePatch};
use systemscope_verify::compare::{
    Divergence, Outcome, StreamResult, Verdict, compare, naive_first_divergence,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Requirement, Role, StreamSpec, Support, plan,
};
use systemscope_verify::runtime::{Projected, Projection, RuntimeBackend};

/// Marks the counter's state in snapshot bytes, so a test patch can find it.
const MARK: u64 = 0x5EED_C0DE_5EED_C0DE;
const LIMIT: u64 = 12;

/// Counts wakes up to [`LIMIT`]: each wake traces `t.step`, every fourth `t.out`, every
/// fifth `t.noise`, and the last `t.halt`. `bad_at` adds a record the projection refuses.
struct Counter {
    n: u64,
    halts: bool,
    bad_at: Option<u64>,
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
        ctx.wake_self(ScheduleWhen::After(Duration::from_ns(1)), Phase::Commit, 0)
    }
    fn handle_event(&mut self, _: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.n += 1;
        let n = self.n;
        ctx.trace("t.step", vec![("n", Value::U64(n))]);
        if n.is_multiple_of(4) {
            ctx.trace("t.out", vec![("byte", Value::U64(n))]);
        }
        if n.is_multiple_of(5) {
            ctx.trace("t.noise", vec![("n", Value::U64(n))]);
        }
        if self.bad_at == Some(n) {
            ctx.trace("t.bad", Vec::new());
        }
        if n == LIMIT && self.halts {
            ctx.trace("t.halt", Vec::new());
            return Ok(());
        }
        if n < LIMIT {
            ctx.wake_self(ScheduleWhen::After(Duration::from_ns(1)), Phase::Commit, 0)?;
        }
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        1
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

fn runtime(halts: bool, bad_at: Option<u64>) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "counter",
        Box::new(Counter {
            n: 0,
            halts,
            bad_at,
        }),
    );
    t.elaborate(SessionConfig::default()).unwrap()
}

/// `t.step` is a `retire` (pc `4n`, `rd_value` absent when 3 divides `n`, `paddr` `n`),
/// `t.noise` an `interrupt`, `t.out` a `uart_tx`, and `t.halt` the stop rule.
#[derive(Default)]
struct CounterProjection {
    finished: bool,
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
            "t.out" => out.push(p("effect", "uart_tx", vec![("byte", u(n))])),
            "t.halt" => self.finished = true,
            other => return Err(format!("unreadable record {other}")),
        }
        Ok(())
    }
    fn finished(&self) -> bool {
        self.finished
    }
}

fn field(name: &str, requirement: Requirement, may_be_absent: bool) -> FieldSpec {
    FieldSpec {
        name: name.to_owned(),
        requirement,
        may_be_absent,
    }
}

/// `arch` compares `retire` and `exception`, never `interrupt`; `effect` is auxiliary.
fn profile() -> ComparisonProfile {
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

fn scenario() -> Scenario {
    Scenario {
        platform: "counter".to_owned(),
        topology_hash: runtime(true, None).topology_hash(),
        cpu_profile: "none".to_owned(),
        program: [0; 32],
        locator: String::new(),
        seed: 0,
    }
}

fn id() -> BackendId {
    BackendId {
        name: "systemscope".to_owned(),
        version: "test".to_owned(),
        build: "counter".to_owned(),
    }
}

fn backend_with(halts: bool, bad_at: Option<u64>) -> RuntimeBackend {
    RuntimeBackend::new(
        id(),
        Box::new(move |_| Ok(runtime(halts, bad_at))),
        Box::new(|| Box::new(CounterProjection::default())),
    )
}

/// Patches `counter/n` after checking its old value.
fn counter_patch(bytes: &mut [u8], p: &StatePatch) -> Result<(), String> {
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

fn backend() -> RuntimeBackend {
    backend_with(true, None).with_patch(Box::new(counter_patch))
}

fn config() -> BackendRunConfig {
    BackendRunConfig { budget: 1_000 }
}

fn side(b: &mut RuntimeBackend, input: InputVariant) -> Side<'_> {
    Side {
        backend: b,
        input,
        config: config(),
    }
}

fn patch_n(event_index: u64, old: u64, new: u64) -> InputVariant {
    InputVariant::State(StatePatch {
        event_index,
        field: "counter/n".to_owned(),
        old: Value::U64(old),
        new: Value::U64(new),
    })
}

fn failure(b: &mut RuntimeBackend, input: InputVariant, budget: u64) -> EngineError {
    let plan = plan(&profile(), &b.support(), &b.support()).unwrap();
    let side = Side {
        backend: b,
        input,
        config: BackendRunConfig { budget },
    };
    run(&plan, &scenario(), side).complete().unwrap_err()
}

#[test]
fn identical_sides_agree_to_the_stop_rule() {
    let (mut e, mut a) = (backend(), backend());
    let verdict = verify(
        &profile(),
        &scenario(),
        side(&mut e, InputVariant::None),
        side(&mut a, InputVariant::None),
    )
    .unwrap();
    let Verdict::Compared(c) = verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!(c.outcome(), Outcome::Agree);
    assert!(matches!(
        c.primary,
        StreamResult::Agree {
            compared: LIMIT,
            ..
        }
    ));
    assert!(matches!(
        c.auxiliary[0],
        StreamResult::Agree { compared: 3, .. }
    ));
}

#[test]
fn observations_follow_the_profile_and_count_only_compared_kinds() {
    let mut b = backend();
    let plan = plan(&profile(), &b.support(), &b.support()).unwrap();
    let recorded = run(&plan, &scenario(), side(&mut b, InputVariant::None))
        .complete()
        .unwrap();
    let arch = &recorded[0];
    // Twelve retirements; the two `interrupt`s are not compared and take no ordinal.
    assert_eq!(arch.len(), LIMIT as usize);
    for (i, o) in arch.iter().enumerate() {
        let n = i as u64 + 1;
        assert_eq!((o.stream.as_str(), o.ordinal), ("arch", i as u64));
        let names: Vec<_> = o.fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["pc", "rd_value", "paddr"]);
        assert_eq!(
            o.field("pc"),
            Some(&ObservedValue::Present(Value::U64(4 * n)))
        );
        assert_eq!(
            o.field("rd_value").unwrap() == &ObservedValue::Absent,
            n.is_multiple_of(3)
        );
        // Retirement `i` is emitted by event `i + 1`.
        assert_eq!(b.event_index("arch", i as u64), Some(n));
    }
    let bytes: Vec<_> = recorded[1]
        .iter()
        .map(|o| o.field("byte").cloned())
        .collect();
    assert_eq!(
        bytes,
        [4, 8, 12].map(|v| Some(ObservedValue::Present(Value::U64(v))))
    );
    assert_eq!(b.event_index("effect", 2), Some(12));
    assert_eq!(b.event_index("effect", 3), None);
    assert_eq!(b.event_index("os", 0), None);
}

#[test]
fn a_state_patch_diverges_at_the_next_boundary() {
    for boundary in [0, 4, 9] {
        let mut e = backend();
        let plan = plan(&profile(), &e.support(), &e.support()).unwrap();
        let expected = run(&plan, &scenario(), side(&mut e, InputVariant::None))
            .complete()
            .unwrap();
        let event = e.event_index("arch", boundary).unwrap();
        let n = boundary + 1;
        let mut a = backend();
        let actual = run(&plan, &scenario(), side(&mut a, patch_n(event, n, n + 1)))
            .complete()
            .unwrap();
        let oracle = naive_first_divergence(&expected[0], &actual[0]);
        assert_eq!(oracle, Some(boundary + 1), "{boundary}");
        let Verdict::Compared(c) = compare(&plan, &expected, &actual).unwrap() else {
            panic!()
        };
        assert_eq!(c.outcome(), Outcome::Diverged);
        let StreamResult::Diverged {
            ordinal,
            divergence,
            ..
        } = c.primary
        else {
            panic!()
        };
        assert_eq!(Some(ordinal), oracle);
        let Divergence::Fields { fields, .. } = divergence else {
            panic!("{divergence:?}")
        };
        assert_eq!(fields[0].name, "pc");
        assert_eq!(
            fields[0].actual,
            ObservedValue::Present(Value::U64(4 * (n + 2)))
        );
        // The patched side retires one fewer: n + 1 is skipped.
        assert_eq!(actual[0].len(), expected[0].len() - 1);
        // The prefix before the patch is the expected one, observations and events alike.
        assert_eq!(
            actual[0][..=boundary as usize],
            expected[0][..=boundary as usize]
        );
        assert_eq!(a.event_index("arch", boundary), Some(event));
    }
}

#[test]
fn a_patch_with_the_wrong_old_value_or_refused_state_fails_the_side() {
    let mut b = backend();
    let e = failure(&mut b, patch_n(3, 99, 5), 1_000);
    assert_eq!(
        e,
        EngineError::BackendFailed("state patch: counter/n is 3, not 99".to_owned())
    );
    let e = failure(&mut b, patch_n(3, 3, LIMIT + 1), 1_000);
    let EngineError::BackendFailed(why) = e else {
        panic!("{e:?}")
    };
    assert!(
        why.starts_with("state patch: the patched checkpoint was refused:"),
        "{why}"
    );
    assert!(why.contains("counter: past the limit"), "{why}");
}

#[test]
fn a_state_patch_needs_a_patcher() {
    let mut b = backend_with(true, None);
    let plan = plan(&profile(), &b.support(), &b.support()).unwrap();
    assert_eq!(
        run(&plan, &scenario(), side(&mut b, patch_n(1, 1, 2))).complete(),
        Err(EngineError::Unsupported("state patch"))
    );
    // Refused when the side starts, before any event.
    assert_eq!(
        b.start(&scenario(), &patch_n(1, 1, 2), &config(), &profile()),
        Err(EngineError::Unsupported("state patch"))
    );
}

#[test]
fn a_side_that_misses_the_stop_rule_fails() {
    let mut b = backend();
    assert_eq!(
        failure(&mut b, InputVariant::None, 5),
        EngineError::BackendFailed(
            "the safety budget of 5 events ran out before the stop rule".to_owned()
        )
    );
    // A budget of exactly the events the run needs is enough.
    let plan = plan(&profile(), &b.support(), &b.support()).unwrap();
    let exact = Side {
        backend: &mut b,
        input: InputVariant::None,
        config: BackendRunConfig { budget: LIMIT },
    };
    assert!(run(&plan, &scenario(), exact).complete().is_ok());
    let mut endless = backend_with(false, None);
    assert_eq!(
        failure(&mut endless, InputVariant::None, 1_000),
        EngineError::BackendFailed(
            "the event queue drained after 12 events, before the stop rule".to_owned()
        )
    );
}

#[test]
fn a_record_the_projection_cannot_read_fails_the_side() {
    let mut b = backend_with(true, Some(7));
    assert_eq!(
        failure(&mut b, InputVariant::None, 1_000),
        EngineError::BackendFailed("event 7: unreadable record t.bad".to_owned())
    );
}

#[test]
fn the_projection_must_accept_the_profile() {
    let mut b = backend();
    let mut other = profile();
    other.stop_rule = "never".to_owned();
    let e = b
        .start(&scenario(), &InputVariant::None, &config(), &other)
        .unwrap_err();
    assert_eq!(
        e,
        EngineError::Internal("projection refused the profile: unknown stop rule never".to_owned())
    );
}

#[test]
fn next_needs_a_started_side_and_a_compared_stream() {
    let mut b = backend();
    assert!(matches!(b.next("arch"), Err(EngineError::Internal(_))));
    b.start(&scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    assert!(matches!(b.next("os"), Err(EngineError::Internal(_))));
    assert!(b.next("arch").unwrap().is_some());
}

#[test]
fn the_adapter_declares_what_its_projection_reports() {
    let b = backend();
    assert_eq!(b.describe(), id());
    let caps = b.capabilities();
    for c in [
        Capability::ArchStream,
        Capability::EffectStream,
        Capability::StepBoundary,
        Capability::DeterministicReplay,
    ] {
        assert!(caps.contains(&c), "{c:?}");
    }
    assert!(!caps.contains(&Capability::OsStream));
}

#[test]
fn observing_changes_nothing() {
    let mut plain = runtime(true, None);
    plain.init().unwrap();
    while plain.step().unwrap().is_some() {}
    let mut b = backend();
    let plan = plan(&profile(), &b.support(), &b.support()).unwrap();
    run(&plan, &scenario(), side(&mut b, InputVariant::None))
        .complete()
        .unwrap();
    let observed = b.runtime().unwrap();
    assert_eq!(observed.state_digest(), plain.state_digest());
    assert_eq!(observed.execution_digest(), plain.execution_digest());
}

#[test]
fn a_divergence_before_the_patched_side_fails_is_reported() {
    // n = 11 becomes 12 after boundary 10, so the counter passes its limit without the
    // halt, and its queue drains; its retirement 11 already differs.
    let mut e = backend();
    let plan = plan(&profile(), &e.support(), &e.support()).unwrap();
    run(&plan, &scenario(), side(&mut e, InputVariant::None));
    let event = e.event_index("arch", 10).unwrap();
    let (mut e, mut a) = (backend(), backend());
    let verdict = verify(
        &profile(),
        &scenario(),
        side(&mut e, InputVariant::None),
        side(&mut a, patch_n(event, 11, 12)),
    )
    .unwrap();
    let Verdict::Compared(c) = verdict else {
        panic!("{verdict:?}")
    };
    assert!(matches!(
        c.primary,
        StreamResult::Diverged { ordinal: 11, .. }
    ));
    assert_eq!(
        c.failures,
        [(
            Role::Actual,
            EngineError::BackendFailed(
                "the event queue drained after 12 events, before the stop rule".to_owned()
            )
        )]
    );
}
