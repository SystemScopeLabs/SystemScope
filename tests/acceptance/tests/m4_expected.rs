//! The committed expected streams of `m3-reference` (`docs/m4-design.md` §25.2): this
//! build reproduces them, they agree with a live run through the locator, and a changed
//! or missing file is refused.

use std::fs;

use systemscope_acceptance::m3::{Fixture, expected, verify};
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{StreamResult, Verdict};
use systemscope_verify::locate::{Strategy, locate};

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
