//! M5.4a on `m5-reference` (`docs/m5-design.md` §13, §15): the `outcome` and `schedule`
//! streams and the `m5.outcome` and `m5.lockstep` profiles. The unperturbed run agrees
//! with itself under both; under perturbations A and B the schedule changes while
//! `outcome` agrees; and a checkpoint resumes `outcome` from its projection state.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::verify::{
    self, JITTER_LINKS, OUTCOME, Perturbation, SCHEDULE, lockstep_profile, outcome_profile,
};
use systemscope_contracts::trace::Value;
use systemscope_runtime::jitter::{Jitter, LinkSet};
use systemscope_rv32::m5::expected_final;
use systemscope_rv32::m5ref;
use systemscope_rv32::perturb::Latencies;
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{Comparison, Outcome, Recorded, Verdict, compare};
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{ComparisonProfile, Plan, plan};
use systemscope_verify::runtime::RuntimeBackend;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

fn the_plan(profile: &ComparisonProfile, b: &RuntimeBackend) -> Plan {
    plan(profile, &b.support(), &b.support()).unwrap()
}

/// Records every stream of one side under `profile`, which must reach the stop rule.
fn record(fixture: &Fixture, p: &Perturbation, profile: &ComparisonProfile) -> Recorded {
    let mut b = verify::backend(fixture, p);
    let plan = the_plan(profile, &b);
    let side = Side {
        backend: &mut b,
        input: InputVariant::None,
        config: verify::config(),
    };
    run(&plan, &verify::scenario(fixture), side)
        .complete()
        .unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

fn compared(profile: &ComparisonProfile, a: &Recorded, b: &Recorded) -> Comparison {
    let b0 = verify::backend(&fixture(), &Perturbation::default());
    match compare(&the_plan(profile, &b0), a, b).unwrap() {
        Verdict::Compared(c) => c,
        other => panic!("{other:?}"),
    }
}

fn u64_field(o: &Observation, name: &str) -> u64 {
    match o.field(name) {
        Some(ObservedValue::Present(Value::U64(v))) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn kinds<'a>(stream: &'a [Observation], kind: &str) -> Vec<&'a Observation> {
    stream.iter().filter(|o| o.kind == kind).collect()
}

#[test]
fn the_unperturbed_run_agrees_with_itself_under_both_profiles() {
    let fixture = fixture();
    let none = Perturbation::default();
    for profile in [outcome_profile(), lockstep_profile()] {
        let a = record(&fixture, &none, &profile);
        let b = record(&fixture, &none, &profile);
        let c = compared(&profile, &a, &b);
        assert_eq!(c.outcome(), Outcome::Agree, "{}: {c:?}", profile.name);
    }
    let streams = record(&fixture, &none, &outcome_profile());
    let outcome = &streams[0];
    // pid order, every process ending with status 0, then the shutdown's three words.
    let pids: Vec<u64> = outcome
        .iter()
        .filter(|o| o.kind != "final")
        .map(|o| u64_field(o, "pid"))
        .collect();
    assert!(pids.is_sorted(), "{pids:?}");
    let ends = kinds(outcome, "end");
    assert_eq!(ends.len(), 6);
    for e in &ends {
        assert_eq!(
            e.field("status"),
            Some(&ObservedValue::Present(Value::I64(0)))
        );
    }
    let finals: Vec<u64> = kinds(outcome, "final")
        .iter()
        .map(|o| u64_field(o, "value"))
        .collect();
    assert_eq!(finals, expected_final());
    assert_eq!(outcome.last().unwrap().kind, "final");
    // The three printing processes write once each: both readers and `spinner`.
    let out: Vec<u64> = kinds(outcome, "out")
        .iter()
        .map(|o| u64_field(o, "pid"))
        .collect();
    assert_eq!(out, [3, 4, 5]);
    // `schedule` is the order of `arch`, `os`, `schedule`: the third auxiliary stream.
    let schedule = &streams[3];
    assert_eq!(kinds(schedule, "preempt").len(), 97);
    assert_eq!(kinds(schedule, "block").len(), 3);
    assert_eq!(kinds(schedule, "wake").len(), 3);
    assert_eq!(kinds(schedule, "switch").len(), 119);
}

fn jitter(seed: u64, max_cycles: u64) -> Perturbation {
    Perturbation {
        jitter: Some(Jitter {
            seed,
            max_cycles,
            links: LinkSet::of(&JITTER_LINKS),
        }),
        ..Perturbation::default()
    }
}

#[test]
fn perturbed_runs_agree_on_outcome_while_the_schedule_changes() {
    let fixture = fixture();
    let profile = outcome_profile();
    let frozen = record(&fixture, &Perturbation::default(), &profile);
    // A: every link takes 2 cycles. The media's own latency, 3000 cycles, is above
    // what an override may set.
    let links = Perturbation {
        latencies: Latencies {
            links: (0..m5ref::LINKS).map(|i| (i, 2)).collect(),
            ..Latencies::default()
        },
        ..Perturbation::default()
    };
    for p in [jitter(1, 4), jitter(0x5eed, 8), links] {
        let perturbed = record(&fixture, &p, &profile);
        let c = compared(&profile, &frozen, &perturbed);
        assert_eq!(c.outcome(), Outcome::AuxiliaryDivergence, "{p:?}: {c:?}");
        // The auxiliary streams in profile order: `arch`, `os`, `schedule`.
        assert!(!c.auxiliary[2].agrees(), "{p:?}");
    }
}

#[test]
fn a_checkpoint_resumes_outcome_from_its_projection_state() {
    let fixture = fixture();
    let profile = outcome_profile();
    let none = Perturbation::default();
    let whole = record(&fixture, &none, &profile);
    let started = || {
        let mut b = verify::backend(&fixture, &none);
        b.start(
            &verify::scenario(&fixture),
            &InputVariant::None,
            &verify::config(),
            &profile,
        )
        .unwrap();
        b
    };
    // Well into the run: processes are created and hold observations not yet emitted.
    let mut b = started();
    for _ in 0..60 {
        b.next(SCHEDULE).unwrap().unwrap();
    }
    let c = b.checkpoint().unwrap();
    assert!(!c.projection.is_empty());
    let covered = c.mark(OUTCOME).unwrap().covered;
    assert_eq!(covered, 0, "nothing is released before pid 1 ends");
    let mut r = started();
    r.restore(&c).unwrap();
    let mut rest = Vec::new();
    while let Some(o) = r.next(OUTCOME).unwrap() {
        rest.push(o);
    }
    assert_eq!(rest, whole[0][covered as usize..]);
    // Without its projection state the checkpoint does not restore.
    let mut stateless = c.clone();
    stateless.projection.clear();
    let mut r = started();
    assert!(r.restore(&stateless).is_err());
}
