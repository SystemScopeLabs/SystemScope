//! Committed streams replayed as the expected side (`docs/m4-design.md` §25.2): the
//! recording round-trips, agrees with the run it was made from, locates a divergence by
//! stream compare as the naive oracle does, and refuses another case.

mod common;

use std::collections::BTreeMap;

use common::*;

use systemscope_verify::backend::{Backend, Capability, Side, run, verify};
use systemscope_verify::case::{InputVariant, Scenario};
use systemscope_verify::compare::{StreamResult, Verdict};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{Strategy, locate};
use systemscope_verify::observation::Observation;
use systemscope_verify::profile::{ComparisonProfile, Plan, plan};
use systemscope_verify::recorded::{RecordedBackend, stream_bytes, stream_from_bytes};

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

/// The unmodified counter's streams, through their committed bytes.
fn recording() -> RecordedBackend {
    let plan = the_plan();
    let mut b = backend();
    let r = run(&plan, &scenario(), side(&mut b, InputVariant::None));
    assert_eq!(r.failure, None);
    let streams: BTreeMap<String, Vec<Observation>> = plan
        .profile
        .streams()
        .zip(r.streams)
        .map(|(s, obs)| {
            let decoded = stream_from_bytes(&stream_bytes(&obs)).unwrap();
            assert_eq!(decoded, obs);
            (s.stream.clone(), decoded)
        })
        .collect();
    RecordedBackend::new(id(), b.support(), &scenario(), &plan.profile, streams)
}

fn side_of(r: &mut RecordedBackend, input: InputVariant) -> Side<'_> {
    Side {
        backend: r,
        input,
        config: config(),
    }
}

#[test]
fn a_recording_declares_what_it_holds() {
    let r = recording();
    assert_eq!(r.describe(), id());
    assert_eq!(r.support(), backend().support());
    let c = r.capabilities();
    assert!(c.contains(&Capability::ArchStream));
    assert!(c.contains(&Capability::EffectStream));
    assert!(c.contains(&Capability::DeterministicReplay));
    assert!(!c.contains(&Capability::OsStream));
    assert!(!c.contains(&Capability::StepBoundary));
    let empty = RecordedBackend::new(
        id(),
        backend().support(),
        &scenario(),
        &profile(),
        BTreeMap::new(),
    );
    assert_eq!(
        empty.capabilities().into_iter().collect::<Vec<_>>(),
        [Capability::DeterministicReplay]
    );
}

#[test]
fn a_recording_agrees_with_its_run_and_locates_as_the_oracle() {
    let plan = the_plan();
    let mut r = recording();
    let mut b = backend();
    let located = locate(
        &plan,
        &scenario(),
        side_of(&mut r, InputVariant::None),
        side(&mut b, InputVariant::None),
        None,
    )
    .unwrap();
    assert_eq!(located.strategy, Strategy::StreamCompare);
    assert!(
        matches!(&located.verdict, Verdict::Compared(c)
            if matches!(c.primary, StreamResult::Agree { compared, .. } if compared == LIMIT)),
        "{:?}",
        located.verdict
    );
    // Every injection the counter takes: the locator's first divergence equals the naive
    // oracle's.
    for at in 1..LIMIT {
        let input = patch_n(at, at, at - 1);
        let mut r = recording();
        let mut b = backend();
        let found = locate(
            &plan,
            &scenario(),
            side_of(&mut r, InputVariant::None),
            side(&mut b, input.clone()),
            None,
        )
        .unwrap();
        let mut r = recording();
        let mut b = backend();
        let naive = verify(
            &profile(),
            &scenario(),
            side_of(&mut r, InputVariant::None),
            side(&mut b, input),
        )
        .unwrap();
        // The locator compares auxiliary streams only up to the primary divergence.
        let primary = |v: &Verdict| match v {
            Verdict::Compared(c) => c.primary.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(primary(&found.verdict), primary(&naive), "{at}");
        assert!(found.divergence().is_some(), "{at}");
    }
}

#[test]
fn a_recording_refuses_another_case() {
    let plan = the_plan();
    let start =
        |r: &mut RecordedBackend,
         input: &InputVariant,
         scenario: Scenario,
         profile: &ComparisonProfile| { r.start(&scenario, input, &config(), profile) };
    let mut r = recording();
    assert!(matches!(
        r.record("arch"),
        Err(EngineError::Internal(m)) if m.contains("before start")
    ));
    assert!(matches!(
        start(&mut r, &patch_n(1, 1, 2), scenario(), &plan.profile),
        Err(EngineError::Unsupported(_))
    ));
    assert!(
        r.record("arch").is_err(),
        "a refused start leaves it unstarted"
    );
    let mut other = scenario();
    other.seed = 1;
    assert!(matches!(
        start(&mut r, &InputVariant::None, other, &plan.profile),
        Err(EngineError::Internal(m)) if m.contains("scenario")
    ));
    // The locator is not identity.
    let mut moved = scenario();
    moved.locator = "elsewhere".to_owned();
    assert_eq!(
        start(&mut r, &InputVariant::None, moved, &plan.profile),
        Ok(())
    );
    let mut newer = plan.profile.clone();
    newer.version = 2;
    assert!(matches!(
        start(&mut r, &InputVariant::None, scenario(), &newer),
        Err(EngineError::Internal(m)) if m.contains("profile counter version 2")
    ));
    start(&mut r, &InputVariant::None, scenario(), &plan.profile).unwrap();
    assert!(matches!(r.next("arch"), Err(EngineError::Unsupported(_))));
    assert!(matches!(
        r.record("os"),
        Err(EngineError::Internal(m)) if m.contains("no recorded os")
    ));
    // Replay is deterministic: every record gives the whole stream again.
    let first = r.record("arch").unwrap();
    assert_eq!(r.record("arch").unwrap(), first);
    assert_eq!(first.len() as u64, LIMIT);
}

#[test]
fn a_recorded_stream_starts_at_ordinal_0() {
    let mut obs = recording();
    obs.start(
        &scenario(),
        &InputVariant::None,
        &config(),
        &the_plan().profile,
    )
    .unwrap();
    let arch = obs.record("arch").unwrap();
    let bytes = stream_bytes(&arch);
    assert_eq!(stream_from_bytes(&bytes).unwrap(), arch);
    // The first ordinal follows the magic and the version.
    let mut later = bytes.clone();
    later[8] = 1;
    assert!(stream_from_bytes(&later).is_err());
    assert!(stream_from_bytes(&bytes[..bytes.len() - 1]).is_err());
}
