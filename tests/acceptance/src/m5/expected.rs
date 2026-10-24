//! The committed expectations of `m5-reference` (`docs/m5-design.md` §21), the expected
//! side of its source-mutant case:
//!
//! - every stream but `arch` of the unpatched, unperturbed run under `m5.outcome`, one
//!   `.obs` file each under [`DIR`]; `arch`, nine megabytes, only by its BLAKE3;
//! - in [`RUNS_FILE`], each stream's BLAKE3 for every run the case checks: the unpatched
//!   scenario and the races unperturbed, the races at their first diverging seeds, the pinned minimized
//!   perturbations, and the unpatched scenario at every CI seed of A, B, and C; and the
//!   BLAKE3 of the dispatch order under C's seed 0, which reorders simultaneous events.
//!
//! `cargo xtask m5-expected bless` writes them, after checking that each race diverges,
//! each minimized perturbation reproduces its race, the unpatched scenario agrees, and
//! the tie seed reorders simultaneous events; `verify` requires this build to reproduce
//! them byte for byte.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value as Json, json};
use systemscope_runtime::ties;
use systemscope_rv32::m5::EVENT_BUDGET;
use systemscope_rv32::m5ref;
use systemscope_rv32::perturb::Perturbation;
use systemscope_verify::compare::{Recording, StreamResult, Verdict, compare_recordings};
use systemscope_verify::observation::Observation;
use systemscope_verify::profile::Plan;
use systemscope_verify::recorded::{stream_bytes, stream_from_bytes};

use super::campaign::{
    CI_SEEDS, FIRST_DIVERGENCE, Judged, Mechanism, Subject, judge as compare, perturbation, record,
    the_plan,
};
use super::minimize::PINNED;
use super::races::Race;
use super::verify::platform_config;
use super::{Fixture, parallel};
use crate::hex;

/// How [`judge`] starts a detected mutant's message. `cargo xtask m4-mutants summarize`
/// reads it, and what follows it, from cargo-mutants' logs.
pub const DETECTED: &str = "M5 source mutant detected:";

/// Where the expectations are committed.
pub const DIR: &str = "tests/verification/expected/m5-reference";
/// The runs' stream digests and the dispatch order's under C.
pub const RUNS_FILE: &str = "runs.json";
/// The stream committed only by its digest.
pub const ARCH: &str = "arch";
/// The unpatched, unperturbed run's name in [`RUNS_FILE`].
pub const UNPERTURBED: &str = "unpatched unperturbed";

/// What a perturbed run must do against its subject's unperturbed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// The subject's unperturbed run itself.
    Base,
    /// `m5.outcome` agrees.
    Agree,
    /// A finding: `outcome` diverges or a side fails.
    Finding,
}

/// One perturbed run the case checks.
#[derive(Clone, Debug)]
struct Run {
    name: String,
    subject: Subject,
    perturbation: Perturbation,
    expect: Expect,
}

/// The runs: the unpatched scenario and each race unperturbed, each race at its first
/// diverging seeds and under its minimized perturbations, then the unpatched scenario at
/// every CI seed.
fn runs() -> Vec<Run> {
    let mut runs: Vec<Run> = std::iter::once(Subject(None))
        .chain(Race::ALL.map(|r| Subject(Some(r))))
        .map(|s| Run {
            name: format!("{} unperturbed", s.name()),
            subject: s,
            perturbation: Perturbation::default(),
            expect: Expect::Base,
        })
        .collect();
    runs.extend(FIRST_DIVERGENCE.map(|(r, m, seed)| Run {
        name: format!("{} {m:?} seed {seed}", r.name()),
        subject: Subject(Some(r)),
        perturbation: perturbation(m, seed),
        expect: Expect::Finding,
    }));
    runs.extend(PINNED.map(|p| Run {
        name: format!("{} {:?} minimized", p.race.name(), p.mechanism),
        subject: Subject(Some(p.race)),
        perturbation: p.perturbation(),
        expect: Expect::Finding,
    }));
    for m in [Mechanism::A, Mechanism::B, Mechanism::C] {
        runs.extend(CI_SEEDS.map(|seed| Run {
            name: format!("unpatched {m:?} seed {seed}"),
            subject: Subject(None),
            perturbation: perturbation(m, seed),
            expect: Expect::Agree,
        }));
    }
    runs
}

/// Each stream's BLAKE3, by name, and the failure that ended the run, if any.
fn digests(plan: &Plan, r: &Recording) -> Json {
    let mut o = Map::new();
    for (s, obs) in plan.profile.streams().zip(&r.streams) {
        o.insert(
            s.stream.clone(),
            json!(hex(blake3::hash(&stream_bytes(obs)).as_bytes())),
        );
    }
    o.insert(
        "failure".to_owned(),
        r.failure
            .as_ref()
            .map_or(Json::Null, |e| json!(e.to_string())),
    );
    Json::Object(o)
}

/// The BLAKE3 of the dispatch order of the unpatched run under C's seed 0, each event as
/// its tick, phase, scheduling counter, source, and target. Fails unless some two
/// consecutive events with equal `(tick, phase)` dispatch out of scheduling order.
pub fn tie_order(fixture: &Fixture) -> Result<String, String> {
    let config = platform_config(&perturbation(Mechanism::C, 0));
    let mut rt = m5ref::build(&fixture.firmware, &fixture.disk, &config)
        .map_err(|e| format!("the tied platform does not build: {e:?}"))?;
    rt.init().map_err(|e| e.to_string())?;
    let mut h = blake3::Hasher::new();
    let (mut last, mut reordered) = (None, false);
    for _ in 0..EVENT_BUDGET {
        let Some(d) = rt.step().map_err(|e| format!("the tied run failed: {e}"))? else {
            break;
        };
        let (k, counter) = (d.key, ties::counter(d.key.sequence));
        if let Some((tick, phase, before)) = last {
            reordered |= tick == k.tick && phase == k.phase && counter < before;
        }
        last = Some((k.tick, k.phase, counter));
        let event = format!("{:?}", (k.tick, k.phase, counter, d.source, d.target));
        h.update(event.as_bytes());
    }
    if !reordered {
        return Err("tie seed 0 reorders no simultaneous events".to_owned());
    }
    Ok(hex(h.finalize().as_bytes()))
}

/// The runs' file, from this build, given the unpatched scenario's unperturbed run. With
/// `check`, each run must do what it is expected to against its subject's unperturbed
/// run.
fn perturbed(fixture: &Fixture, unpatched: &Recording, check: bool) -> Result<String, String> {
    let plan = the_plan(fixture);
    let runs = runs();
    let bases = parallel(&Race::ALL.map(|r| Subject(Some(r))), |&s| {
        record(fixture, s, &Perturbation::default())
    });
    let base = |s: Subject| match s.0 {
        None => unpatched,
        Some(r) => &bases[Race::ALL.iter().position(|&x| x == r).expect("a race")],
    };
    let seen = parallel(&runs, |run| {
        let r = match run.expect {
            Expect::Base => None,
            _ => Some(record(fixture, run.subject, &run.perturbation)),
        };
        let r = r.as_ref().unwrap_or_else(|| base(run.subject));
        (digests(&plan, r), compare(&plan, base(run.subject), r))
    });
    let mut out = Map::new();
    for (run, (d, judged)) in runs.iter().zip(seen) {
        let ok = match run.expect {
            Expect::Base => d["failure"].is_null(),
            Expect::Agree => judged == Judged::Agree,
            Expect::Finding => judged != Judged::Agree,
        };
        if check && !ok {
            return Err(format!(
                "{}: expected {:?}, judged {judged:?}",
                run.name, run.expect
            ));
        }
        out.insert(run.name.clone(), d);
    }
    let file = json!({
        "format": "systemscope-expected",
        "version": 1,
        "scenario": super::SCENARIO,
        "runs": out,
        "tie_order": tie_order(fixture)?,
    });
    let mut text = serde_json::to_string_pretty(&file).expect("JSON");
    text.push('\n');
    Ok(text)
}

/// Each file's name and bytes, recorded from this build: the unperturbed streams but
/// [`ARCH`] in profile order, then [`RUNS_FILE`].
pub fn record_all(fixture: &Fixture, check: bool) -> Result<Vec<(String, Vec<u8>)>, String> {
    let plan = the_plan(fixture);
    let r = record(fixture, Subject(None), &Perturbation::default());
    if let Some(f) = r.failure {
        return Err(format!("the unmodified m5-reference fails: {f}"));
    }
    let mut files: Vec<(String, Vec<u8>)> = plan
        .profile
        .streams()
        .zip(&r.streams)
        .filter(|(s, _)| s.stream != ARCH)
        .map(|(s, obs)| (format!("{}.obs", s.stream), stream_bytes(obs)))
        .collect();
    files.push((
        RUNS_FILE.to_owned(),
        perturbed(fixture, &r, check)?.into_bytes(),
    ));
    Ok(files)
}

/// Writes this build's files under `root`, after checking every run.
pub fn bless(fixture: &Fixture, root: &Path) -> Result<(), String> {
    let dir = root.join(DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, bytes) in record_all(fixture, true)? {
        let path = dir.join(name);
        fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Requires the committed files under `root` to equal this build's byte for byte.
pub fn verify(fixture: &Fixture, root: &Path) -> Result<(), Vec<String>> {
    let fresh = record_all(fixture, true).map_err(|e| vec![e])?;
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

/// The committed unperturbed streams under `root`, as a recording under `plan`, with
/// `arch` for [`ARCH`].
pub fn committed(plan: &Plan, root: &Path, arch: &[Observation]) -> Result<Recording, String> {
    let streams = plan
        .profile
        .streams()
        .map(|s| {
            if s.stream == ARCH {
                return Ok(arch.to_vec());
            }
            let path = root.join(DIR).join(format!("{}.obs", s.stream));
            let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            stream_from_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect::<Result<_, String>>()?;
    Ok(Recording {
        streams,
        failure: None,
    })
}

/// The first difference between two runs' files, as `run stream`.
pub fn first_difference(expected: &Json, actual: &Json) -> Option<String> {
    let (Some(e), Some(a)) = (expected["runs"].as_object(), actual["runs"].as_object()) else {
        return Some("runs".to_owned());
    };
    for (run, digests) in e {
        let Some(fresh) = a.get(run) else {
            return Some(format!("{run}, not run"));
        };
        let streams = digests.as_object().into_iter().flatten();
        if let Some((stream, _)) = streams.into_iter().find(|(s, d)| fresh[s.as_str()] != **d) {
            return Some(format!("{run} {stream}"));
        }
    }
    (expected["tie_order"] != actual["tie_order"]).then(|| "tie order".to_owned())
}

/// The source-mutant case (§21): this build's `m5-reference` against the committed
/// expectations, cheapest first, stopping at the first detection. `Ok` when everything
/// agrees. Otherwise the message starts with [`DETECTED`] and names the detection:
///
/// - on the unperturbed run, `Diverged at outcome[n]`, `AuxiliaryDivergence in s[n]`,
///   `AuxiliaryDivergence in arch` by its digest, or `BackendFailed`;
/// - on another run, `PerturbedDivergence in <run> <stream>`;
/// - `TieOrder` when the tie seed reorders no simultaneous events, or reorders them
///   another way.
pub fn judge(fixture: &Fixture, root: &Path) -> Result<(), String> {
    let plan = the_plan(fixture);
    let live = record(fixture, Subject(None), &Perturbation::default());
    let arch = plan
        .profile
        .streams()
        .position(|s| s.stream == ARCH)
        .expect("an arch stream");
    let expected = committed(&plan, root, &live.streams[arch])?;
    let c = match compare_recordings(&plan, &expected, &live) {
        Ok(Verdict::Compared(c)) => c,
        Ok(Verdict::Incomparable(why)) => return Err(format!("incomparable: {why:?}")),
        Err(e) => return Err(format!("{DETECTED} BackendFailed: {e}")),
    };
    if let StreamResult::Diverged {
        stream, ordinal, ..
    } = &c.primary
    {
        return Err(format!("{DETECTED} Diverged at {stream}[{ordinal}]"));
    }
    if let Some((_, e)) = c.failures.first() {
        return Err(format!("{DETECTED} BackendFailed: {e}"));
    }
    if let Some(StreamResult::Diverged {
        stream, ordinal, ..
    }) = c.auxiliary.iter().find(|a| !a.agrees())
    {
        return Err(format!(
            "{DETECTED} AuxiliaryDivergence in {stream}[{ordinal}]"
        ));
    }

    let path = root.join(DIR).join(RUNS_FILE);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let committed: Json =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let digest = hex(blake3::hash(&stream_bytes(&live.streams[arch])).as_bytes());
    if committed["runs"][UNPERTURBED][ARCH] != json!(digest) {
        return Err(format!("{DETECTED} AuxiliaryDivergence in {ARCH}"));
    }
    let fresh = match perturbed(fixture, &live, false) {
        Ok(t) => serde_json::from_str::<Json>(&t).expect("this build's JSON"),
        Err(e) => return Err(format!("{DETECTED} TieOrder: {e}")),
    };
    match first_difference(&committed, &fresh) {
        None => Ok(()),
        Some(w) if w == "tie order" => Err(format!("{DETECTED} TieOrder: another order")),
        Some(w) => Err(format!("{DETECTED} PerturbedDivergence in {w}")),
    }
}
