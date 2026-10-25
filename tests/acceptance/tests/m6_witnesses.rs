//! M6.2's witnesses on `m5-reference` (`docs/m6-design.md` §10): R6 and R7 each patch one
//! range of a program, and each diverges under its hand-authored script of timer shifts at
//! its pinned `outcome` ordinal. The unpatched scenario agrees under every witness, and
//! R7's diverges only with both of its shifts.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::campaign::{Judged, Subject, judge, record, the_plan};
use systemscope_acceptance::m5::races::Race;
use systemscope_runtime::script::Script;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;
use systemscope_verify::case::InputVariant;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

fn scripted(script: Script) -> Perturbation {
    Perturbation {
        script: Some(script),
        ..Perturbation::default()
    }
}

#[test]
fn every_narrow_race_patches_one_range_of_its_own_bytes() {
    let f = fixture();
    for race in Race::NARROW {
        let Ok(InputVariant::Media(p)) = race.variant(&f.disk) else {
            panic!("{}: no media patch", race.name());
        };
        let at = usize::try_from(p.offset).expect("offset");
        assert_eq!(p.old.len(), 8, "{}: two instructions", race.name());
        assert_eq!(f.disk[at..at + 8], p.old[..], "{}", race.name());
        assert_eq!(p.new[..4], p.old[4..], "{}: they trade places", race.name());
        assert_eq!(p.new[4..], p.old[..4], "{}: they trade places", race.name());
        assert_eq!(race.defect(), None, "{}", race.name());
    }
    for race in Race::ALL {
        assert!(race.witness().is_none(), "{}", race.name());
    }
}

/// `race` diverges under its witness at the pinned ordinal, and the unpatched scenario
/// agrees under it.
fn diverges_under_its_witness(race: Race) {
    let (script, ordinal) = race.witness().expect("a witness");
    let f = fixture();
    let plan = the_plan(&f);
    for (subject, expected) in [
        (Subject(Some(race)), Judged::Diverged(ordinal)),
        (Subject(None), Judged::Agree),
    ] {
        let base = record(&f, subject, &Perturbation::default());
        assert_eq!(base.failure, None, "{}", subject.name());
        let judged = judge(
            &plan,
            &base,
            &record(&f, subject, &scripted(script.clone())),
        );
        assert_eq!(
            judged,
            expected,
            "{} under {}'s witness",
            subject.name(),
            race.name()
        );
    }
}

#[test]
fn handoff_flag_first_diverges_under_its_witness() {
    diverges_under_its_witness(Race::R6);
}

#[test]
fn peterson_order_diverges_under_its_witness() {
    diverges_under_its_witness(Race::R7);
}

#[test]
fn peterson_order_needs_both_shifts_of_its_witness() {
    let (script, _) = Race::R7.witness().expect("a witness");
    assert_eq!(script.shifts.len(), 2);
    let f = fixture();
    let plan = the_plan(&f);
    let subject = Subject(Some(Race::R7));
    let base = record(&f, subject, &Perturbation::default());
    for shift in &script.shifts {
        let alone = Script {
            shifts: vec![*shift],
            ..Script::default()
        };
        let judged = judge(&plan, &base, &record(&f, subject, &scripted(alone)));
        assert_eq!(judged, Judged::Agree, "{shift:?} alone");
    }
}
