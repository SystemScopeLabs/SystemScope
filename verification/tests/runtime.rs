//! The SystemScope runtime adapter on a small counter component: projection, ordinals,
//! the stop rule, state patches, failures, and observer invariance
//! (`docs/m4-design.md` §9.2, §20, §21, §25.1).

mod common;

use common::*;
use systemscope_contracts::trace::Value;
use systemscope_verify::backend::{Backend, Capability, Side, run, verify};
use systemscope_verify::case::{BackendRunConfig, InputVariant, execution_id};
use systemscope_verify::compare::{
    Divergence, Outcome, StreamResult, Verdict, compare, naive_first_divergence,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::{Observation, ObservedValue, PrefixDigest};
use systemscope_verify::profile::{Role, plan};
use systemscope_verify::runtime::RuntimeBackend;

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
        Capability::Snapshot,
        Capability::Restore,
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
