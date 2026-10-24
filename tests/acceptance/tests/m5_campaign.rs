//! M5.5 and M5.6a on `m5-reference` (`docs/m5-design.md` §12.3, §14): planted races and
//! campaigns. The unpatched scenario agrees under `m5.outcome` for every CI seed of
//! perturbations A, B, and C, and every planted race diverges for a pinned seed of A and
//! of B, with every earlier seed agreeing. C reorders simultaneous events, which no model
//! depends on: every stream of every subject is unchanged under it.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::campaign::{
    CI_SEEDS, FIRST_DIVERGENCE, Judged, Mechanism, Subject, judge, perturbation, record, the_plan,
};
use systemscope_acceptance::m5::races::Race;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{Verdict, compare_recordings};

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn every_race_patches_one_range_of_its_own_bytes_or_plants_a_kernel_defect() {
    let f = fixture();
    for race in Race::ALL {
        let variant = race.variant(&f.disk).unwrap_or_else(|e| panic!("{e}"));
        match (variant, race.defect()) {
            (InputVariant::Media(p), None) => {
                let at = usize::try_from(p.offset).expect("offset");
                assert_eq!(p.old.len(), p.new.len(), "{}", race.name());
                assert_eq!(f.disk[at..at + p.old.len()], p.old[..], "{}", race.name());
                assert_ne!(p.old, p.new, "{}", race.name());
                assert_eq!(p.old.len() % 4, 0, "{}: whole instructions", race.name());
            }
            (InputVariant::None, Some(_)) => assert_eq!(race, Race::R5),
            (v, d) => panic!("{}: {v:?} with {d:?}", race.name()),
        }
    }
}

fn unpatched_agrees(mechanism: Mechanism) {
    let f = fixture();
    let plan = the_plan(&f);
    let base = record(&f, Subject(None), &Perturbation::default());
    assert_eq!(base.failure, None);
    for seed in CI_SEEDS {
        let perturbed = record(&f, Subject(None), &perturbation(mechanism, seed));
        assert_eq!(
            judge(&plan, &base, &perturbed),
            Judged::Agree,
            "{mechanism:?} seed {seed}"
        );
    }
}

#[test]
fn the_unpatched_scenario_agrees_under_a() {
    unpatched_agrees(Mechanism::A);
}

#[test]
fn the_unpatched_scenario_agrees_under_b() {
    unpatched_agrees(Mechanism::B);
}

#[test]
fn the_unpatched_scenario_agrees_under_c() {
    unpatched_agrees(Mechanism::C);
}

/// Under C, the auxiliary `os`, `arch`, and `schedule` streams agree too, for the
/// unpatched scenario and every race: the tie order reaches the scheduler, but no
/// component's behavior depends on the order of events with equal `(tick, phase)`.
#[test]
fn tie_order_changes_no_stream_of_any_subject() {
    let f = fixture();
    let plan = the_plan(&f);
    let subjects = std::iter::once(Subject(None)).chain(Race::ALL.map(|r| Subject(Some(r))));
    for subject in subjects {
        let base = record(&f, subject, &Perturbation::default());
        let tied = record(&f, subject, &perturbation(Mechanism::C, 0));
        match compare_recordings(&plan, &base, &tied) {
            Ok(Verdict::Compared(c)) => {
                assert!(c.primary.agrees(), "{}", subject.name());
                assert!(c.failures.is_empty(), "{}", subject.name());
                assert_eq!(c.auxiliary.len(), 3, "{}", subject.name());
                assert!(
                    c.auxiliary.iter().all(|a| a.agrees()),
                    "{}: {:?}",
                    subject.name(),
                    c.auxiliary
                );
            }
            other => panic!("{}: {other:?}", subject.name()),
        }
    }
}

/// Runs `race`'s campaign under `mechanism` up to its first divergence, which must come
/// at its [`FIRST_DIVERGENCE`] seed with every earlier seed agreeing.
fn diverges_first(race: Race, mechanism: Mechanism) {
    let (_, _, first) = FIRST_DIVERGENCE
        .into_iter()
        .find(|&(r, m, _)| (r, m) == (race, mechanism))
        .expect("a pinned seed");
    let f = fixture();
    let plan = the_plan(&f);
    let subject = Subject(Some(race));
    let base = record(&f, subject, &Perturbation::default());
    assert_eq!(base.failure, None, "{}", race.name());
    for seed in 0..=first {
        let judged = judge(
            &plan,
            &base,
            &record(&f, subject, &perturbation(mechanism, seed)),
        );
        match judged {
            Judged::Agree if seed < first => {}
            Judged::Diverged(_) if seed == first => {}
            j => panic!("{} {mechanism:?} seed {seed}: {j:?}", race.name()),
        }
    }
}

#[test]
fn counter_unlocked_diverges_under_a() {
    diverges_first(Race::R1, Mechanism::A);
}

#[test]
fn counter_unlocked_diverges_under_b() {
    diverges_first(Race::R1, Mechanism::B);
}

#[test]
fn peterson_turn_diverges_under_a() {
    diverges_first(Race::R2, Mechanism::A);
}

#[test]
fn peterson_turn_diverges_under_b() {
    diverges_first(Race::R2, Mechanism::B);
}

#[test]
fn mailbox_data_first_diverges_under_a() {
    diverges_first(Race::R3, Mechanism::A);
}

#[test]
fn mailbox_data_first_diverges_under_b() {
    diverges_first(Race::R3, Mechanism::B);
}

#[test]
fn kernel_wake_lost_diverges_under_a() {
    diverges_first(Race::R5, Mechanism::A);
}

#[test]
fn kernel_wake_lost_diverges_under_b() {
    diverges_first(Race::R5, Mechanism::B);
}
