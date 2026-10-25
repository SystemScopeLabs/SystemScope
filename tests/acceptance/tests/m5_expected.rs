//! The committed expectations of `m5-reference` (`docs/m5-design.md` §21): this build
//! reproduces them, the source-mutant case names each detection, and the M5 targets
//! select their functions and run the case.

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use systemscope_contracts::trace::Value;

use systemscope_acceptance::m4::mutants::{Fate, M5_TARGETS, TARGETS, caught, target};
use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::expected::{
    self, ARCH, DETECTED, DIR, RUNS_FILE, first_difference,
};
use systemscope_rv32::workspace_root;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::recorded::{stream_bytes, stream_from_bytes};

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

/// The committed files.
const FILES: [&str; 4] = ["outcome.obs", "os.obs", "schedule.obs", RUNS_FILE];

#[test]
fn this_build_reproduces_the_expectations() {
    expected::verify(&fixture(), &workspace_root()).unwrap_or_else(|e| panic!("{e:?}"));
}

/// The source-mutant case of `docs/m5-design.md` §21: under cargo-mutants this test
/// fails, with [`DETECTED`], for every mutant `m5-reference` observes.
#[test]
fn source_mutant_case() {
    expected::judge(&fixture(), &workspace_root()).unwrap_or_else(|e| panic!("{e}"));
}

/// A copy of the committed files under a scratch root, with `edit` applied to `stream`.
fn doctored(name: &str, stream: &str, edit: impl Fn(&mut Vec<Observation>)) -> PathBuf {
    let root = workspace_root();
    let scratch = root.join("target/m5-judge-test").join(name);
    let _ = fs::remove_dir_all(&scratch);
    let dir = scratch.join(DIR);
    fs::create_dir_all(&dir).unwrap();
    for file in FILES {
        let mut bytes = fs::read(root.join(DIR).join(file)).unwrap();
        if file == format!("{stream}.obs") {
            let mut obs = stream_from_bytes(&bytes).unwrap();
            edit(&mut obs);
            bytes = stream_bytes(&obs);
        }
        fs::write(dir.join(file), bytes).unwrap();
    }
    scratch
}

fn flip(o: &mut Observation) {
    let v = o
        .fields
        .iter_mut()
        .find_map(|(_, v)| match v {
            ObservedValue::Present(Value::U64(x)) => Some(x),
            _ => None,
        })
        .expect("a number");
    *v ^= 1;
}

/// A changed unperturbed stream is found where it changed; the perturbed runs are not
/// reached.
#[test]
fn the_source_mutant_case_names_each_unperturbed_detection() {
    let f = fixture();
    let at = doctored("outcome", "outcome", |obs| flip(&mut obs[3]));
    assert_eq!(
        expected::judge(&f, &at),
        Err(format!("{DETECTED} Diverged at outcome[3]"))
    );
    let at = doctored("os", "os", |obs| flip(&mut obs[10]));
    assert_eq!(
        expected::judge(&f, &at),
        Err(format!("{DETECTED} AuxiliaryDivergence in os[10]"))
    );
    let at = doctored("arch", "outcome", |_| {});
    let path = at.join(DIR).join(RUNS_FILE);
    let mut runs: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    runs["runs"][expected::UNPERTURBED][ARCH] = json!("00");
    fs::write(&path, serde_json::to_string_pretty(&runs).unwrap()).unwrap();
    assert_eq!(
        expected::judge(&f, &at),
        Err(format!("{DETECTED} AuxiliaryDivergence in {ARCH}"))
    );
    let _ = fs::remove_dir_all(workspace_root().join("target/m5-judge-test"));
}

#[test]
fn the_first_difference_names_the_run_and_stream() {
    let text = fs::read_to_string(workspace_root().join(DIR).join(RUNS_FILE)).unwrap();
    let committed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(first_difference(&committed, &committed), None);
    let names: Vec<&String> = committed["runs"].as_object().unwrap().keys().collect();
    assert_eq!(names.len(), 5 + 8 + 7 + 24, "{names:?}");
    let mut changed = committed.clone();
    changed["runs"]["peterson-turn B seed 0"]["schedule"] = json!("00");
    assert_eq!(
        first_difference(&committed, &changed).as_deref(),
        Some("peterson-turn B seed 0 schedule")
    );
    let mut changed = committed.clone();
    changed["runs"]
        .as_object_mut()
        .unwrap()
        .remove("unpatched C seed 7");
    assert_eq!(
        first_difference(&committed, &changed).as_deref(),
        Some("unpatched C seed 7, not run")
    );
    let mut changed = committed.clone();
    changed["tie_order"] = json!("00");
    assert_eq!(
        first_difference(&committed, &changed).as_deref(),
        Some("tie order")
    );
}

#[test]
fn every_m5_target_function_exists_in_its_file() {
    let root = workspace_root();
    for t in &M5_TARGETS {
        let text = fs::read_to_string(root.join(t.file)).unwrap();
        for f in t.functions {
            let name = f.rsplit("::").next().unwrap();
            assert!(text.contains(&format!("fn {name}(")), "{}: {f}", t.file);
        }
        let package = match t.file.split('/').next().unwrap() {
            "runtime" => "runtime",
            _ => t.file.split('/').nth(1).unwrap(),
        };
        assert_eq!(t.package, format!("systemscope-{package}"));
        assert_eq!(target(t.name), Some(t));
        assert_eq!(
            t.test,
            ["--test", "m5_expected", "-E", "test(=source_mutant_case)"]
        );
    }
    let names: std::collections::BTreeSet<_> =
        TARGETS.iter().chain(&M5_TARGETS).map(|t| t.name).collect();
    assert_eq!(names.len(), TARGETS.len() + M5_TARGETS.len());
}

#[test]
fn an_m5_detection_is_read_from_its_log() {
    let log = format!(
        "running 1 test\n    {DETECTED} PerturbedDivergence in counter-unlocked B seed 0 schedule\n"
    );
    assert_eq!(
        caught(&log),
        Fate::Detected("PerturbedDivergence in counter-unlocked B seed 0 schedule".to_owned())
    );
}
