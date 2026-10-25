//! M5.3b: `m5-reference`'s golden files and its every-event resume
//! (`docs/m5-design.md` §12, §19). The exhaustive resume of every checkpoint is the
//! `m5-every-event` CI job, `cargo xtask m5-golden every-event shard <i> <n>`, and on the
//! perturbed platforms the weekly nightly, `cargo xtask m5-golden every-event perturbed
//! <a|b|c>`.

use std::fs;

use systemscope_acceptance::m5::campaign::{Mechanism, perturbation};
use systemscope_acceptance::m5::verify::defective_config;
use systemscope_acceptance::m5::{
    Fixture, GOLDEN_PATH, Golden, MID_SNAPSHOT_PATH, events_under, every_event, every_event_under,
    reference_events,
};
use systemscope_acceptance::sweep::threads;
use systemscope_rv32::m5ref::Config;
use systemscope_rv32::workspace_root;

fn committed() -> (String, Vec<u8>) {
    let root = workspace_root();
    (
        fs::read_to_string(root.join(GOLDEN_PATH)).unwrap(),
        fs::read(root.join(MID_SNAPSHOT_PATH)).unwrap(),
    )
}

#[test]
fn the_workload_reproduces_the_committed_golden_files() {
    let (json, snapshot) = committed();
    Golden::verify(&workspace_root(), &json, &snapshot).unwrap_or_else(|e| panic!("{e:#?}"));
}

#[test]
fn a_doctored_golden_file_or_snapshot_is_refused() {
    let root = workspace_root();
    let (json, snapshot) = committed();
    let doctored = json.replace("\"max_blocked\": 2", "\"max_blocked\": 3");
    assert_ne!(doctored, json);
    let errors = Golden::verify(&root, &doctored, &snapshot).unwrap_err();
    assert!(
        errors.iter().any(|e| e == "~ max_blocked: 3 -> 2"),
        "{errors:?}"
    );
    let mut flipped = snapshot.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    let errors = Golden::verify(&root, &json, &flipped).unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("differs from this run's")),
        "{errors:?}"
    );
}

#[test]
fn checkpoints_around_the_mid_snapshot_and_the_end_resume_exactly() {
    let fixture = Fixture::read(&workspace_root()).unwrap();
    let events = reference_events(&fixture).unwrap();
    let (json, _) = committed();
    let golden: serde_json::Value = serde_json::from_str(&json).unwrap();
    let mid = usize::try_from(golden["mid_event"].as_u64().unwrap()).unwrap();
    let threads = threads();
    for range in [mid - 500..mid + 500, events - 500..events + 1] {
        let sweep = every_event(&fixture, range.clone(), threads);
        assert_eq!(sweep.checked, range.len());
        assert!(sweep.failures.is_empty(), "{:?}", sweep.failures.first());
    }
}

/// The frozen platform's events counted by stepping are the reference run's.
#[test]
fn stepping_the_frozen_platform_counts_the_reference_events() {
    let fixture = Fixture::read(&workspace_root()).unwrap();
    assert_eq!(
        events_under(&fixture, &Config::frozen()).unwrap(),
        reference_events(&fixture).unwrap()
    );
}

/// Under A's and B's seed 0, the first and last checkpoints resume exactly: the perturbed
/// latencies and the jitter are part of each checkpoint (§19).
#[test]
fn perturbed_checkpoints_resume_exactly() {
    let fixture = Fixture::read(&workspace_root()).unwrap();
    let threads = threads();
    for mechanism in [Mechanism::A, Mechanism::B] {
        let config = defective_config(&perturbation(mechanism, 0), None);
        let events = events_under(&fixture, &config).unwrap();
        for range in [0..500, events - 500..events + 1] {
            let sweep = every_event_under(&fixture, &config, range.clone(), threads);
            assert_eq!(sweep.checked, range.len(), "{mechanism:?}");
            assert!(
                sweep.failures.is_empty(),
                "{mechanism:?}: {:?}",
                sweep.failures.first()
            );
        }
    }
}
