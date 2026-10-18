//! M4 reproducer bundles of `m3-reference` (`docs/m4-design.md` §16, §17.2, §28.2): the
//! unmodified sides' agreement and SP-1 to CB-2 (§25.1), each minimized with a checkpoint
//! about every 10,000 events. Every bundle is a pure function of the committed fixtures,
//! so another machine's must equal this machine's byte for byte and replay here.

use std::path::Path;

use systemscope_contracts::trace::Value;
use systemscope_rv32::spike_adapter::injective_rs1;
use systemscope_verify::backend::Backend;
use systemscope_verify::bundle::{Bundle, Case, read_dir, replay, write_dir};
use systemscope_verify::case::{InputVariant, MediaPatch, StatePatch};
use systemscope_verify::error::EngineError;
use systemscope_verify::minimize::minimize;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{Plan, plan};
use systemscope_verify::report::Builder;
use systemscope_verify::runtime::RuntimeBackend;

use super::Fixture;
use super::verify::{self, CPU_PATH};

/// The directory suffix of a bundle.
pub const SUFFIX: &str = ".repro";

/// The bundles' names, in emission order.
pub const NAMES: [&str; 6] = ["agree", "sp1", "sp2", "sp3", "cb1", "cb2"];

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

/// The unmodified side's whole `arch` stream, with the backend that recorded it.
struct Reference {
    backend: RuntimeBackend,
    arch: Vec<Observation>,
}

fn reference(fixture: &Fixture) -> Result<Reference, String> {
    let mut backend = verify::backend(fixture);
    let e = |e: EngineError| e.to_string();
    let scenario = verify::scenario(fixture);
    backend
        .start(
            &scenario,
            &InputVariant::None,
            &verify::config(),
            &verify::profile(),
        )
        .map_err(e)?;
    let mut arch = Vec::new();
    while let Some(o) = backend.next("arch").map_err(e)? {
        arch.push(o);
    }
    Ok(Reference { backend, arch })
}

impl Reference {
    /// The event that emits boundary `b`.
    fn event(&self, b: usize) -> Result<u64, String> {
        self.backend
            .event_index("arch", b as u64)
            .ok_or_else(|| format!("no event at boundary {b}"))
    }

    fn u32(&self, b: usize, field: &str) -> Result<u32, String> {
        let o = self.arch.get(b).ok_or_else(|| format!("no boundary {b}"))?;
        u32_of(o.field(field)).map_err(|e| format!("arch[{b}].{field}: {e}"))
    }

    fn is(&self, b: usize, field: &str, value: Value) -> bool {
        self.arch[b].field(field) == Some(&ObservedValue::Present(value))
    }

    /// A checkpoint about every 10,000 events, as the same-engine report takes.
    fn every(&self) -> Result<u64, String> {
        let n = self.arch.len();
        Ok(10_000 / (self.event(n - 1)? / n as u64))
    }
}

/// Patches the CPU's `field` from `old` to `new` after event `event_index`.
fn cpu_patch(event_index: u64, field: &str, old: u32, new: u32) -> InputVariant {
    InputVariant::State(StatePatch {
        event_index,
        field: format!("{CPU_PATH}/{field}"),
        old: Value::U64(old.into()),
        new: Value::U64(new.into()),
    })
}

/// SP-1: `pc ^= 0x4` after boundary 0, where the checkpoint's pc is the next
/// retirement's.
fn sp1(r: &Reference) -> Result<InputVariant, String> {
    let pc = r.u32(1, "pc")?;
    Ok(cpu_patch(r.event(0)?, "pc", pc, pc ^ 0x4))
}

/// SP-2: `x[rs1] ^= 1` before `r`, the first U-mode retirement of an injective
/// instruction. Reset leaves the registers 0, and only retirements write them.
fn sp2(r: &Reference) -> Result<InputVariant, String> {
    let mut x = [0u32; 32];
    for (i, o) in r.arch.iter().enumerate() {
        if o.kind != "retire" {
            continue;
        }
        if r.is(i, "priv", Value::U64(0))
            && let Some(rs1) = injective_rs1(r.u32(i, "insn")?)
        {
            let old = x[rs1 as usize];
            return Ok(cpu_patch(r.event(i - 1)?, &format!("x{rs1}"), old, old ^ 1));
        }
        if let Some(ObservedValue::Present(Value::U64(rd))) = o.field("rd") {
            x[*rd as usize] = r.u32(i, "rd_value")?;
        }
    }
    Err("no U-mode injective instruction".to_owned())
}

/// SP-3: `stvec ^= 0x4` before `e`, the first `EnvironmentCallFromU`, where `arch[e + 1]`
/// is the handler's first instruction.
fn sp3(r: &Reference) -> Result<InputVariant, String> {
    let ecall = Value::Str("EnvironmentCallFromU".into());
    let e = (0..r.arch.len())
        .find(|&i| r.arch[i].kind == "exception" && r.is(i, "cause", ecall.clone()))
        .ok_or("no U-mode ecall")?;
    let stvec = r.u32(e + 1, "pc")?;
    Ok(cpu_patch(r.event(e - 1)?, "stvec", stvec, stvec ^ 0x4))
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|w| u32::from_le_bytes(w.try_into().expect("four bytes")))
        .ok_or_else(|| format!("no word at {at:#x}"))
}

/// `hello`'s entry word, entry 0 of the executable table (`docs/m3-design.md` §8.1),
/// replaced by `new` of the word it holds.
fn code_patch(fixture: &Fixture, new: impl Fn(u32) -> u32) -> Result<InputVariant, String> {
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
    Ok(InputVariant::Media(MediaPatch {
        offset: at as u64,
        old: old.to_le_bytes().to_vec(),
        new: new(old).to_le_bytes().to_vec(),
    }))
}

/// CB-1: the entry word becomes `addi x0, x0, 0`, or `addi x0, x0, 1` if it already is
/// that.
fn cb1(fixture: &Fixture) -> Result<InputVariant, String> {
    code_patch(fixture, |old| {
        if old == 0x0000_0013 {
            0x0010_0013
        } else {
            0x0000_0013
        }
    })
}

/// CB-2: the entry word becomes 0, an illegal instruction.
fn cb2(fixture: &Fixture) -> Result<InputVariant, String> {
    code_patch(fixture, |_| 0)
}

/// Every bundle, by name, in [`NAMES`] order.
pub fn bundles(fixture: &Fixture) -> Result<Vec<(&'static str, Bundle)>, String> {
    let plan = the_plan(fixture)?;
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let sides = Builder::same(&make, verify::config());
    let r = reference(fixture)?;
    let every = r.every()?;
    let inputs = [
        InputVariant::None,
        sp1(&r)?,
        sp2(&r)?,
        sp3(&r)?,
        cb1(fixture)?,
        cb2(fixture)?,
    ];
    NAMES
        .into_iter()
        .zip(inputs)
        .map(|(name, input)| {
            let inputs = [InputVariant::None, input];
            minimize(&plan, &scenario, sides, inputs, every)
                .map(|m| (name, m.bundle))
                .map_err(|e| format!("{name}: {e}"))
        })
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

/// Replays `b` here.
fn replayed(fixture: &Fixture, b: &Bundle) -> Result<(), String> {
    let plan = the_plan(fixture)?;
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let case = Case {
        plan: &plan,
        scenario: &scenario,
        sides: Builder::same(&make, verify::config()),
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
