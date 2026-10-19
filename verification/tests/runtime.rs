//! The SystemScope runtime adapter on a small counter component: projection, ordinals,
//! the stop rule, state patches, failures, and observer invariance
//! (`docs/m4-design.md` §9.2, §20, §21, §25.1).

mod common;

use common::*;
use systemscope_contracts::trace::TraceRecord;
use systemscope_contracts::trace::Value;
use systemscope_verify::backend::{Backend, Capability, Side, run, verify};
use systemscope_verify::case::{BackendRunConfig, InputVariant, execution_id};
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::compare::{
    Divergence, Outcome, StreamResult, Verdict, compare, naive_first_divergence,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::{Observation, ObservedValue, PrefixDigest};
use systemscope_verify::profile::{ComparisonProfile, Support};
use systemscope_verify::profile::{Role, plan};
use systemscope_verify::runtime::{Projected, Projection, RuntimeBackend};
use systemscope_verify::state::{Source, StateField};

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
        Err(EngineError::Unsupported("state patch".to_owned()))
    );
    // Refused when the side starts, before any event.
    assert_eq!(
        b.start(&scenario(), &patch_n(1, 1, 2), &config(), &profile()),
        Err(EngineError::Unsupported("state patch".to_owned()))
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
        Capability::Snapshot,
        Capability::Restore,
        Capability::State,
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

/// Every observation of both streams, taken stream by stream, from a started side.
fn rest(b: &mut RuntimeBackend) -> [Vec<Observation>; 2] {
    ["arch", "effect"].map(|s| {
        let mut out = Vec::new();
        while let Some(o) = b.next(s).unwrap() {
            out.push(o);
        }
        out
    })
}

fn started(input: &InputVariant) -> RuntimeBackend {
    let mut b = backend();
    b.start(&scenario(), input, &config(), &profile()).unwrap();
    b
}

#[test]
fn every_checkpoint_resumes_the_uninterrupted_streams() {
    // Unpatched, and patched after event 4: checkpoints before the patch must apply it on
    // resume, and the checkpoint after event 4 already holds it.
    for (input, retirements) in [(InputVariant::None, LIMIT), (patch_n(4, 4, 5), LIMIT - 1)] {
        let whole = rest(&mut started(&input));
        assert_eq!(whole[0].len() as u64, retirements);
        let mut b = started(&input);
        let mut checkpoints = vec![b.checkpoint().unwrap()];
        for _ in 1..retirements {
            b.next("arch").unwrap().unwrap();
            checkpoints.push(b.checkpoint().unwrap());
        }
        // The last retirement's event also halts: no checkpoint after the stop rule.
        b.next("arch").unwrap().unwrap();
        assert_eq!(
            b.checkpoint(),
            Err(EngineError::Internal(
                "checkpoint after the stop rule".to_owned()
            ))
        );
        let execution = execution_id(&scenario(), &input, &id());
        for (j, c) in checkpoints.iter().enumerate() {
            // One retirement per event; the effect stream counts what was emitted, taken
            // or not.
            assert_eq!((c.execution, c.event_index), (execution, j as u64), "{j}");
            let names: Vec<_> = c.streams.iter().map(|m| m.stream.as_str()).collect();
            assert_eq!(names, ["arch", "effect"]);
            for (s, stream) in whole.iter().enumerate() {
                let m = &c.streams[s];
                let covered = stream
                    .iter()
                    .filter(|o| emitting_event(o, &input) <= j as u64);
                let covered = covered.count() as u64;
                assert_eq!(m.covered, covered, "{j} {}", m.stream);
                let mut p = PrefixDigest::new();
                stream[..covered as usize].iter().for_each(|o| p.absorb(o));
                assert_eq!(m.prefix, p.digest(), "{j} {}", m.stream);
            }
            let mut r = started(&input);
            r.restore(c).unwrap();
            let resumed = rest(&mut r);
            for s in 0..2 {
                let from = c.streams[s].covered as usize;
                assert_eq!(resumed[s], whole[s][from..], "{j} stream {s}");
            }
            let first = c.streams[0].covered;
            assert_eq!(r.event_index("arch", first), Some(j as u64 + 1));
            if first > 0 {
                assert_eq!(r.event_index("arch", first - 1), None);
            }
        }
    }
}

/// The event that emits observation `o` of a counter run with `input`: retirement `i` is
/// event `i + 1`, or `i + 2` once a patch after event 4 skips `n = 5`; a byte is event `n`.
fn emitting_event(o: &Observation, input: &InputVariant) -> u64 {
    let skipped = matches!(input, InputVariant::State(_));
    match o.field("byte") {
        Some(ObservedValue::Present(Value::U64(n))) => *n - u64::from(skipped && *n > 4),
        _ => o.ordinal + 1,
    }
}

/// [`CounterProjection`] whose `uart_tx` byte is the count of bytes before it: state
/// across event boundaries, which it saves into each checkpoint.
#[derive(Default)]
struct Tally {
    inner: CounterProjection,
    outs: u64,
}

impl Projection for Tally {
    fn support(&self) -> Support {
        self.inner.support()
    }
    fn begin(&mut self, profile: &ComparisonProfile) -> Result<(), String> {
        self.inner.begin(profile)
    }
    fn project(&mut self, record: &TraceRecord, out: &mut Vec<Projected>) -> Result<(), String> {
        let from = out.len();
        self.inner.project(record, out)?;
        for p in &mut out[from..] {
            if p.stream == "effect" {
                p.fields = vec![(
                    "byte".to_owned(),
                    ObservedValue::Present(Value::U64(self.outs)),
                )];
                self.outs += 1;
            }
        }
        Ok(())
    }
    fn finished(&self) -> bool {
        self.inner.finished()
    }
    fn save(&self) -> Vec<u8> {
        self.outs.to_le_bytes().to_vec()
    }
    fn load(&mut self, state: &[u8]) -> Result<(), String> {
        let bytes = state.try_into().map_err(|_| format!("{state:?}"))?;
        self.outs = u64::from_le_bytes(bytes);
        Ok(())
    }
}

fn tally_backend() -> RuntimeBackend {
    RuntimeBackend::new(
        id(),
        Box::new(|_, _| Ok(runtime(true, None))),
        Box::new(|| Box::new(Tally::default())),
    )
}

#[test]
fn a_projection_with_state_resumes_from_the_state_it_saved() {
    let start = |mut b: RuntimeBackend| {
        b.start(&scenario(), &InputVariant::None, &config(), &profile())
            .unwrap();
        b
    };
    let whole = rest(&mut start(tally_backend()));
    let bytes: Vec<u64> = (0..whole[1].len() as u64).collect();
    let seen: Vec<u64> = whole[1]
        .iter()
        .map(|o| match o.field("byte") {
            Some(ObservedValue::Present(Value::U64(n))) => *n,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(seen, bytes);
    let mut b = start(tally_backend());
    let mut checkpoints = vec![b.checkpoint().unwrap()];
    for _ in 1..LIMIT {
        b.next("arch").unwrap().unwrap();
        checkpoints.push(b.checkpoint().unwrap());
    }
    for (j, c) in checkpoints.iter().enumerate() {
        assert_eq!(c.projection, c.streams[1].covered.to_le_bytes(), "{j}");
        let mut r = start(tally_backend());
        r.restore(c).unwrap();
        let resumed = rest(&mut r);
        for s in 0..2 {
            let from = c.streams[s].covered as usize;
            assert_eq!(resumed[s], whole[s][from..], "{j} stream {s}");
        }
    }
    // A stateless projection's checkpoint carries none, and refuses one; the stateful one
    // refuses a state it did not save.
    let stateless = started(&InputVariant::None).checkpoint().unwrap();
    assert!(stateless.projection.is_empty());
    let mut r = started(&InputVariant::None);
    let with = Checkpoint {
        projection: vec![1],
        ..stateless.clone()
    };
    assert_eq!(
        r.restore(&with),
        Err(EngineError::Internal(
            "projection state: the projection keeps no state".to_owned()
        ))
    );
    let mut r = start(tally_backend());
    assert_eq!(
        r.restore(&stateless),
        Err(EngineError::Internal("projection state: []".to_owned()))
    );
}

#[test]
fn restore_refuses_misuse_and_other_executions() {
    let mut b = backend();
    let internal = |e: EngineError| matches!(e, EngineError::Internal(_));
    assert!(internal(b.checkpoint().unwrap_err()));
    assert!(internal(b.emitted("arch").unwrap_err()));
    let c = started(&InputVariant::None).checkpoint().unwrap();
    assert!(internal(b.restore(&c).unwrap_err()));
    // Another input variant is another execution.
    let mut p = started(&patch_n(4, 4, 5));
    assert_eq!(
        p.restore(&c),
        Err(EngineError::Internal(
            "the checkpoint belongs to another execution".to_owned()
        ))
    );
    let mut b = started(&InputVariant::None);
    let mut missing = c.clone();
    missing.streams.pop();
    assert_eq!(
        b.restore(&missing),
        Err(EngineError::Internal(
            "the checkpoint marks [\"arch\"], not the compared streams".to_owned()
        ))
    );
    let mut corrupt = c.clone();
    corrupt.state.truncate(10);
    let e = b.restore(&corrupt).unwrap_err();
    assert!(
        matches!(&e, EngineError::BackendFailed(why) if why.starts_with("restore: ")),
        "{e:?}"
    );
    // A side whose projection failed has no checkpoint.
    let mut bad = backend_with(true, Some(2));
    bad.start(&scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    let e = loop {
        if let Err(e) = bad.next("arch") {
            break e;
        }
    };
    assert_eq!(bad.checkpoint(), Err(e));
}

#[test]
fn emitted_takes_queued_observations_without_advancing() {
    let mut b = started(&InputVariant::None);
    for _ in 0..4 {
        b.next("arch").unwrap().unwrap();
    }
    let effect = b.emitted("effect").unwrap();
    assert_eq!(effect.len(), 1);
    assert_eq!(
        effect[0].field("byte"),
        Some(&ObservedValue::Present(Value::U64(4)))
    );
    assert_eq!(b.emitted("effect").unwrap(), Vec::new());
    assert_eq!(b.emitted("arch").unwrap(), Vec::new());
    assert_eq!(b.next("arch").unwrap().unwrap().ordinal, 4);
    assert!(matches!(b.emitted("os"), Err(EngineError::Internal(_))));
}

/// The counter's state as the adapter reads it: its `inspect()`, then its snapshot schema
/// and the BLAKE3 of its snapshot bytes.
fn counter_state(n: u64) -> Vec<StateField> {
    let field = |field: &str, value, source| StateField {
        component: "counter".to_owned(),
        field: field.to_owned(),
        value,
        source,
    };
    let mut bytes = MARK.to_le_bytes().to_vec();
    bytes.extend(n.to_le_bytes());
    let derived = || Source::Derived("snapshot".to_owned());
    vec![
        field("n", Value::U64(n), Source::Inspect),
        field("halts", Value::Bool(true), Source::Inspect),
        field("schema", Value::U64(1), derived()),
        field(
            "blake3",
            Value::Bytes(blake3::hash(&bytes).as_bytes().to_vec()),
            derived(),
        ),
    ]
}

#[test]
fn the_state_is_read_at_the_current_boundary_without_advancing() {
    let mut b = backend();
    assert_eq!(
        b.state(),
        Err(EngineError::Internal("state before start".to_owned()))
    );
    let input = patch_n(4, 4, 5);
    let whole = rest(&mut started(&input));
    let mut b = started(&input);
    assert_eq!(b.state(), Ok(counter_state(0)));
    // After the fourth retirement the patch has taken the counter to 5.
    let mut arch = Vec::new();
    for n in [1, 2, 3, 5, 6] {
        arch.push(b.next("arch").unwrap().unwrap());
        let digest = b.runtime().unwrap().state_digest().unwrap();
        let execution = b.runtime().unwrap().execution_digest();
        assert_eq!(b.state(), Ok(counter_state(n)), "{n}");
        assert_eq!(b.state(), Ok(counter_state(n)), "{n}");
        assert_eq!(b.runtime().unwrap().state_digest().unwrap(), digest);
        assert_eq!(b.runtime().unwrap().execution_digest(), execution);
    }
    // Reading changed nothing: the streams go on as if it never happened.
    let [rest_arch, effect] = rest(&mut b);
    arch.extend(rest_arch);
    assert_eq!([arch, effect], whole);
    assert_eq!(b.state(), Ok(counter_state(LIMIT)));
    // A restored side reads its restored state.
    let mut b = started(&input);
    b.next("arch").unwrap().unwrap();
    let c = b.checkpoint().unwrap();
    let mut r = started(&input);
    r.restore(&c).unwrap();
    assert_eq!(r.state(), Ok(counter_state(1)));
}

#[test]
fn a_decoder_adds_its_fields_to_every_state_read() {
    let decoded = |bytes: &[u8]| StateField {
        component: "counter".to_owned(),
        field: "bytes".to_owned(),
        value: Value::U64(bytes.len() as u64),
        source: Source::Decoder("test/1".to_owned()),
    };
    let mut b = backend().with_decoder(Box::new(move |bytes| Ok(vec![decoded(bytes)])));
    b.start(&scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    let len = b.runtime().unwrap().snapshot().unwrap().len();
    let mut want = counter_state(0);
    want.push(decoded(&vec![0; len]));
    assert_eq!(b.state(), Ok(want));
    let mut b = backend().with_decoder(Box::new(|_| Err("unreadable".to_owned())));
    b.start(&scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    assert_eq!(
        b.state(),
        Err(EngineError::BackendFailed(
            "state decoder: unreadable".to_owned()
        ))
    );
}

#[test]
fn step_event_dispatches_one_event() {
    let mut b = backend();
    assert_eq!(b.events(), None);
    assert_eq!(
        b.step_event(),
        Err(EngineError::Internal("step before start".to_owned()))
    );
    let mut b = started(&patch_n(2, 2, 7));
    for k in 0..4 {
        assert_eq!(b.events(), Some(k));
        b.step_event().unwrap();
    }
    // The patch after event 2 applies on the way: n becomes 1, 2 (patched to 7), 8, 9.
    assert_eq!(b.state().unwrap()[0].value, Value::U64(9));
    // A side whose projection failed keeps its failure.
    let mut bad = backend_with(true, Some(1));
    bad.start(&scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    let e = bad.next("arch").and_then(|_| bad.next("arch")).unwrap_err();
    assert_eq!(bad.step_event(), Err(e));
}

#[test]
fn the_build_sees_the_input_variant() {
    // The counter's build refuses media patches; the adapter itself never applies them.
    let media = InputVariant::Media(systemscope_verify::case::MediaPatch {
        offset: 0,
        old: vec![0],
        new: vec![1],
    });
    assert_eq!(
        failure(&mut backend(), media, 1_000),
        EngineError::BackendFailed("the counter has no media".to_owned())
    );
}
