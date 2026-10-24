//! M5.6b on `m5-reference` (`docs/m5-design.md` §16, §18): each finding's minimized
//! perturbation, pinned from the offline search (`m5_minimize`), still fails the way the
//! campaign's whole perturbation does, loses the failure without any one of its parts,
//! and makes a reproducer that replays byte for byte from its `perturbation.bin`.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::campaign::{Mechanism, Subject, perturbation, record, the_plan};
use systemscope_acceptance::m5::minimize::{
    Finding, PINNED, Pinned, failure, listed, parts, replay, reproducer, without,
};
use systemscope_acceptance::m5::races::Race;
use systemscope_acceptance::m5::repro;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;
use systemscope_verify::bundle::{MANIFEST, Manifest, RESULT};
use systemscope_verify::compare::Outcome;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

/// The minimized perturbation without each of its parts, one at a time.
fn reduced(pinned: &Pinned) -> Vec<Perturbation> {
    let p = pinned.perturbation();
    match pinned.mechanism {
        Mechanism::B => (0..pinned.only.len())
            .map(|i| {
                let mut only = pinned.only.to_vec();
                only.remove(i);
                listed(&p, &only)
            })
            .collect(),
        _ => parts(&p)
            .into_iter()
            .map(|part| without(&p, part))
            .collect(),
    }
}

/// The pinned perturbation of `race` under `mechanism`.
fn pinned(race: Race, mechanism: Mechanism) -> &'static Pinned {
    PINNED
        .iter()
        .find(|p| (p.race, p.mechanism) == (race, mechanism))
        .expect("pinned")
}

fn check(pinned: &Pinned) {
    let f = fixture();
    let plan = the_plan(&f);
    let subject = Subject(Some(pinned.race));
    let name = format!("{} {:?}", pinned.race.name(), pinned.mechanism);
    let base = record(&f, subject, &Perturbation::default());
    let whole = record(&f, subject, &perturbation(pinned.mechanism, pinned.seed));
    let finding = Finding {
        fixture: &f,
        plan: &plan,
        subject,
        base: &base,
        failure: failure(&plan, &base, &whole).unwrap_or_else(|| panic!("{name}: diverges")),
    };
    let minimized = pinned.perturbation();
    assert!(finding.reproduces(&minimized), "{name}: the same failure");
    let reduced = reduced(pinned);
    assert!(!reduced.is_empty(), "{name}");
    for (i, q) in reduced.iter().enumerate() {
        assert!(!finding.reproduces(q), "{name}: part {i} is needed");
    }

    let b = reproducer(&f, subject, [&Perturbation::default(), &minimized])
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(b.entries["actual/perturbation.bin"], minimized.to_bytes());
    assert!(!b.entries.contains_key("expected/perturbation.bin"));
    let m = Manifest::parse(&b.entries[MANIFEST]).unwrap();
    assert_eq!(m.outcome, Outcome::Diverged, "{name}");
    assert!(
        m.entries
            .iter()
            .any(|(p, _)| p == "actual/perturbation.bin"),
        "{name}: the manifest lists it"
    );
    let r = replay(&f, subject, &b).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_eq!(r.to_bytes(), b.entries[RESULT], "{name}");
}

#[test]
fn counter_unlocked_under_a() {
    check(pinned(Race::R1, Mechanism::A));
}

#[test]
fn counter_unlocked_under_b() {
    check(pinned(Race::R1, Mechanism::B));
}

#[test]
fn peterson_turn_under_a() {
    check(pinned(Race::R2, Mechanism::A));
}

#[test]
fn peterson_turn_under_b() {
    check(pinned(Race::R2, Mechanism::B));
}

/// R3 under B is not pinned (§27.9).
#[test]
fn mailbox_data_first_under_a() {
    check(pinned(Race::R3, Mechanism::A));
}

#[test]
fn kernel_wake_lost_under_a() {
    check(pinned(Race::R5, Mechanism::A));
}

#[test]
fn kernel_wake_lost_under_b() {
    check(pinned(Race::R5, Mechanism::B));
}

/// Each pinned reproducer has its own bundle name, the race's and the mechanism's.
#[test]
fn every_pinned_reproducer_has_its_own_bundle_name() {
    let names: Vec<String> = PINNED.iter().map(repro::name).collect();
    assert_eq!(names[0], "counter-unlocked-a");
    assert_eq!(names[1], "counter-unlocked-b");
    let distinct: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(distinct.len(), PINNED.len());
}

/// Replay refuses an empty directory and a bundle named for no pinned perturbation.
#[test]
fn replay_refuses_an_empty_directory_and_an_unknown_name() {
    let f = fixture();
    let dir = std::env::temp_dir().join(format!("m5-repro-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("nobody-a.repro")).unwrap();
    let errors = repro::replay_all(&f, &dir).unwrap_err();
    assert!(
        errors[0].contains("no pinned perturbation is named nobody-a"),
        "{errors:?}"
    );
    std::fs::remove_dir_all(dir.join("nobody-a.repro")).unwrap();
    let errors = repro::replay_all(&f, &dir).unwrap_err();
    assert!(errors[0].starts_with("no .repro bundle"), "{errors:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
