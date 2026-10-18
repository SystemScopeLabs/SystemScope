//! Identities of scenarios, input variants, and backends, the execution identity, the
//! default `record`, and engine error messages (`docs/m4-design.md` §6.6, §9.2, §20).

use systemscope_contracts::trace::Value;
use systemscope_verify::backend::{Backend, Capabilities, Capability};
use systemscope_verify::case::{
    BackendId, BackendRunConfig, InputVariant, MediaPatch, Scenario, StatePatch, execution_id,
};
use systemscope_verify::checkpoint::Checkpoint;
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
    let media = || MediaPatch {
        offset: 8,
        old: vec![1, 2],
        new: vec![3, 4],
    };
    let edits: [fn(&mut MediaPatch); 4] = [
        |p| p.offset = 9,
        |p| p.old[1] = 5,
        |p| p.new[0] = 5,
        // The lengths are encoded: moving a byte from old to new is another patch.
        |p| {
            p.old = vec![1];
            p.new = vec![2, 3, 4];
        },
    ];
    ids.push(InputVariant::Media(media()).identity());
    for edit in edits {
        let mut p = media();
        edit(&mut p);
        ids.push(InputVariant::Media(p).identity());
    }
    distinct(&ids);
    assert_eq!(state(1).identity(), state(1).identity());
    let mut e = systemscope_contracts::canonical::Encoder::new();
    e.u8(2);
    e.u64(8);
    e.bytes(&[1, 2]);
    e.bytes(&[3, 4]);
    assert_eq!(
        InputVariant::Media(media()).identity(),
        *blake3::hash(e.as_bytes()).as_bytes()
    );
}

#[test]
fn a_media_patch_checks_the_bytes_it_replaces() {
    let patch = |offset, old: &[u8], new: &[u8]| MediaPatch {
        offset,
        old: old.to_vec(),
        new: new.to_vec(),
    };
    let mut image = vec![0, 1, 2, 3];
    patch(2, &[2, 3], &[7, 8]).apply(&mut image).unwrap();
    assert_eq!(image, [0, 1, 7, 8]);
    patch(0, &[], &[]).apply(&mut image).unwrap();
    patch(4, &[], &[]).apply(&mut image).unwrap();
    assert_eq!(image, [0, 1, 7, 8]);
    for (p, why) in [
        (patch(0, &[0], &[1, 2]), "media patch: 1 old bytes, 2 new"),
        (
            patch(3, &[8, 0], &[1, 1]),
            "media patch: 2 bytes at 0x3 are outside the 4-byte image",
        ),
        (
            patch(u64::MAX, &[0], &[1]),
            "media patch: 1 bytes at 0xffffffffffffffff are outside the 4-byte image",
        ),
        (
            patch(1, &[1, 2], &[0, 0]),
            "media patch: the image holds [01, 07] at 0x1, not [01, 02]",
        ),
    ] {
        assert_eq!(p.apply(&mut image), Err(why.to_owned()));
        assert_eq!(image, [0, 1, 7, 8]);
    }
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
    assert_eq!(
        b.record("arch"),
        Err(EngineError::Unsupported("record".to_owned()))
    );
    assert_eq!(b.left, 3);
}

#[test]
fn checkpoints_and_emitted_observations_are_unsupported_by_default() {
    let mut b = Fixed {
        step: true,
        left: 3,
    };
    assert_eq!(
        b.emitted("arch"),
        Err(EngineError::Unsupported("emitted".to_owned()))
    );
    assert_eq!(
        b.checkpoint(),
        Err(EngineError::Unsupported("checkpoint".to_owned()))
    );
    let c = Checkpoint {
        execution: [0; 32],
        event_index: 0,
        streams: Vec::new(),
        state: Vec::new(),
    };
    assert_eq!(
        b.restore(&c),
        Err(EngineError::Unsupported("restore".to_owned()))
    );
    assert_eq!(b.left, 3);
}

#[test]
fn engine_errors_say_what_failed() {
    let cases = [
        (
            EngineError::BackendFailed("x".to_owned()),
            "backend failed: x",
        ),
        (
            EngineError::Unsupported("record".to_owned()),
            "unsupported: record",
        ),
        (EngineError::Internal("y".to_owned()), "internal error: y"),
        (
            EngineError::BoundaryDesync("z".to_owned()),
            "boundary desync: z",
        ),
    ];
    for (e, text) in cases {
        assert_eq!(e.to_string(), text);
    }
}
