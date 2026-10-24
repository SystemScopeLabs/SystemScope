//! `m5-reference`'s golden files and its every-event resume (`docs/m5-design.md` §12,
//! §19), and in [`verify`] its streams under the M4 engine (§13, §15).
//!
//! - `tests/golden/m5-reference.json`: the firmware's and the disk's BLAKE3, the exact
//!   UART output, the run's metrics and coverage, `StateDigest`, `ExecutionDigest`,
//!   `TraceDigest`, the final snapshot's size and BLAKE3, and the mid snapshot's
//!   checkpoint, size, and BLAKE3.
//! - `tests/golden/m5-reference.mid.snap`: the runtime snapshot at [`MID_CHECKPOINT`],
//!   with both readers blocked on the block controller.
//!
//! Only `cargo xtask m5-golden bless` writes them, through [`Golden::generate`] and
//! [`Golden::render`]. Tests and `cargo xtask m5-golden verify` read the committed copies
//! and never write. The event watchdog is not part of either file.
//!
//! The every-event resume is linear, not M2's and M3's resume-to-the-end, which would be
//! quadratic in the run's 1.3 million events: checkpoint `k` is restored into a freshly
//! built platform, must snapshot back to the same bytes, and must then dispatch the
//! uninterrupted run's event `k` and reach its checkpoint `k + 1` byte for byte. By
//! induction, a resume from any checkpoint replays the rest of the run; the golden check
//! resumes the mid snapshot to the end.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use serde_json::{Map, Value};
use systemscope_contracts::COMPATIBILITY_ID;
use systemscope_contracts::trace::TraceOrigin;
use systemscope_elf::LoadImage;
use systemscope_os::interrupts::{BLOCK_KIND, WAKE_KIND};
use systemscope_runtime::runtime::Runtime;
use systemscope_rv32::m5::{self, EVENT_BUDGET, M5Manifest, M5Run, Metrics};
use systemscope_rv32::m5ref::{self, Config};
use systemscope_rv32::runner::Start;

use crate::hex;

pub mod campaign;
pub mod coverage;
pub mod evidence;
pub mod expected;
pub mod minimize;
pub mod races;
pub mod repro;
pub mod verify;

/// The most runs [`parallel`] keeps in flight.
const WORKERS: usize = 4;

/// Runs `jobs` on up to four threads, in any order, keeping each result at its
/// job's index.
pub fn parallel<J: Sync, R: Send>(jobs: &[J], f: impl Fn(&J) -> R + Sync) -> Vec<R> {
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

/// The scenario's name in the golden file.
pub const SCENARIO: &str = "m5-reference";
/// The golden file, relative to the workspace root.
pub const GOLDEN_PATH: &str = "tests/golden/m5-reference.json";
/// The mid snapshot, relative to the workspace root.
pub const MID_SNAPSHOT_PATH: &str = "tests/golden/m5-reference.mid.snap";
/// The mid snapshot's file name, as the golden file records it.
pub const MID_FILE: &str = "m5-reference.mid.snap";
/// Where the mid snapshot is taken.
pub const MID_CHECKPOINT: &str = "after the first event that leaves two readers blocked";

/// The committed M5 fixture: the firmware, the disk, and the expected `os.*` records, read
/// through the M5 manifest.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// BLAKE3 of the firmware ELF, as the manifest pins it.
    pub firmware_blake3: [u8; 32],
    /// The loaded firmware.
    pub firmware: LoadImage,
    /// BLAKE3 of the disk, as the manifest pins it.
    pub disk_blake3: [u8; 32],
    /// The raw disk.
    pub disk: Vec<u8>,
    /// The committed `os.*` records.
    pub expected: String,
}

impl Fixture {
    /// Reads the committed fixture under `root`; fails unless every file is exactly the
    /// one the manifest names.
    pub fn read(root: &Path) -> Result<Fixture, String> {
        let manifest = M5Manifest::read(root)?;
        let (firmware, disk, expected) = m5::read_fixture(root)?;
        Ok(Fixture {
            firmware_blake3: manifest.firmware.blake3,
            firmware,
            disk_blake3: manifest.disk.blake3,
            disk,
            expected,
        })
    }

    /// The frozen `m5-reference` with this disk, elaborated.
    ///
    /// # Panics
    ///
    /// If the frozen platform does not build, which M5.3b rules out.
    pub fn platform(&self) -> Runtime {
        self.platform_with(&Config::frozen())
            .expect("the frozen m5-reference builds")
    }

    /// `m5-reference` with this disk under `config`, elaborated.
    pub fn platform_with(&self, config: &Config) -> Result<Runtime, String> {
        m5ref::build(&self.firmware, &self.disk, config).map_err(|e| format!("{e:?}"))
    }

    /// Runs the workload from `start` to its end on a fresh frozen platform, under the
    /// watchdog.
    pub fn run(&self, start: Start) -> M5Run {
        m5::run(&self.firmware, &self.disk, start, Vec::new())
    }

    /// Runs the workload traced from `init` and judges it, the `os.*` records included.
    pub fn judged_run(&self) -> Result<M5Run, String> {
        let run = self.run(Start::Init { traced: true });
        m5::judge(&run, &self.expected)?;
        Ok(run)
    }
}

/// [`MID_CHECKPOINT`] in a traced run: the number of events up to and including the one
/// whose records leave two processes blocked.
pub fn mid_checkpoint(run: &M5Run) -> Result<usize, String> {
    let trace = run
        .finished
        .trace
        .as_ref()
        .ok_or("the run was not traced")?;
    let (mut events, mut blocked) = (0usize, 0usize);
    for r in &trace.records {
        match r.origin {
            TraceOrigin::Runtime => events += 1,
            TraceOrigin::Component if r.kind == BLOCK_KIND => {
                blocked += 1;
                if blocked == 2 {
                    return Ok(events);
                }
            }
            TraceOrigin::Component if r.kind == WAKE_KIND => blocked -= 1,
            _ => {}
        }
    }
    Err("no event leaves two processes blocked".to_owned())
}

/// Snapshots the frozen platform after `k` events of an untraced run from `init`.
pub fn snapshot_after(fixture: &Fixture, k: usize) -> Result<Vec<u8>, String> {
    let mut rt = fixture.platform();
    rt.init().map_err(|e| e.to_string())?;
    for i in 0..k {
        rt.step()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the run ended after {i} events"))?;
    }
    rt.snapshot().map_err(|e| e.to_string())
}

/// Restores `snapshot` into a freshly built platform, requires it to snapshot back to the
/// same bytes, and runs it to its end under the watchdog; returns the final snapshot and
/// the platform.
pub fn resume_to_end(fixture: &Fixture, snapshot: &[u8]) -> Result<(Vec<u8>, Runtime), String> {
    let mut rt = fixture.platform();
    rt.restore(snapshot)
        .map_err(|e| format!("restore failed: {e}"))?;
    if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
        return Err("snapshot -> restore -> snapshot is not the identity".to_owned());
    }
    let mut events = 0u64;
    loop {
        if events >= EVENT_BUDGET {
            return Err(format!("stopped at the budget of {EVENT_BUDGET} events"));
        }
        match rt.step() {
            Ok(Some(_)) => events += 1,
            Ok(None) => break,
            Err(e) => return Err(format!("the resumed run failed: {e}")),
        }
    }
    if let Some(e) = rt.fault() {
        return Err(format!("the resumed run faulted: {e}"));
    }
    Ok((rt.snapshot().map_err(|e| e.to_string())?, rt))
}

/// The golden file's contents, as `(key, JSON value)` pairs in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Golden {
    /// Every field, rendered.
    pub fields: Vec<(String, String)>,
}

fn q(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

fn digest(d: &[u8]) -> String {
    q(&hex(d))
}

impl Golden {
    /// Runs the committed fixture under `root`, judges it, and returns the golden file and
    /// the mid snapshot.
    pub fn generate(root: &Path) -> Result<(Golden, Vec<u8>), String> {
        let fixture = Fixture::read(root)?;
        let run = fixture.judged_run()?;
        let k = mid_checkpoint(&run)?;
        let mid = snapshot_after(&fixture, k)?;
        Ok((Golden::of(&fixture, &run, k, &mid)?, mid))
    }

    /// The golden file of a judged run whose mid snapshot, after `k` events, is `mid`.
    pub fn of(fixture: &Fixture, run: &M5Run, k: usize, mid: &[u8]) -> Result<Golden, String> {
        let m = Metrics::of(run);
        let output = run.output.clone()?;
        let snapshot = run
            .snapshot
            .as_deref()
            .ok_or("the run has no final snapshot")?;
        let state = m.state.ok_or("the run has no StateDigest")?;
        let trace = m.trace.ok_or("the run has no TraceDigest")?;
        let c = &m.coverage;
        let n = |v: u64| v.to_string();
        let fields: Vec<(&str, String)> = vec![
            ("scenario", q(SCENARIO)),
            ("compatibility_id", q(COMPATIBILITY_ID)),
            ("firmware_blake3", digest(&fixture.firmware_blake3)),
            ("disk_blake3", digest(&fixture.disk_blake3)),
            ("uart", q(&hex(&output))),
            ("uart_blake3", digest(blake3::hash(&output).as_bytes())),
            ("events", n(m.events)),
            ("instret", n(m.instret)),
            ("os_records", n(m.os_records as u64)),
            ("syscalls", n(m.syscalls as u64)),
            ("switches", n(m.switches as u64)),
            ("preempts", n(m.preempts as u64)),
            ("interrupts", n(m.interrupts as u64)),
            ("dma_commands", n(m.dma_commands as u64)),
            ("uart_bytes", n(m.uart_bytes as u64)),
            (
                "counter_preempts",
                format!("[{}, {}]", c.counter_preempts[0], c.counter_preempts[1]),
            ),
            ("max_blocked", n(c.max_blocked as u64)),
            ("idle_interrupts", n(c.idle_interrupts as u64)),
            ("both_sources", n(c.both_sources as u64)),
            ("state", digest(&state)),
            ("execution", digest(&m.execution)),
            ("trace", digest(&trace)),
            ("final_snapshot_size", n(snapshot.len() as u64)),
            (
                "final_snapshot_blake3",
                digest(blake3::hash(snapshot).as_bytes()),
            ),
            ("mid_file", q(MID_FILE)),
            ("mid_checkpoint", q(MID_CHECKPOINT)),
            ("mid_event", n(k as u64)),
            ("mid_size", n(mid.len() as u64)),
            ("mid_blake3", digest(blake3::hash(mid).as_bytes())),
        ];
        Ok(Golden {
            fields: fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        })
    }

    /// The canonical file: one field per line, in order.
    pub fn render(&self) -> String {
        let body: Vec<String> = self
            .fields
            .iter()
            .map(|(k, v)| format!("  {}: {v}", q(k)))
            .collect();
        format!("{{\n{}\n}}\n", body.join(",\n"))
    }

    /// The field `key`, rendered.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Restores `snapshot`, the mid snapshot, into a fresh platform, runs it to the end,
    /// and requires the golden final snapshot and digests.
    pub fn check_mid(&self, fixture: &Fixture, snapshot: &[u8]) -> Result<(), String> {
        if self.get("mid_size") != Some(&snapshot.len().to_string())
            || self.get("mid_blake3") != Some(&digest(blake3::hash(snapshot).as_bytes()))
        {
            return Err(format!("{MID_FILE} is not the one {GOLDEN_PATH} records"));
        }
        let (end, rt) = resume_to_end(fixture, snapshot)?;
        let state = rt.state_digest().map_err(|e| e.to_string())?;
        if self.get("final_snapshot_size") != Some(&end.len().to_string())
            || self.get("final_snapshot_blake3") != Some(&digest(blake3::hash(&end).as_bytes()))
            || self.get("state") != Some(&digest(&state))
        {
            return Err(format!(
                "{MID_FILE} resumed to the end does not reach the golden final state"
            ));
        }
        Ok(())
    }

    /// `cargo xtask m5-golden verify`: reruns the workload and requires `json` back byte
    /// for byte, `snapshot` to be the regenerated mid snapshot, and it to resume to the
    /// golden end.
    pub fn verify(root: &Path, json: &str, snapshot: &[u8]) -> Result<(), Vec<String>> {
        let (golden, mid) = Golden::generate(root).map_err(|e| vec![e])?;
        let mut errors = Vec::new();
        if golden.render() != json {
            errors.extend(describe_changes(Some(json), &golden));
            errors.push(format!("{GOLDEN_PATH} differs from this run"));
        }
        if mid != snapshot {
            errors.push(format!("{MID_SNAPSHOT_PATH} differs from this run's"));
        }
        let fixture = Fixture::read(root).map_err(|e| vec![e])?;
        if let Err(e) = golden.check_mid(&fixture, snapshot) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `cargo xtask m5-golden check`: another machine's emitted files must equal the
    /// committed ones and this machine's, and its snapshot must resume here to the golden
    /// end.
    pub fn check_foreign(
        root: &Path,
        (json, snapshot): (&str, &[u8]),
        (foreign_json, foreign_snapshot): (&str, &[u8]),
    ) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if foreign_json != json {
            errors.extend(describe_changes(Some(json), &parse_fields(foreign_json)?));
            errors.push(format!(
                "the foreign golden file differs from {GOLDEN_PATH}"
            ));
        }
        if foreign_snapshot != snapshot {
            errors.push(format!(
                "the foreign {MID_FILE} differs from {MID_SNAPSHOT_PATH}"
            ));
        }
        if let Err(mut e) = Golden::verify(root, foreign_json, foreign_snapshot) {
            errors.append(&mut e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// A golden file's fields, for [`describe_changes`].
fn parse_fields(json: &str) -> Result<Golden, Vec<String>> {
    let map: Map<String, Value> =
        serde_json::from_str(json).map_err(|e| vec![format!("not a golden file: {e}")])?;
    Ok(Golden {
        fields: map.into_iter().map(|(k, v)| (k, v.to_string())).collect(),
    })
}

/// What differs between the golden file `old` and `new`, field by field.
pub fn describe_changes(old: Option<&str>, new: &Golden) -> Vec<String> {
    let Some(old) = old else {
        return vec![format!("+ {GOLDEN_PATH}: new")];
    };
    let Ok(old): Result<Map<String, Value>, _> = serde_json::from_str(old) else {
        return vec![format!("~ {GOLDEN_PATH}: unreadable before")];
    };
    let mut changes = Vec::new();
    for (k, v) in &new.fields {
        let after: Value = serde_json::from_str(v).unwrap_or(Value::Null);
        match old.get(k) {
            None => changes.push(format!("+ {k}: {v}")),
            Some(before) if *before != after => changes.push(format!("~ {k}: {before} -> {v}")),
            Some(_) => {}
        }
    }
    for k in old.keys() {
        if new.get(k).is_none() {
            changes.push(format!("- {k}"));
        }
    }
    changes
}

/// The events of an untraced run of the workload, which must end without a fault.
pub fn reference_events(fixture: &Fixture) -> Result<usize, String> {
    let run = fixture.run(Start::Init { traced: false });
    if run.snapshot.is_none() {
        return Err(format!("the run ended with {}", run.finished.outcome.end));
    }
    usize::try_from(run.finished.outcome.events).map_err(|e| e.to_string())
}

/// The events of an untraced run of the workload on the platform built under `config`,
/// which must end without a fault within the event budget.
pub fn events_under(fixture: &Fixture, config: &Config) -> Result<usize, String> {
    let mut rt = fixture.platform_with(config)?;
    rt.init().map_err(|e| e.to_string())?;
    let mut events = 0usize;
    while rt.step().map_err(|e| e.to_string())?.is_some() {
        events += 1;
        if events as u64 > EVENT_BUDGET {
            return Err(format!("no end within the budget of {EVENT_BUDGET} events"));
        }
    }
    match rt.fault() {
        Some(e) => Err(format!("the run faulted: {e}")),
        None => Ok(events),
    }
}

/// The outcome of [`every_event`].
#[derive(Clone, Debug, Default)]
pub struct Sweep {
    /// Checkpoints resumed.
    pub checked: usize,
    /// Every failure, as `(k, why)`.
    pub failures: Vec<(usize, String)>,
}

/// Resumes checkpoints `range` of a producer run, each into a freshly built platform:
/// the restore must snapshot back to the checkpoint, then dispatch the producer's next
/// event and reach its next checkpoint byte for byte. The last checkpoint, after the last
/// event, must dispatch nothing.
fn sweep_range(fixture: &Fixture, config: &Config, range: std::ops::Range<usize>) -> Sweep {
    let mut sweep = Sweep::default();
    let fail = |sweep: &mut Sweep, k, e: String| sweep.failures.push((k, e));
    let mut producer = match fixture.platform_with(config) {
        Ok(rt) => rt,
        Err(e) => {
            fail(&mut sweep, range.start, e);
            return sweep;
        }
    };
    if let Err(e) = producer.init() {
        fail(&mut sweep, range.start, e.to_string());
        return sweep;
    }
    for k in 0..range.start {
        match producer.step() {
            Ok(Some(_)) => {}
            Ok(None) => return sweep,
            Err(e) => {
                fail(&mut sweep, k, e.to_string());
                return sweep;
            }
        }
    }
    let mut checkpoint = match producer.snapshot() {
        Ok(s) => s,
        Err(e) => {
            fail(&mut sweep, range.start, e.to_string());
            return sweep;
        }
    };
    for k in range {
        let expected = match producer.step() {
            Ok(ev) => ev,
            Err(e) => {
                fail(&mut sweep, k, format!("the producer failed: {e}"));
                return sweep;
            }
        };
        sweep.checked += 1;
        let next = producer.snapshot().map_err(|e| e.to_string());
        let result = next.clone().and_then(|next| {
            let mut rt = fixture.platform_with(config)?;
            rt.restore(&checkpoint)
                .map_err(|e| format!("restore failed: {e}"))?;
            if rt.snapshot().map_err(|e| e.to_string())? != checkpoint {
                return Err("snapshot -> restore -> snapshot is not the identity".to_owned());
            }
            let ev = rt.step().map_err(|e| e.to_string())?;
            if ev != expected {
                return Err("the resumed event differs".to_owned());
            }
            if rt.snapshot().map_err(|e| e.to_string())? != next {
                return Err("the resumed run reaches another next checkpoint".to_owned());
            }
            Ok(())
        });
        if let Err(e) = result {
            fail(&mut sweep, k, e);
        }
        match (expected, next) {
            (Some(_), Ok(next)) => checkpoint = next,
            _ => return sweep,
        }
    }
    sweep
}

/// Checkpoints `range` split into at most `parts` contiguous, non-empty ranges of equal
/// length, each checkpoint costing the same.
pub fn split(range: std::ops::Range<usize>, parts: usize) -> Vec<std::ops::Range<usize>> {
    let parts = parts.clamp(1, range.len().max(1));
    let (len, extra) = (range.len() / parts, range.len() % parts);
    let mut start = range.start;
    (0..parts)
        .map(|i| {
            let end = start + len + usize::from(i < extra);
            let part = start..end;
            start = end;
            part
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// Every checkpoint `k` in `range` (at most 0 through the last event) restored into a
/// fresh platform and run to checkpoint `k + 1`, split across `threads` threads.
pub fn every_event(fixture: &Fixture, range: std::ops::Range<usize>, threads: usize) -> Sweep {
    every_event_under(fixture, &Config::frozen(), range, threads)
}

/// [`every_event`] on the platform built under `config`, a perturbed one among them
/// (`docs/m5-design.md` §19).
pub fn every_event_under(
    fixture: &Fixture,
    config: &Config,
    range: std::ops::Range<usize>,
    threads: usize,
) -> Sweep {
    let mut sweep = Sweep::default();
    thread::scope(|s| {
        let handles: Vec<_> = split(range, threads)
            .into_iter()
            .map(|part| s.spawn(move || sweep_range(fixture, config, part)))
            .collect();
        for h in handles {
            let part = h.join().expect("a checkpoint thread panicked");
            sweep.checked += part.checked;
            sweep.failures.extend(part.failures);
        }
    });
    sweep.failures.sort_by_key(|f| f.0);
    sweep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_covers_the_range_in_order() {
        assert_eq!(split(0..10, 3), vec![0..4, 4..7, 7..10]);
        assert_eq!(split(5..7, 4), vec![5..6, 6..7]);
        assert_eq!(split(3..3, 2), Vec::<std::ops::Range<usize>>::new());
    }

    #[test]
    fn rendering_parses_back_and_changes_are_described() {
        let golden = |fields: [(&str, String); 2]| Golden {
            fields: fields.map(|(k, v)| (k.to_owned(), v)).to_vec(),
        };
        let one = golden([("events", "7".to_owned()), ("scenario", q(SCENARIO))]);
        let text = one.render();
        assert_eq!(parse_fields(&text).unwrap(), one);
        assert!(describe_changes(Some(&text), &one).is_empty());
        let changed = golden([("scenario", q(SCENARIO)), ("instret", "8".to_owned())]);
        assert_eq!(
            describe_changes(Some(&text), &changed),
            vec!["+ instret: 8".to_owned(), "- events".to_owned()]
        );
    }
}
