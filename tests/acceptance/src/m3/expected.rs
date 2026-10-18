//! The committed expected streams of `m3-reference` (`docs/m4-design.md` §25.2): every
//! stream of the `m3-reference` profile, recorded from the unmodified build, one `.obs`
//! file each under [`DIR`]. A source mutant is compared against them, so the expected side
//! does not run the mutated code. `cargo xtask m4-expected bless` writes them, and
//! `verify` requires this build to reproduce them byte for byte.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::case::{BackendId, InputVariant};
use systemscope_verify::observation::Observation;
use systemscope_verify::profile::{Plan, plan};
use systemscope_verify::recorded::{RecordedBackend, stream_bytes, stream_from_bytes};

use super::{Fixture, verify};

/// Where the expected streams are committed.
pub const DIR: &str = "tests/verification/expected/m3-reference";

/// The `m3-reference` plan of two unmodified sides.
pub fn the_plan(fixture: &Fixture) -> Result<Plan, String> {
    let b = verify::backend(fixture);
    plan(&verify::profile(), &b.support(), &b.support()).map_err(|e| format!("{e:?}"))
}

/// Each stream's file name and canonical bytes, recorded from this build, in profile
/// order.
pub fn record(fixture: &Fixture) -> Result<Vec<(String, Vec<u8>)>, String> {
    let plan = the_plan(fixture)?;
    let mut b = verify::backend(fixture);
    let side = Side {
        backend: &mut b,
        input: InputVariant::None,
        config: verify::config(),
    };
    let r = run(&plan, &verify::scenario(fixture), side);
    if let Some(f) = r.failure {
        return Err(format!("the unmodified m3-reference fails: {f}"));
    }
    Ok(plan
        .profile
        .streams()
        .zip(r.streams)
        .map(|(s, obs)| (format!("{}.obs", s.stream), stream_bytes(&obs)))
        .collect())
}

/// Writes this build's streams under `root`.
pub fn bless(fixture: &Fixture, root: &Path) -> Result<(), String> {
    let dir = root.join(DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, bytes) in record(fixture)? {
        let path = dir.join(name);
        fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Requires the committed streams under `root` to equal this build's byte for byte.
pub fn verify(fixture: &Fixture, root: &Path) -> Result<(), Vec<String>> {
    let fresh = record(fixture).map_err(|e| vec![e])?;
    let errors: Vec<String> = fresh
        .iter()
        .filter_map(|(name, bytes)| {
            let path = root.join(DIR).join(name);
            match fs::read(&path) {
                Ok(committed) if committed == *bytes => None,
                Ok(_) => Some(format!("{} differs from this build's", path.display())),
                Err(e) => Some(format!("{}: {e}", path.display())),
            }
        })
        .collect();
    match errors.is_empty() {
        true => Ok(()),
        false => Err(errors),
    }
}

/// The committed streams under `root` as the expected side of an `m3-reference` case.
pub fn backend(fixture: &Fixture, root: &Path) -> Result<RecordedBackend, String> {
    let plan = the_plan(fixture)?;
    let streams = plan
        .profile
        .streams()
        .map(|s| {
            let path = root.join(DIR).join(format!("{}.obs", s.stream));
            let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let obs = stream_from_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((s.stream.clone(), obs))
        })
        .collect::<Result<BTreeMap<String, Vec<Observation>>, String>>()?;
    Ok(RecordedBackend::new(
        BackendId {
            name: "recorded".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: super::SCENARIO.to_owned(),
        },
        verify::backend(fixture).support(),
        &verify::scenario(fixture),
        &plan.profile,
        streams,
    ))
}
