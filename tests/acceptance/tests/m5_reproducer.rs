//! M5.6b on `m5-reference` (`docs/m5-design.md` §16, §18): each finding's minimized
//! perturbation, pinned from the offline search (`m5_minimize`), still fails the way the
//! campaign's whole perturbation does, loses the failure without any one of its parts,
//! and makes a reproducer that replays byte for byte from its `perturbation.bin`.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::campaign::{Mechanism, Subject, perturbation, record, the_plan};
use systemscope_acceptance::m5::minimize::{
    Finding, failure, listed, parts, replay, reproducer, without,
};
use systemscope_acceptance::m5::races::Race;
use systemscope_runtime::jitter::MessageId;
use systemscope_rv32::perturb::{Latencies, Perturbation};
use systemscope_rv32::workspace_root;
use systemscope_verify::bundle::{MANIFEST, Manifest, RESULT};
use systemscope_verify::compare::Outcome;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

/// A minimized perturbation: the campaign seed it was reduced from, and what is left.
struct Pinned {
    race: Race,
    mechanism: Mechanism,
    seed: u64,
    /// A: the link overrides left, in CPU cycles.
    links: &'static [(u32, u64)],
    /// A: the slice left.
    slice: Option<u32>,
    /// B: the delayed messages left.
    only: &'static [MessageId],
}

impl Pinned {
    fn perturbation(&self) -> Perturbation {
        match self.mechanism {
            Mechanism::A => Perturbation {
                latencies: Latencies {
                    links: self.links.iter().copied().collect(),
                    ..Latencies::default()
                },
                slice: self.slice,
                ..Perturbation::default()
            },
            Mechanism::B => listed(&perturbation(Mechanism::B, self.seed), self.only),
            Mechanism::C => unreachable!("C has no finding"),
        }
    }

    /// The minimized perturbation without each of its parts, one at a time.
    fn reduced(&self) -> Vec<Perturbation> {
        let p = self.perturbation();
        match self.mechanism {
            Mechanism::B => (0..self.only.len())
                .map(|i| {
                    let mut only = self.only.to_vec();
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
    let reduced = pinned.reduced();
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
    check(&Pinned {
        race: Race::R1,
        mechanism: Mechanism::A,
        seed: 0,
        links: &[(12, 2)],
        slice: Some(8002),
        only: &[],
    });
}

#[test]
fn counter_unlocked_under_b() {
    check(&Pinned {
        race: Race::R1,
        mechanism: Mechanism::B,
        seed: 0,
        links: &[],
        slice: None,
        only: &[(3, 0, 22), (3, 0, 25), (3, 1, 14), (3, 1, 25), (3, 1, 26)],
    });
}

#[test]
fn peterson_turn_under_a() {
    check(&Pinned {
        race: Race::R2,
        mechanism: Mechanism::A,
        seed: 1,
        links: &[(4, 4), (11, 10)],
        slice: Some(5250),
        only: &[],
    });
}

/// Peterson's turn race needs many delays at once: 42 messages on four links.
#[rustfmt::skip]
#[test]
fn peterson_turn_under_b() {
    check(&Pinned {
        race: Race::R2,
        mechanism: Mechanism::B,
        seed: 0,
        links: &[],
        slice: None,
        only: &[
            (3, 0, 21), (3, 0, 22), (3, 0, 27), (3, 0, 28), (3, 0, 36), (3, 0, 37), (3, 0, 42),
            (3, 1, 21), (3, 1, 22), (3, 1, 27), (3, 1, 28), (3, 1, 36), (3, 1, 41), (3, 1, 42),
            (8, 0, 42), (8, 0, 52), (8, 0, 54),
            (11, 0, 40), (11, 0, 42), (11, 0, 43), (11, 0, 51), (11, 0, 52), (11, 0, 53),
            (11, 0, 54), (11, 0, 82), (11, 0, 85),
            (11, 1, 40), (11, 1, 41), (11, 1, 42), (11, 1, 43), (11, 1, 51), (11, 1, 52),
            (11, 1, 53), (11, 1, 70), (11, 1, 71), (11, 1, 78), (11, 1, 82),
            (12, 0, 40), (12, 0, 50), (12, 0, 52), (12, 0, 70), (12, 0, 78),
        ],
    });
}

/// R3 under B is not pinned: its search had not finished after eight hours (§27.9).
#[test]
fn mailbox_data_first_under_a() {
    check(&Pinned {
        race: Race::R3,
        mechanism: Mechanism::A,
        seed: 0,
        links: &[(11, 15)],
        slice: None,
        only: &[],
    });
}

#[test]
fn kernel_wake_lost_under_a() {
    check(&Pinned {
        race: Race::R5,
        mechanism: Mechanism::A,
        seed: 1,
        links: &[],
        slice: Some(7995),
        only: &[],
    });
}

#[test]
fn kernel_wake_lost_under_b() {
    check(&Pinned {
        race: Race::R5,
        mechanism: Mechanism::B,
        seed: 0,
        links: &[],
        slice: None,
        only: &[(3, 1, 21)],
    });
}
