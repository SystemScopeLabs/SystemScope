//! The regression corpus (`docs/m4-design.md` §18, §28.2): every entry replays byte for
//! byte, it holds the agreement case and SP-2, and a bundle that cannot be an entry is
//! refused.

use std::fs;

use systemscope_acceptance::corpus;
use systemscope_acceptance::m3::Fixture;
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;
use systemscope_verify::bundle::read_dir;
use systemscope_verify::compare::Outcome;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root(), Scenario::Reference).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn every_corpus_entry_replays() {
    let entries = corpus::replay(&fixture(), &workspace_root()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        entries,
        [
            ("agree".to_owned(), Outcome::Agree),
            ("sp2".to_owned(), Outcome::Diverged)
        ]
    );
}

#[test]
fn a_bundle_that_cannot_be_an_entry_is_refused() {
    let f = fixture();
    let root = workspace_root();
    let entry = read_dir(&root.join(corpus::DIR).join("sp2")).unwrap();
    assert_eq!(corpus::check(&f, &entry), Ok(Outcome::Diverged));
    // A changed result, an extra file, and a missing manifest.
    let mut changed = entry.clone();
    let result = changed.entries.get_mut("result.bin").unwrap();
    let last = result.len() - 1;
    result[last] ^= 1;
    assert!(corpus::check(&f, &changed).is_err());
    let mut extra = entry.clone();
    extra.entries.insert("notes.txt".to_owned(), b"x".to_vec());
    assert!(corpus::check(&f, &extra).is_err());
    let mut bare = entry.clone();
    bare.entries.remove("manifest.json");
    assert_eq!(corpus::check(&f, &bare), Err("no manifest.json".to_owned()));
    // A profile the corpus cannot replay.
    let mut other = entry;
    let manifest = String::from_utf8(other.entries["manifest.json"].clone()).unwrap();
    let renamed = manifest.replacen(
        "\"name\": \"m3-reference\"",
        "\"name\": \"spike-rv32ui\"",
        1,
    );
    assert_ne!(renamed, manifest);
    other
        .entries
        .insert("manifest.json".to_owned(), renamed.into_bytes());
    assert!(
        corpus::check(&f, &other)
            .unwrap_err()
            .contains("spike-rv32ui")
    );
}

#[test]
fn the_corpus_holds_only_bundle_directories() {
    let root = workspace_root();
    let scratch = root.join("target/m4-corpus-test");
    let _ = fs::remove_dir_all(&scratch);
    let dir = scratch.join(corpus::DIR);
    fs::create_dir_all(&dir).unwrap();
    assert!(corpus::replay(&fixture(), &scratch).unwrap_err()[0].contains("no entry"));
    fs::write(dir.join("README"), "x").unwrap();
    assert!(
        corpus::entries(&scratch)
            .unwrap_err()
            .contains("not a bundle directory")
    );
    let _ = fs::remove_dir_all(&scratch);
}
