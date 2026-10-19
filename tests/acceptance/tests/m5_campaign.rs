//! M5.5 on `m5-reference` (`docs/m5-design.md` §12.3, §14): planted races and
//! campaigns. The unpatched scenario agrees under `m5.outcome` for every CI seed of
//! perturbations A and B, and every planted race diverges for a pinned seed of each, with
//! every earlier seed agreeing.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::campaign::{
    Judged, Mechanism, Subject, judge, perturbation, record, the_plan,
};
use systemscope_acceptance::m5::races::Race;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;
use systemscope_verify::case::InputVariant;

/// The seeds CI runs the unpatched campaign over, for each mechanism.
const CI_SEEDS: std::ops::Range<u64> = 0..8;

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

/// Runs `race`'s campaign under `mechanism` up to its first divergence, which must come
/// at `first` with every earlier seed agreeing.
fn diverges_first_at(race: Race, mechanism: Mechanism, first: u64) {
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
    diverges_first_at(Race::R1, Mechanism::A, 0);
}

#[test]
fn counter_unlocked_diverges_under_b() {
    diverges_first_at(Race::R1, Mechanism::B, 0);
}

#[test]
fn peterson_turn_diverges_under_a() {
    diverges_first_at(Race::R2, Mechanism::A, 1);
}

#[test]
fn peterson_turn_diverges_under_b() {
    diverges_first_at(Race::R2, Mechanism::B, 0);
}

#[test]
fn mailbox_data_first_diverges_under_a() {
    diverges_first_at(Race::R3, Mechanism::A, 0);
}

#[test]
fn mailbox_data_first_diverges_under_b() {
    diverges_first_at(Race::R3, Mechanism::B, 1);
}

#[test]
fn kernel_wake_lost_diverges_under_a() {
    diverges_first_at(Race::R5, Mechanism::A, 1);
}

#[test]
fn kernel_wake_lost_diverges_under_b() {
    diverges_first_at(Race::R5, Mechanism::B, 0);
}
