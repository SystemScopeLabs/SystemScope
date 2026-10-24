//! M5 reproducer bundles of `m5-reference` (`docs/m5-design.md` §18, §25): each pinned
//! minimized perturbation's reproducer, the unperturbed side against the perturbed one.
//! Every bundle is a pure function of the committed fixtures and [`PINNED`], so another
//! machine's must equal this machine's byte for byte and replay here to the same result.

use std::path::Path;

use systemscope_rv32::perturb::Perturbation;
use systemscope_verify::bundle::{Bundle, RESULT, read_dir, write_dir};

use super::campaign::{Mechanism, Subject};
use super::minimize::{PINNED, Pinned, replay, reproducer};
use super::{Fixture, parallel};

/// The directory suffix of a bundle.
pub const SUFFIX: &str = ".repro";

/// The bundle name of `pinned`, the race's name and the mechanism's letter.
pub fn name(pinned: &Pinned) -> String {
    let letter = match pinned.mechanism {
        Mechanism::A => "a",
        Mechanism::B => "b",
        Mechanism::C => "c",
    };
    format!("{}-{letter}", pinned.race.name())
}

fn bundle(fixture: &Fixture, pinned: &Pinned) -> Result<Bundle, String> {
    let subject = Subject(Some(pinned.race));
    reproducer(
        fixture,
        subject,
        [&Perturbation::default(), &pinned.perturbation()],
    )
    .map_err(|e| format!("{}: {e}", name(pinned)))
}

/// Every bundle, by name, in [`PINNED`] order.
pub fn bundles(fixture: &Fixture) -> Result<Vec<(String, Bundle)>, String> {
    parallel(&PINNED, |p| bundle(fixture, p).map(|b| (name(p), b)))
        .into_iter()
        .collect()
}

/// Writes every bundle into `dir`, as `<name>.repro`.
pub fn emit(fixture: &Fixture, dir: &Path) -> Result<(), String> {
    for (name, b) in bundles(fixture)? {
        let at = dir.join(format!("{name}{SUFFIX}"));
        write_dir(&b, &at).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    Ok(())
}

/// Replays `b`, the bundle of `pinned`, here: the replayed result must equal the one it
/// holds.
pub fn replayed(fixture: &Fixture, pinned: &Pinned, b: &Bundle) -> Result<(), String> {
    let r = replay(fixture, Subject(Some(pinned.race)), b).map_err(|e| format!("{e:?}"))?;
    if b.entries.get(RESULT).map(Vec::as_slice) == Some(&r.to_bytes()[..]) {
        Ok(())
    } else {
        Err(format!("the replayed result differs from {RESULT}"))
    }
}

/// Requires another machine's bundles in `dir` to equal this machine's byte for byte,
/// entry by entry, and to replay here.
pub fn check(fixture: &Fixture, dir: &Path) -> Result<(), Vec<String>> {
    let own = bundles(fixture).map_err(|e| vec![e])?;
    let errors: Vec<String> = parallel(&PINNED, |pinned| {
        let name = name(pinned);
        let at = dir.join(format!("{name}{SUFFIX}"));
        let foreign = match read_dir(&at) {
            Ok(b) => b,
            Err(e) => return vec![format!("{}: {e}", at.display())],
        };
        let (_, own) = own.iter().find(|(n, _)| *n == name).expect("every pinned");
        let mut errors = Vec::new();
        let paths = own.entries.keys().chain(foreign.entries.keys());
        for path in paths.collect::<std::collections::BTreeSet<_>>() {
            if own.entries.get(path) != foreign.entries.get(path) {
                errors.push(format!("{name}: {path} differs from this machine's"));
            }
        }
        if let Err(e) = replayed(fixture, pinned, &foreign) {
            errors.push(format!("{name}: {e}"));
        }
        errors
    })
    .into_iter()
    .flatten()
    .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Replays every `*.repro` bundle in `dir` here, each named for a pinned perturbation.
pub fn replay_all(fixture: &Fixture, dir: &Path) -> Result<usize, Vec<String>> {
    let entries = std::fs::read_dir(dir).map_err(|e| vec![format!("{}: {e}", dir.display())])?;
    let mut dirs: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == &SUFFIX[1..]))
        .collect();
    dirs.sort();
    let errors: Vec<String> = parallel(&dirs, |at| {
        let stem = at.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let pinned = PINNED
            .iter()
            .find(|p| name(p) == stem)
            .ok_or_else(|| format!("no pinned perturbation is named {stem}"));
        pinned
            .and_then(|p| {
                read_dir(at)
                    .map_err(|e| e.to_string())
                    .and_then(|b| replayed(fixture, p, &b))
            })
            .err()
            .map(|e| format!("{}: {e}", at.display()))
    })
    .into_iter()
    .flatten()
    .collect();
    match (dirs.len(), errors.is_empty()) {
        (0, _) => Err(vec![format!("no {SUFFIX} bundle in {}", dir.display())]),
        (n, true) => Ok(n),
        _ => Err(errors),
    }
}
