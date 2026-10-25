//! The fault-injection campaign on `m3-reference` (`docs/m4-design.md` §25, §30.7): for
//! every fixed seed, the same-engine checkpoint search and the naive oracle find the same
//! result. The findings are pinned, so the campaign cannot change what a seed means
//! unnoticed. Four tests of eight seeds run in parallel.

use systemscope_acceptance::m3::Fixture;
use systemscope_acceptance::m4::faults;
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;

/// Each fixed seed's finding: `Diverged at arch[d]` or `Agree` for a masked fault.
const FINDINGS: [&str; 32] = [
    "Agree",
    "Agree",
    "Agree",
    "Diverged at arch[629]",
    "Agree",
    "Diverged at arch[1910]",
    "Agree",
    "Agree",
    "Diverged at arch[183]",
    "Diverged at arch[184]",
    "Diverged at arch[1090]",
    "Diverged at arch[998]",
    "Diverged at arch[1817]",
    "Agree",
    "Diverged at arch[2073]",
    "Agree",
    "Agree",
    "Agree",
    "Diverged at arch[2162]",
    "Agree",
    "Diverged at arch[1178]",
    "Agree",
    "Agree",
    "Agree",
    "Agree",
    "Agree",
    "Diverged at arch[1253]",
    "Agree",
    "Agree",
    "Diverged at arch[904]",
    "Diverged at arch[180]",
    "Agree",
];

fn quarter(q: u64) {
    let f = Fixture::read(&workspace_root(), Scenario::Reference).unwrap();
    let seeds = q * 8..(q + 1) * 8;
    assert!(seeds.end <= faults::FIXED_SEEDS.end);
    let found = faults::campaign(&f, seeds.clone()).unwrap_or_else(|e| panic!("{e:?}"));
    for (fault, finding) in &found {
        assert!(
            (1..=31).contains(&fault.register) && fault.bit < 32,
            "{fault:?}"
        );
        assert_eq!(
            finding, FINDINGS[fault.seed as usize],
            "seed {}",
            fault.seed
        );
        // A divergence is never before the fault.
        if let Some(d) = finding.strip_prefix("Diverged at arch[") {
            let d: u64 = d.trim_end_matches(']').parse().unwrap();
            assert!(d >= fault.boundary, "{fault:?} {finding}");
        }
    }
    assert_eq!(
        found.iter().map(|(f, _)| f.seed).collect::<Vec<_>>(),
        seeds.collect::<Vec<_>>()
    );
}

#[test]
fn fixed_fault_seeds_0_to_7() {
    quarter(0);
}

#[test]
fn fixed_fault_seeds_8_to_15() {
    quarter(1);
}

#[test]
fn fixed_fault_seeds_16_to_23() {
    quarter(2);
}

#[test]
fn fixed_fault_seeds_24_to_31() {
    quarter(3);
}

#[test]
fn a_seed_draws_one_fault() {
    let f = Fixture::read(&workspace_root(), Scenario::Reference).unwrap();
    let r = faults::reference(&f).unwrap();
    let a = faults::fault(&r, 3).unwrap();
    assert_eq!(a, faults::fault(&r, 3).unwrap());
    assert_eq!((a.boundary, a.register, a.bit), (625, 12, 8));
    assert_ne!(a, faults::fault(&r, 4).unwrap());
}

/// The nightly seed's fault, from [`faults::SEED_VAR`]: `M4_FAULT_SEED=<seed> cargo nextest
/// run -p systemscope-acceptance --test m4_faults --run-ignored only`.
#[test]
#[ignore = "nightly: needs M4_FAULT_SEED"]
fn nightly_fault_seed() {
    let var = faults::SEED_VAR;
    let seed = std::env::var(var)
        .ok()
        .and_then(|text| systemscope_acceptance::parse_seed(&text))
        .unwrap_or_else(|| panic!("set {var} to the seed to test, e.g. {var}=0x1234"));
    println!("{var}={seed:#x}");
    let f = Fixture::read(&workspace_root(), Scenario::Reference).unwrap();
    let found = faults::campaign(&f, [seed]).unwrap_or_else(|e| panic!("{var}={seed:#x}: {e:?}"));
    println!("{:?}: {}", found[0].0, found[0].1);
}
