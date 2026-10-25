//! The committed expected streams of `m3-reference` (`docs/m4-design.md` §25.2): this
//! build reproduces them, they agree with a live run through the locator, and a changed
//! or missing file is refused.

use std::fs;
use std::path::PathBuf;

use systemscope_contracts::trace::Value;

use systemscope_acceptance::m3::Fixture;
use systemscope_acceptance::m4::{expected, verify};
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{StreamResult, Verdict};
use systemscope_verify::locate::{Strategy, locate};
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::recorded::{stream_bytes, stream_from_bytes};

fn fixture() -> Fixture {
    Fixture::read(&workspace_root(), Scenario::Reference).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn this_build_reproduces_the_expected_streams() {
    let f = fixture();
    expected::verify(&f, &workspace_root()).unwrap_or_else(|e| panic!("{e:?}"));
    let names: Vec<String> = expected::record(&f)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["arch.obs", "effect.obs", "os.obs"]);
}

fn side(backend: &mut dyn Backend) -> Side<'_> {
    Side {
        backend,
        input: InputVariant::None,
        config: verify::config(),
    }
}

#[test]
fn the_expected_streams_agree_with_a_live_run() {
    let f = fixture();
    let root = workspace_root();
    let plan = expected::the_plan(&f).unwrap();
    let mut recorded = expected::backend(&f, &root).unwrap();
    let mut live = verify::backend(&f);
    let located = locate(
        &plan,
        &verify::scenario(&f),
        side(&mut recorded),
        side(&mut live),
        None,
    )
    .unwrap();
    assert_eq!(located.strategy, Strategy::StreamCompare);
    let Verdict::Compared(c) = &located.verdict else {
        panic!("{:?}", located.verdict)
    };
    // §28.1: 2,200 primary observations.
    assert!(
        matches!(c.primary, StreamResult::Agree { compared: 2200, .. }),
        "{:?}",
        c.primary
    );
    assert!(
        c.auxiliary
            .iter()
            .all(|a| matches!(a, StreamResult::Agree { .. })),
        "{:?}",
        c.auxiliary
    );
}

/// The source-mutant case of `docs/m4-design.md` §25.2: under cargo-mutants this test
/// fails, with [`expected::DETECTED`], for every mutant `m3-reference` observes.
#[test]
fn source_mutant_case() {
    expected::judge(&fixture(), &workspace_root()).unwrap_or_else(|e| panic!("{e}"));
}

/// A copy of the committed streams under a scratch root, with `edit` applied to one.
fn doctored(name: &str, stream: &str, edit: impl Fn(&mut Vec<Observation>)) -> PathBuf {
    let root = workspace_root();
    let scratch = root.join("target/m4-judge-test").join(name);
    let _ = fs::remove_dir_all(&scratch);
    let dir = scratch.join(expected::DIR);
    fs::create_dir_all(&dir).unwrap();
    for file in ["arch.obs", "effect.obs", "os.obs"] {
        let mut bytes = fs::read(root.join(expected::DIR).join(file)).unwrap();
        if file == format!("{stream}.obs") {
            let mut obs = stream_from_bytes(&bytes).unwrap();
            edit(&mut obs);
            bytes = stream_bytes(&obs);
        }
        fs::write(dir.join(file), bytes).unwrap();
    }
    scratch
}

#[test]
fn the_source_mutant_case_names_each_detection() {
    let f = fixture();
    let flip = |o: &mut Observation, name: &str| {
        let (_, v) = o.fields.iter_mut().find(|(n, _)| n == name).unwrap();
        let ObservedValue::Present(Value::U64(x)) = v else {
            panic!("{v:?}")
        };
        *x ^= 1;
    };
    let at = doctored("pc", "arch", |obs| flip(&mut obs[100], "pc"));
    assert_eq!(
        expected::judge(&f, &at),
        Err(format!(
            "{} Diverged at arch[100], retire pc",
            expected::DETECTED
        ))
    );
    let at = doctored("ended", "arch", |obs| {
        obs.pop();
    });
    let e = expected::judge(&f, &at).unwrap_err();
    assert!(
        e.starts_with(&format!(
            "{} Diverged at arch[2199], ended",
            expected::DETECTED
        )),
        "{e}"
    );
    let at = doctored("os", "os", |obs| {
        let o = &mut obs[10];
        let (name, _) = o.fields[0].clone();
        flip(o, &name);
    });
    assert_eq!(
        expected::judge(&f, &at),
        Err(format!(
            "{} AuxiliaryDivergence in os[10]",
            expected::DETECTED
        ))
    );
    let _ = fs::remove_dir_all(workspace_root().join("target/m4-judge-test"));
}

#[test]
fn a_changed_or_missing_stream_is_refused() {
    let f = fixture();
    let root = workspace_root();
    let scratch = root.join("target/m4-expected-test");
    let _ = fs::remove_dir_all(&scratch);
    let dir = scratch.join(expected::DIR);
    fs::create_dir_all(&dir).unwrap();
    for name in ["arch.obs", "effect.obs", "os.obs"] {
        fs::copy(root.join(expected::DIR).join(name), dir.join(name)).unwrap();
    }
    expected::verify(&f, &scratch).unwrap();
    let mut os = fs::read(dir.join("os.obs")).unwrap();
    let last = os.len() - 1;
    os[last] ^= 1;
    fs::write(dir.join("os.obs"), &os).unwrap();
    let errors = expected::verify(&f, &scratch).unwrap_err();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("os.obs differs"), "{errors:?}");
    fs::write(dir.join("os.obs"), &os[..last]).unwrap();
    assert!(expected::backend(&f, &scratch).is_err());
    fs::remove_file(dir.join("effect.obs")).unwrap();
    assert_eq!(expected::verify(&f, &scratch).unwrap_err().len(), 2);
    assert!(
        expected::backend(&f, &scratch)
            .unwrap_err()
            .contains("effect.obs")
    );
    let _ = fs::remove_dir_all(&scratch);
}
