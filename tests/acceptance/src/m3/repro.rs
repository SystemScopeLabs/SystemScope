//! M4 reproducer bundles of `m3-reference` (`docs/m4-design.md` §16, §28.2): the
//! unmodified sides' agreement, SP-1, and CB-1, from the initial states, and CB-1 also
//! from both sides' checkpoints at its divergence minus the report window. Every bundle
//! is a pure function of the committed fixtures, so another machine's must equal this
//! machine's byte for byte and replay here.

use std::path::Path;

use systemscope_contracts::trace::Value;
use systemscope_verify::backend::Backend;
use systemscope_verify::bundle::{self, Bundle, Case, Manifest, read_dir, replay, write_dir};
use systemscope_verify::case::{InputVariant, MediaPatch, StatePatch};
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{Plan, plan};

use super::Fixture;
use super::verify::{self, CPU_PATH};

/// The directory suffix of a bundle.
pub const SUFFIX: &str = ".repro";

/// The bundles' names, in emission order.
pub const NAMES: [&str; 4] = ["agree", "sp1", "cb1", "cb1-checkpoint"];

fn the_plan(fixture: &Fixture) -> Result<Plan, String> {
    let b = verify::backend(fixture);
    plan(&verify::profile(), &b.support(), &b.support()).map_err(|e| format!("{e:?}"))
}

fn u32_of(value: Option<&ObservedValue>) -> Result<u32, String> {
    match value {
        Some(ObservedValue::Present(Value::U64(v))) => {
            u32::try_from(*v).map_err(|_| format!("{v:#x} is not 32-bit"))
        }
        other => Err(format!("not a 32-bit value: {other:?}")),
    }
}

/// SP-1: `pc ^= 0x4` after boundary 0, where the checkpoint's pc is the next
/// retirement's.
fn sp1(fixture: &Fixture) -> Result<InputVariant, String> {
    let mut b = verify::backend(fixture);
    let e = |e: systemscope_verify::error::EngineError| e.to_string();
    let scenario = verify::scenario(fixture);
    b.start(
        &scenario,
        &InputVariant::None,
        &verify::config(),
        &verify::profile(),
    )
    .map_err(e)?;
    b.next("arch").map_err(e)?.ok_or("no boundary 0")?;
    let next = b.next("arch").map_err(e)?.ok_or("no boundary 1")?;
    let pc = u32_of(next.field("pc"))?;
    let event_index = b.event_index("arch", 0).ok_or("no event at boundary 0")?;
    Ok(InputVariant::State(StatePatch {
        event_index,
        field: format!("{CPU_PATH}/pc"),
        old: Value::U64(pc.into()),
        new: Value::U64((pc ^ 0x4).into()),
    }))
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|w| u32::from_le_bytes(w.try_into().expect("four bytes")))
        .ok_or_else(|| format!("no word at {at:#x}"))
}

/// CB-1: `hello`'s entry word, entry 0 of the executable table, becomes `addi x0, x0, 0`,
/// or `addi x0, x0, 1` if it already is that (`docs/m3-design.md` §8.1).
fn cb1(fixture: &Fixture) -> Result<InputVariant, String> {
    let disk = &fixture.disk;
    if le32(disk, 0)? != 0x3058_5353 {
        return Err("the disk is not SSX0".to_owned());
    }
    let (lba, len) = (le32(disk, 0x10)? as usize, le32(disk, 0x14)? as usize);
    let elf = disk
        .get(lba * 512..lba * 512 + len)
        .ok_or("hello is off the disk")?;
    let le16 = |at: usize| le32(elf, at).map(|w| (w & 0xFFFF) as usize);
    let entry = le32(elf, 0x18)?;
    let (phoff, size, count) = (le32(elf, 0x1C)? as usize, le16(0x2A)?, le16(0x2C)?);
    let mut offset = None;
    for i in 0..count {
        let ph = phoff + i * size;
        let (kind, at, vaddr, filesz) = (
            le32(elf, ph)?,
            le32(elf, ph + 4)?,
            le32(elf, ph + 8)?,
            le32(elf, ph + 16)?,
        );
        if kind == 1 && entry >= vaddr && entry + 4 <= vaddr + filesz {
            offset = Some(at + entry - vaddr);
            break;
        }
    }
    let at = lba * 512 + offset.ok_or("e_entry is outside every PT_LOAD segment")? as usize;
    let old = le32(disk, at)?;
    let new: u32 = if old == 0x0000_0013 {
        0x0010_0013
    } else {
        0x0000_0013
    };
    Ok(InputVariant::Media(MediaPatch {
        offset: at as u64,
        old: old.to_le_bytes().to_vec(),
        new: new.to_le_bytes().to_vec(),
    }))
}

/// A side's checkpoint after it emits `boundaries` primary observations.
fn checkpoint_after(
    fixture: &Fixture,
    input: &InputVariant,
    boundaries: u64,
) -> Result<Checkpoint, String> {
    let mut b = verify::backend(fixture);
    let scenario = verify::scenario(fixture);
    let e = |e: systemscope_verify::error::EngineError| e.to_string();
    b.start(&scenario, input, &verify::config(), &verify::profile())
        .map_err(e)?;
    for _ in 0..boundaries {
        b.next("arch").map_err(e)?.ok_or("the stream ended")?;
    }
    b.checkpoint().map_err(e)
}

/// Every bundle, by name, in [`NAMES`] order.
pub fn bundles(fixture: &Fixture) -> Result<Vec<(&'static str, Bundle)>, String> {
    let plan = the_plan(fixture)?;
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let write = |input: InputVariant, origins: [Option<&Checkpoint>; 2]| {
        let inputs = [InputVariant::None, input];
        bundle::write(&plan, &scenario, &make, inputs, verify::config(), origins)
            .map_err(|e| e.to_string())
    };
    let cb1 = cb1(fixture)?;
    let initial = write(cb1.clone(), [None, None])?;
    let m = Manifest::parse(&initial.entries[bundle::MANIFEST])?;
    let d = m.primary_divergence.ok_or("CB-1 does not diverge")?.ordinal;
    let c = d.saturating_sub(u64::from(plan.profile.report_window));
    let ke = checkpoint_after(fixture, &InputVariant::None, c)?;
    let ka = checkpoint_after(fixture, &cb1, c)?;
    Ok(vec![
        ("agree", write(InputVariant::None, [None, None])?),
        ("sp1", write(sp1(fixture)?, [None, None])?),
        ("cb1", initial),
        ("cb1-checkpoint", write(cb1, [Some(&ke), Some(&ka)])?),
    ])
}

/// Writes every bundle into `dir`, as `<name>.repro`.
pub fn emit(fixture: &Fixture, dir: &Path) -> Result<(), String> {
    for (name, b) in bundles(fixture)? {
        let at = dir.join(format!("{name}{SUFFIX}"));
        write_dir(&b, &at).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    Ok(())
}

/// Replays `b` here.
fn replayed(fixture: &Fixture, b: &Bundle) -> Result<(), String> {
    let plan = the_plan(fixture)?;
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let case = Case {
        plan: &plan,
        scenario: &scenario,
        config: verify::config(),
        make: &make,
    };
    replay(b, case).map(|_| ()).map_err(|e| e.to_string())
}

/// Requires another machine's bundles in `dir` to equal this machine's byte for byte,
/// entry by entry, and to replay here.
pub fn check(fixture: &Fixture, dir: &Path) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    for (name, own) in bundles(fixture).map_err(|e| vec![e])? {
        let at = dir.join(format!("{name}{SUFFIX}"));
        let foreign = match read_dir(&at) {
            Ok(b) => b,
            Err(e) => {
                errors.push(format!("{}: {e}", at.display()));
                continue;
            }
        };
        let paths = own.entries.keys().chain(foreign.entries.keys());
        for path in paths.collect::<std::collections::BTreeSet<_>>() {
            if own.entries.get(path) != foreign.entries.get(path) {
                errors.push(format!("{name}: {path} differs from this machine's"));
            }
        }
        if let Err(e) = replayed(fixture, &foreign) {
            errors.push(format!("{name}: {e}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Replays every `*.repro` bundle in `dir` here.
pub fn replay_all(fixture: &Fixture, dir: &Path) -> Result<usize, Vec<String>> {
    let entries = std::fs::read_dir(dir).map_err(|e| vec![format!("{}: {e}", dir.display())])?;
    let mut dirs: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == &SUFFIX[1..]))
        .collect();
    dirs.sort();
    let mut errors = Vec::new();
    for at in &dirs {
        if let Err(e) = read_dir(at)
            .map_err(|e| e.to_string())
            .and_then(|b| replayed(fixture, &b))
        {
            errors.push(format!("{}: {e}", at.display()));
        }
    }
    match (dirs.len(), errors.is_empty()) {
        (0, _) => Err(vec![format!("no {SUFFIX} bundle in {}", dir.display())]),
        (n, true) => Ok(n),
        _ => Err(errors),
    }
}
