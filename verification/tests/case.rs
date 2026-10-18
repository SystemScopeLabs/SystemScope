//! Identities of scenarios, input variants, and backends, the execution identity, the
//! default `record`, and engine error messages (`docs/m4-design.md` §6.6, §9.2, §20).

use systemscope_contracts::trace::Value;
use systemscope_verify::backend::{Backend, Capabilities, Capability};
use systemscope_verify::case::{
    BackendId, BackendRunConfig, InputVariant, Scenario, StatePatch, execution_id,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{ComparisonProfile, Support};

fn scenario() -> Scenario {
    Scenario {
        platform: "p".to_owned(),
        topology_hash: [1; 32],
        cpu_profile: "M3".to_owned(),
        program: [2; 32],
        locator: "disk.img".to_owned(),
        seed: 3,
    }
}

fn state(event_index: u64) -> InputVariant {
    InputVariant::State(StatePatch {
        event_index,
        field: "cpu/pc".to_owned(),
        old: Value::U64(4),
        new: Value::U64(0),
    })
}

fn id() -> BackendId {
    BackendId {
        name: "b".to_owned(),
        version: "1".to_owned(),
        build: "x".to_owned(),
    }
}

/// Asserts every identity in `ids` differs from every other.
fn distinct(ids: &[[u8; 32]]) {
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn a_scenario_identity_covers_every_field_but_the_locator() {
    let s = scenario();
    let mut moved = s.clone();
    moved.locator = "elsewhere/disk.img".to_owned();
    assert_eq!(moved.identity(), s.identity());
    let edits: [fn(&mut Scenario); 5] = [
        |s| s.platform.push('x'),
        |s| s.topology_hash[0] ^= 1,
        |s| s.cpu_profile.push('x'),
        |s| s.program[31] ^= 1,
        |s| s.seed += 1,
    ];
    let mut ids = vec![s.identity()];
    for edit in edits {
        let mut e = s.clone();
        edit(&mut e);
        ids.push(e.identity());
    }
    distinct(&ids);
}

#[test]
fn an_input_variant_identity_covers_the_whole_patch() {
    let mut ids = vec![
        InputVariant::None.identity(),
        state(1).identity(),
        state(2).identity(),
    ];
    let edits: [fn(&mut StatePatch); 3] = [
        |p| p.field.push('x'),
        |p| p.old = Value::U64(5),
        |p| p.new = Value::U64(1),
    ];
    for edit in edits {
        let InputVariant::State(mut p) = state(1) else {
            unreachable!()
        };
        edit(&mut p);
        ids.push(InputVariant::State(p).identity());
    }
    distinct(&ids);
    assert_eq!(state(1).identity(), state(1).identity());
}

#[test]
fn a_backend_identity_covers_every_field() {
    let edits: [fn(&mut BackendId); 3] = [
        |b| b.name.push('x'),
        |b| b.version.push('x'),
        |b| b.build.push('x'),
    ];
    let mut ids = vec![id().identity()];
    for edit in edits {
        let mut b = id();
        edit(&mut b);
        ids.push(b.identity());
    }
    distinct(&ids);
}

#[test]
fn the_execution_identity_hashes_its_three_parts_in_order() {
    let (s, i, b) = (scenario(), state(1), id());
    let mut h = blake3::Hasher::new();
    h.update(&s.identity());
    h.update(&i.identity());
    h.update(&b.identity());
    assert_eq!(execution_id(&s, &i, &b), *h.finalize().as_bytes());
    distinct(&[
        execution_id(&s, &i, &b),
        execution_id(&s, &InputVariant::None, &b),
        execution_id(
            &s,
            &i,
            &BackendId {
                build: "y".to_owned(),
                ..id()
            },
        ),
    ]);
}

/// A backend that reports `left` observations, with or without `StepBoundary`.
struct Fixed {
    step: bool,
    left: u64,
}

impl Backend for Fixed {
    fn describe(&self) -> BackendId {
        id()
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = Capabilities::from([Capability::ArchStream]);
        if self.step {
            c.insert(Capability::StepBoundary);
        }
        c
    }

    fn support(&self) -> Support {
        Support::default()
    }

    fn start(
        &mut self,
        _: &Scenario,
        _: &InputVariant,
        _: &BackendRunConfig,
        _: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    fn next(&mut self, stream: &str) -> Result<Option<Observation>, EngineError> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        Ok(Some(Observation {
            stream: stream.to_owned(),
            ordinal: self.left,
            kind: "retire".to_owned(),
            fields: vec![("pc".to_owned(), ObservedValue::Present(Value::U64(0)))],
        }))
    }
}

#[test]
fn the_default_record_steps_to_the_stop_rule_and_needs_step_boundary() {
    let mut b = Fixed {
        step: true,
        left: 3,
    };
    let ordinals: Vec<u64> = b
        .record("arch")
        .unwrap()
        .iter()
        .map(|o| o.ordinal)
        .collect();
    assert_eq!(ordinals, [2, 1, 0]);
    assert_eq!(b.record("arch"), Ok(Vec::new()));
    let mut b = Fixed {
        step: false,
        left: 3,
    };
    assert_eq!(b.record("arch"), Err(EngineError::Unsupported("record")));
    assert_eq!(b.left, 3);
}

#[test]
fn engine_errors_say_what_failed() {
    let cases = [
        (
            EngineError::BackendFailed("x".to_owned()),
            "backend failed: x",
        ),
        (EngineError::Unsupported("record"), "unsupported: record"),
        (EngineError::Internal("y".to_owned()), "internal error: y"),
    ];
    for (e, text) in cases {
        assert_eq!(e.to_string(), text);
    }
}
