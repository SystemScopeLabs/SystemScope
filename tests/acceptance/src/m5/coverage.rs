//! The coverage reports (`docs/m5-design.md` §17): architectural coverage of
//! `m5-reference` and `m3-reference`, and schedule and perturbation coverage of the
//! `m5-reference` campaigns. Each is a reviewed file under [`DIR`]; `cargo xtask
//! m5-coverage bless` writes them, and `verify` requires this build to reproduce them
//! byte for byte. Reproduction is the only check: no coverage figure is a gate.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use serde_json::{Value as Json, json};
use systemscope_contracts::trace::Value;
use systemscope_rv32::perturb::Perturbation;
use systemscope_verify::backend::{Side, run};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::Recording;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::Plan;
use systemscope_verify::recorded::stream_bytes;

use super::Fixture;
use super::campaign::{Judged, Mechanism, Subject, judge, perturbation, record, the_plan};
use super::races::Race;
use super::verify::SCHEDULE;
use crate::coverage;
use crate::m3::{self, expected};

/// Where the reports are committed.
pub const DIR: &str = "tests/verification/coverage";
/// `m3-reference`'s architectural coverage.
pub const M3_FILE: &str = "m3-reference.json";
/// `m5-reference`'s architectural coverage.
pub const M5_FILE: &str = "m5-reference.json";
/// The `m5-reference` campaigns' schedule and perturbation coverage.
pub const CAMPAIGN_FILE: &str = "m5-campaign.json";

/// The seeds the coverage campaign runs for each mechanism and subject (O5).
pub const SEEDS: std::ops::Range<u64> = 0..32;
/// The mechanisms it runs.
pub const MECHANISMS: [Mechanism; 3] = [Mechanism::A, Mechanism::B, Mechanism::C];
/// The most runs it keeps in flight.
const WORKERS: usize = 4;

/// A report's text: pretty JSON with a final newline.
fn text(report: &Json) -> String {
    let mut s = serde_json::to_string_pretty(report).expect("JSON");
    s.push('\n');
    s
}

fn report(platform: &str, body: (&str, Json)) -> Json {
    json!({
        "format": "systemscope-coverage",
        "version": 1,
        "platform": platform,
        body.0: body.1,
    })
}

/// The observations of `name` in `r`, a recording under `plan`.
fn stream<'a>(plan: &Plan, r: &'a Recording, name: &str) -> &'a [Observation] {
    let i = plan
        .profile
        .streams()
        .position(|s| s.stream == name)
        .unwrap_or_else(|| panic!("the plan has no {name} stream"));
    &r.streams[i]
}

/// `m5-reference`'s architectural coverage, from its unperturbed run.
pub fn m5_arch(fixture: &Fixture) -> Result<String, String> {
    let r = record(fixture, Subject(None), &Perturbation::default());
    if let Some(f) = &r.failure {
        return Err(format!("m5-reference fails: {f}"));
    }
    let arch = coverage::arch(stream(&the_plan(fixture), &r, "arch"));
    Ok(text(&report("m5-reference", ("arch", arch))))
}

/// `m3-reference`'s architectural coverage, for comparison.
pub fn m3_arch(fixture: &m3::Fixture) -> Result<String, String> {
    let plan = expected::the_plan(fixture)?;
    let mut b = m3::verify::backend(fixture);
    let side = Side {
        backend: &mut b,
        input: InputVariant::None,
        config: m3::verify::config(),
    };
    let r = run(&plan, &m3::verify::scenario(fixture), side);
    if let Some(f) = &r.failure {
        return Err(format!("m3-reference fails: {f}"));
    }
    let arch = coverage::arch(stream(&plan, &r, "arch"));
    Ok(text(&report("m3-reference", ("arch", arch))))
}

/// The campaign's subjects: the unpatched scenario, then each race.
fn subjects() -> Vec<Subject> {
    std::iter::once(Subject(None))
        .chain(Race::ALL.map(|r| Subject(Some(r))))
        .collect()
}

/// What the campaign keeps of one perturbed run.
struct Seen {
    schedule: [u8; 32],
    preempted: BTreeSet<(u64, u64)>,
    judged: Judged,
}

fn number(o: &Observation, name: &str) -> u64 {
    match o.field(name) {
        Some(ObservedValue::Present(Value::U64(v))) => *v,
        other => panic!("{} {name}: {other:?}", o.kind),
    }
}

/// The schedule's BLAKE3 and its preemption points, `(pid, sepc)`.
fn schedule_of(plan: &Plan, r: &Recording) -> ([u8; 32], BTreeSet<(u64, u64)>) {
    let s = stream(plan, r, SCHEDULE);
    let preempted = s
        .iter()
        .filter(|o| o.kind == "preempt")
        .map(|o| (number(o, "pid"), number(o, "sepc")))
        .collect();
    (*blake3::hash(&stream_bytes(s)).as_bytes(), preempted)
}

/// Runs `jobs` on up to [`WORKERS`] threads, in any order, keeping each result at its
/// job's index.
fn parallel<J: Sync, R: Send>(jobs: &[J], f: impl Fn(&J) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new(jobs.iter().map(|_| None).collect());
    thread::scope(|s| {
        for _ in 0..WORKERS {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let r = f(job);
                    out.lock().expect("unpoisoned")[i] = Some(r);
                }
            });
        }
    });
    out.into_inner()
        .expect("unpoisoned")
        .into_iter()
        .map(|r| r.expect("every job ran"))
        .collect()
}

/// Schedule and perturbation coverage of the campaigns: for each mechanism and subject,
/// over [`SEEDS`], how many seeds changed the schedule, how many distinct schedules ran,
/// and how many seeds diverged or failed; for the unpatched scenario, each process's
/// preemption points.
pub fn campaign(fixture: &Fixture) -> Result<String, String> {
    let plan = the_plan(fixture);
    let subjects = subjects();
    let bases = parallel(&subjects, |&s| {
        let r = record(fixture, s, &Perturbation::default());
        (schedule_of(&plan, &r), r)
    });
    for (s, (_, r)) in subjects.iter().zip(&bases) {
        if let Some(f) = &r.failure {
            return Err(format!("{} fails unperturbed: {f}", s.name()));
        }
    }
    let jobs: Vec<(Mechanism, usize, u64)> = MECHANISMS
        .iter()
        .flat_map(|&m| (0..subjects.len()).flat_map(move |i| SEEDS.map(move |seed| (m, i, seed))))
        .collect();
    let seen = parallel(&jobs, |&(m, i, seed)| {
        let r = record(fixture, subjects[i], &perturbation(m, seed));
        let (schedule, preempted) = schedule_of(&plan, &r);
        Seen {
            schedule,
            preempted,
            judged: judge(&plan, &bases[i].1, &r),
        }
    });

    let mut mechanisms = serde_json::Map::new();
    for &m in &MECHANISMS {
        let mut per_subject = serde_json::Map::new();
        let mut points: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
        for (i, subject) in subjects.iter().enumerate() {
            let ((base_schedule, base_preempted), _) = &bases[i];
            let runs: Vec<&Seen> = jobs
                .iter()
                .zip(&seen)
                .filter(|((jm, ji, _), _)| *jm == m && *ji == i)
                .map(|(_, s)| s)
                .collect();
            let changed = runs.iter().filter(|s| s.schedule != *base_schedule).count();
            let distinct: BTreeSet<&[u8; 32]> = std::iter::once(base_schedule)
                .chain(runs.iter().map(|s| &s.schedule))
                .collect();
            let diverged = runs
                .iter()
                .filter(|s| matches!(s.judged, Judged::Diverged(_)))
                .count();
            let failed = runs
                .iter()
                .filter(|s| matches!(s.judged, Judged::Failed(_)))
                .count();
            per_subject.insert(
                subject.name().to_owned(),
                json!({
                    "schedule_changed": format!("{changed}/{}", runs.len()),
                    "distinct_schedules": distinct.len(),
                    "diverged": diverged,
                    "failed": failed,
                }),
            );
            if subject.0.is_none() {
                for &(pid, sepc) in base_preempted
                    .iter()
                    .chain(runs.iter().flat_map(|s| &s.preempted))
                {
                    points.entry(pid).or_default().insert(sepc);
                }
            }
        }
        let effective = per_subject.values().any(|v| {
            !v["schedule_changed"]
                .as_str()
                .is_some_and(|s| s.starts_with("0/"))
        });
        let points: BTreeMap<String, Vec<String>> = points
            .into_iter()
            .map(|(pid, pcs)| {
                (
                    pid.to_string(),
                    pcs.iter().map(|pc| format!("{pc:#010x}")).collect(),
                )
            })
            .collect();
        mechanisms.insert(
            format!("{m:?}"),
            json!({
                "effective": effective,
                "subjects": per_subject,
                "unpatched_preemption_points": points,
            }),
        );
    }
    let body = json!({
        "seeds": format!("{}..{}", SEEDS.start, SEEDS.end),
        "mechanisms": mechanisms,
    });
    Ok(text(&report("m5-reference", ("campaign", body))))
}

/// Which reports a call covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reports {
    /// The architectural reports, which take two runs.
    Arch,
    /// The campaign report, which takes about 500 runs.
    Campaign,
}

fn fresh(
    m3f: &m3::Fixture,
    m5f: &Fixture,
    reports: Reports,
) -> Result<Vec<(&'static str, String)>, String> {
    Ok(match reports {
        Reports::Arch => vec![(M3_FILE, m3_arch(m3f)?), (M5_FILE, m5_arch(m5f)?)],
        Reports::Campaign => vec![(CAMPAIGN_FILE, campaign(m5f)?)],
    })
}

/// Writes this build's `reports` under `root`.
pub fn bless(
    m3f: &m3::Fixture,
    m5f: &Fixture,
    root: &Path,
    reports: Reports,
) -> Result<(), String> {
    let dir = root.join(DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, text) in fresh(m3f, m5f, reports)? {
        let path = dir.join(name);
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Requires the committed `reports` under `root` to equal this build's byte for byte.
pub fn verify(
    m3f: &m3::Fixture,
    m5f: &Fixture,
    root: &Path,
    reports: Reports,
) -> Result<(), Vec<String>> {
    let errors: Vec<String> = fresh(m3f, m5f, reports)
        .map_err(|e| vec![e])?
        .into_iter()
        .filter_map(|(name, text)| {
            let path = root.join(DIR).join(name);
            match fs::read_to_string(&path) {
                Ok(committed) if committed == text => None,
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
