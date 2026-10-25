//! The regression corpus (`docs/m4-design.md` §18): canonical directory bundles under
//! [`DIR`], each a `Diverged` or `Agree` reproducer that must keep replaying byte for byte.
//!
//! No function here writes into the corpus. A candidate is a bundle in a scratch
//! directory, such as one `cargo xtask m4-repro emit` wrote; [`check`] says whether it can
//! be an entry, and a reviewed commit copies it in.

use std::fs;
use std::path::{Path, PathBuf};

use systemscope_verify::bundle::{Bundle, Manifest, read_dir};
use systemscope_verify::compare::Outcome;

use crate::m3::{self, Fixture};
use crate::m4::repro;

/// Where the corpus is committed.
pub const DIR: &str = "tests/verification/corpus";

/// The manifest's entry path.
const MANIFEST: &str = "manifest.json";

/// Whether `bundle` can be a corpus entry: its profile is one the corpus can replay
/// (`m3-reference`), its outcome is `Diverged` or `Agree`, and it validates and replays
/// here byte for byte. Returns its outcome.
pub fn check(fixture: &Fixture, bundle: &Bundle) -> Result<Outcome, String> {
    let bytes = bundle
        .entries
        .get(MANIFEST)
        .ok_or_else(|| format!("no {MANIFEST}"))?;
    let manifest = Manifest::parse(bytes)?;
    if manifest.profile.name != m3::SCENARIO {
        return Err(format!(
            "the corpus replays only {} bundles, not {}",
            m3::SCENARIO,
            manifest.profile.name
        ));
    }
    if manifest.outcome == Outcome::AuxiliaryDivergence {
        return Err("an auxiliary divergence is never a corpus entry".to_owned());
    }
    repro::replayed(fixture, bundle)?;
    Ok(manifest.outcome)
}

/// Reads the bundle in `dir` and [`check`]s it.
pub fn check_dir(fixture: &Fixture, dir: &Path) -> Result<Outcome, String> {
    let bundle = read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    check(fixture, &bundle)
}

/// Every entry directory under `root`, in name order.
pub fn entries(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join(DIR);
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if !path.is_dir() {
            return Err(format!("{} is not a bundle directory", path.display()));
        }
        out.push(path);
    }
    out.sort();
    Ok(out)
}

/// Checks every entry under `root`: each entry's name and outcome, or every error.
pub fn replay(fixture: &Fixture, root: &Path) -> Result<Vec<(String, Outcome)>, Vec<String>> {
    let dirs = entries(root).map_err(|e| vec![e])?;
    if dirs.is_empty() {
        return Err(vec![format!("{DIR} holds no entry")]);
    }
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for dir in dirs {
        let name = dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match check_dir(fixture, &dir) {
            Ok(outcome) => ok.push((name, outcome)),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    match errors.is_empty() {
        true => Ok(ok),
        false => Err(errors),
    }
}
