//! The reproducer bundle on the counter: canonical writing, byte-identical replay from
//! initial and checkpoint origins, and corruption detection (`docs/m4-design.md` §16).

mod common;

use common::*;
use systemscope_verify::backend::Backend;
use systemscope_verify::bundle::{
    Bundle, Case, MANIFEST, Manifest, RESULT, ReplayError, read_dir, replay, validate, write,
    write_dir,
};
use systemscope_verify::case::{BackendRunConfig, InputVariant, Scenario};
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::compare::Outcome;
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::record_index;
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::report::{from_origins, same_engine};
use systemscope_verify::result::{ResultOutcome, StateEvidence, VerificationResult};

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

/// Each side's checkpoints, one after every primary boundary.
fn checkpoints(inputs: &[InputVariant; 2]) -> [Vec<Checkpoint>; 2] {
    let plan = the_plan();
    [Role::Expected, Role::Actual].map(|role| {
        let input = inputs[role as usize].clone();
        let mut b = backend();
        record_index(&plan, &scenario(), side(&mut b, input), role, 1)
            .unwrap()
            .checkpoints
    })
}

fn covering(checkpoints: &[Checkpoint], covered: u64) -> &Checkpoint {
    checkpoints
        .iter()
        .find(|c| c.mark("arch").unwrap().covered == covered)
        .unwrap()
}

fn bundle(inputs: &[InputVariant; 2], origins: [Option<&Checkpoint>; 2]) -> Bundle {
    write(
        &the_plan(),
        &scenario(),
        &backend,
        inputs.clone(),
        config(),
        origins,
    )
    .unwrap()
}

fn replayed(b: &Bundle) -> Result<VerificationResult, ReplayError> {
    let plan = the_plan();
    let scenario = scenario();
    replay(
        b,
        Case {
            plan: &plan,
            scenario: &scenario,
            config: config(),
            make: &backend,
        },
    )
}

fn corrupt(b: &Bundle) -> bool {
    matches!(
        replayed(b),
        Err(ReplayError::Corrupt(EngineError::CorruptReproducer(_)))
    )
}

/// `b` with `path` replaced by `bytes`, its manifest hash updated to match.
fn rehashed(b: &Bundle, path: &str, bytes: Vec<u8>) -> Bundle {
    let mut m = Manifest::parse(&b.entries[MANIFEST]).unwrap();
    for (p, h) in &mut m.entries {
        if p == path {
            *h = *blake3::hash(&bytes).as_bytes();
        }
    }
    let mut b = b.clone();
    b.entries.insert(path.to_owned(), bytes);
    b.entries
        .insert(MANIFEST.to_owned(), m.render().into_bytes());
    b
}

const DIVERGED: u64 = 8;

fn diverging() -> [InputVariant; 2] {
    [
        InputVariant::None,
        patch_n(DIVERGED, DIVERGED, DIVERGED + 1),
    ]
}

#[test]
fn every_origin_finds_what_same_engine_finds() {
    let inputs = diverging();
    let found = same_engine(
        &the_plan(),
        &scenario(),
        &backend,
        inputs.clone(),
        config(),
        2,
    );
    assert_eq!(found.outcome(), ResultOutcome::Compared(Outcome::Diverged));
    let ks = checkpoints(&inputs);
    // The report window is 3: a checkpoint covers at most 8 - 3 boundaries.
    for covered in [None, Some(1), Some(4), Some(5)] {
        let at = covered.map(|n| [covering(&ks[0], n), covering(&ks[1], n)]);
        for origins in match at {
            None => vec![[None, None]],
            Some([e, a]) => vec![[Some(e), Some(a)], [Some(e), None], [None, Some(a)]],
        } {
            let r = from_origins(
                &the_plan(),
                &scenario(),
                &backend,
                inputs.clone(),
                config(),
                origins,
            );
            // The scans' own origins may differ; what they find does not.
            let at = format!("{covered:?} {origins:?}");
            assert_eq!(
                (&r.verdict, &r.window),
                (&found.verdict, &found.window),
                "{at}"
            );
            let (s, f) = (r.state.as_ref().unwrap(), found.state.as_ref().unwrap());
            assert_eq!(
                (s.read_at, &s.differences),
                (f.read_at, &f.differences),
                "{at}"
            );
            let first = |s: &StateEvidence| s.first.clone().map(|d| (d.event, d.differing));
            assert_eq!(first(s), first(f), "{at}");
            let event = |r: &VerificationResult| r.trace.canonical.as_ref().map(|t| t.event);
            assert_eq!(event(&r), event(&found), "{at}");
        }
    }
}

#[test]
fn an_origin_past_the_window_or_an_agreement_from_a_checkpoint_is_an_error() {
    let inputs = diverging();
    let ks = checkpoints(&inputs);
    let past = [Some(covering(&ks[0], 6)), Some(covering(&ks[1], 6))];
    let r = from_origins(&the_plan(), &scenario(), &backend, inputs, config(), past);
    assert!(matches!(r.verdict, Err(EngineError::Internal(_))), "{r:?}");
    let same = [InputVariant::None, InputVariant::None];
    let ks = checkpoints(&same);
    let at = [Some(covering(&ks[0], 2)), Some(covering(&ks[1], 2))];
    let r = from_origins(&the_plan(), &scenario(), &backend, same, config(), at);
    assert!(matches!(r.verdict, Err(EngineError::Internal(_))), "{r:?}");
    // Checkpoints that cover different boundaries, or one the other side's start does not
    // reproduce, are a desync.
    let ks = checkpoints(&diverging());
    for origins in [
        [Some(covering(&ks[0], 1)), Some(covering(&ks[1], 2))],
        [
            None,
            Some(&Checkpoint {
                streams: vec![],
                ..covering(&ks[1], 2).clone()
            }),
        ],
    ] {
        let r = from_origins(
            &the_plan(),
            &scenario(),
            &backend,
            diverging(),
            config(),
            origins,
        );
        assert!(
            matches!(r.verdict, Err(EngineError::BoundaryDesync(_))),
            "{r:?}"
        );
    }
}

#[test]
fn a_diverged_bundle_holds_its_material_and_replays_byte_identically() {
    let inputs = diverging();
    let ks = checkpoints(&inputs);
    let origins = [Some(covering(&ks[0], 5)), Some(covering(&ks[1], 5))];
    let b = bundle(&inputs, origins);
    let paths: Vec<&str> = b.entries.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "actual.obs",
            "actual/checkpoint.bin",
            "actual/input.bin",
            "expected.obs",
            "expected/checkpoint.bin",
            MANIFEST,
            RESULT,
        ]
    );
    assert_eq!(bundle(&inputs, origins), b);
    let r = replayed(&b).unwrap();
    assert_eq!(r.to_bytes(), b.entries[RESULT]);
    let text = String::from_utf8(b.entries[MANIFEST].clone()).unwrap();
    assert!(!text.contains('\r'));
    for line in [
        "  \"format\": \"systemscope-reproducer\",\n",
        "      \"role\": \"actual\",\n",
        "      \"origin\": {\"checkpoint\": 5}\n",
        "  \"outcome\": \"diverged\",\n",
        "    \"ordinal\": 8,\n",
        "    \"kind\": \"fields\"\n",
    ] {
        assert!(text.contains(line), "{line:?} in\n{text}");
    }
    let m = Manifest::parse(text.as_bytes()).unwrap();
    assert_eq!(m.sides.map(|s| s.origin), [Some(5), Some(5)]);
    // An initial origin holds no checkpoint.
    let b = bundle(&inputs, [None, origins[1]]);
    assert!(!b.entries.contains_key("expected/checkpoint.bin"));
    assert!(
        b.entries[MANIFEST]
            .windows(19)
            .any(|w| w == b"\"origin\": \"initial\"")
    );
    let one = replayed(&b).unwrap();
    assert_eq!((&one.verdict, &one.window), (&r.verdict, &r.window));
}

#[test]
fn an_agreeing_bundle_holds_no_observations() {
    let same = [InputVariant::None, InputVariant::None];
    let b = bundle(&same, [None, None]);
    let paths: Vec<&str> = b.entries.keys().map(String::as_str).collect();
    assert_eq!(paths, [MANIFEST, RESULT]);
    let r = replayed(&b).unwrap();
    assert_eq!(r.outcome(), ResultOutcome::Compared(Outcome::Agree));
    let m = Manifest::parse(&b.entries[MANIFEST]).unwrap();
    assert_eq!((m.outcome, m.primary_divergence), (Outcome::Agree, None));
}

#[test]
fn only_a_compared_outcome_is_a_reproducer() {
    let failing = [InputVariant::None, patch_n(4, 7, 5)];
    let w = write(
        &the_plan(),
        &scenario(),
        &backend,
        failing,
        config(),
        [None, None],
    );
    assert!(matches!(w, Err(EngineError::BackendFailed(_))), "{w:?}");
}

#[test]
fn a_bundle_survives_a_directory() {
    let inputs = diverging();
    let ks = checkpoints(&inputs);
    let b = bundle(&inputs, [Some(covering(&ks[0], 3)), None]);
    let dir = std::env::temp_dir().join(format!("ss-bundle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_dir(&b, &dir).unwrap();
    assert!(
        write_dir(&b, &dir).is_err(),
        "never into an existing directory"
    );
    let read = read_dir(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(read.unwrap(), b);
}

fn a_bundle() -> Bundle {
    let inputs = diverging();
    let ks = checkpoints(&inputs);
    bundle(
        &inputs,
        [Some(covering(&ks[0], 4)), Some(covering(&ks[1], 4))],
    )
}

#[test]
fn every_manifest_byte_is_checked() {
    let b = a_bundle();
    assert!(replayed(&b).is_ok());
    let manifest = &b.entries[MANIFEST];
    for i in 0..manifest.len() {
        let mut m = manifest.clone();
        m[i] ^= 1;
        let mut c = b.clone();
        c.entries.insert(MANIFEST.to_owned(), m);
        assert!(corrupt(&c), "byte {i}: {:?}", manifest[i] as char);
    }
    let mut c = b.clone();
    c.entries.remove(MANIFEST);
    assert!(corrupt(&c));
}

#[test]
fn every_entry_byte_is_checked() {
    let b = a_bundle();
    for (path, bytes) in &b.entries {
        if path == MANIFEST {
            continue;
        }
        let mut missing = b.clone();
        missing.entries.remove(path);
        assert!(corrupt(&missing), "{path} missing");
        let short = rehashed(&b, path, bytes[..bytes.len() - 1].to_vec());
        assert!(replayed(&short).is_err(), "{path} truncated");
        for i in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[i] ^= 1;
            let mut c = b.clone();
            c.entries.insert(path.clone(), flipped.clone());
            assert!(corrupt(&c), "{path} byte {i} without its hash");
            let c = rehashed(&b, path, flipped);
            assert!(replayed(&c).is_err(), "{path} byte {i}");
        }
    }
    let mut extra = b.clone();
    extra.entries.insert("extra.bin".to_owned(), vec![0]);
    assert!(corrupt(&extra));
}

#[test]
fn checkpoints_must_belong_to_their_side_and_restore() {
    let b = a_bundle();
    let ks = checkpoints(&diverging());
    // The actual side's checkpoint at the expected side's origin: another execution.
    let swapped = rehashed(
        &b,
        "expected/checkpoint.bin",
        covering(&ks[1], 4).to_bytes(),
    );
    assert!(corrupt(&swapped));
    let mut broken = covering(&ks[0], 4).clone();
    broken.state.truncate(broken.state.len() / 2);
    let c = rehashed(&b, "expected/checkpoint.bin", broken.to_bytes());
    assert!(corrupt(&c));
    // Each check refuses before anything runs.
    let plan = the_plan();
    let scenario = scenario();
    let case = |scenario: &Scenario, config: BackendRunConfig| {
        validate(
            &b,
            Case {
                plan: &plan,
                scenario,
                config,
                make: &backend,
            },
        )
    };
    assert!(case(&scenario, config()).is_ok());
    let other = Scenario {
        seed: 1,
        ..scenario.clone()
    };
    assert!(case(&other, config()).is_err());
    assert!(case(&scenario, BackendRunConfig { budget: 7 }).is_err());
}
