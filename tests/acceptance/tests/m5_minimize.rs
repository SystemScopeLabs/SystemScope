//! M5.6b on `m5-reference` (`docs/m5-design.md` §16, §18): minimized perturbations.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::campaign::{Mechanism, Subject, perturbation, record, the_plan};
use systemscope_acceptance::m5::minimize::{Finding, failure, minimize_a, minimize_b};
use systemscope_acceptance::m5::races::Race;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

/// Each race's first diverging seed under A and B (`m5_campaign`).
const FIRST: [(Race, u64, u64); 4] = [
    (Race::R1, 0, 0),
    (Race::R2, 1, 0),
    (Race::R3, 0, 1),
    (Race::R5, 1, 0),
];

/// The offline search the pinned results come from. Run with
/// `cargo nextest run -p systemscope-acceptance --test m5_minimize --run-ignored only`.
#[test]
#[ignore = "offline: hundreds of full runs"]
fn search() {
    let f = fixture();
    let plan = the_plan(&f);
    let only: Option<String> = std::env::var("SS_RACE").ok();
    for (race, a, b) in FIRST {
        if only.as_deref().is_some_and(|o| o != race.name()) {
            continue;
        }
        let subject = Subject(Some(race));
        let base = record(&f, subject, &Perturbation::default());
        for (mechanism, seed) in [(Mechanism::A, a), (Mechanism::B, b)] {
            // R3 under B had not finished after eight hours (`docs/m5-design.md` §27.9).
            if (race, mechanism) == (Race::R3, Mechanism::B) {
                continue;
            }
            let whole = perturbation(mechanism, seed);
            let failure = failure(&plan, &base, &record(&f, subject, &whole)).expect("diverges");
            let finding = Finding {
                fixture: &f,
                plan: &plan,
                subject,
                base: &base,
                failure,
            };
            let min = match mechanism {
                Mechanism::A => minimize_a(&finding, &whole),
                _ => minimize_b(&finding, &whole).unwrap_or_else(|e| panic!("{e}")),
            };
            eprintln!(
                "MIN {} {mechanism:?} seed {seed}: links {:?} slice {:?} only {:?}",
                race.name(),
                min.latencies.links,
                min.slice,
                min.jitter
                    .as_ref()
                    .and_then(|j| j.only.as_deref().map(<[_]>::to_vec)),
            );
        }
    }
}
