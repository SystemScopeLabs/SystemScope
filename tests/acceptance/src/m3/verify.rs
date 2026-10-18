//! `m3-reference` under the M4 verification engine (`docs/m4-design.md` §6.4, §22,
//! §25.1): the SystemScope ↔ SystemScope profile, its `effect` and `os` projections next
//! to the `rv32.*` one, the runtime backend, and the state patches of SP-1 to SP-3.

use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_rv32::m3;
use systemscope_rv32::m3ref::{self, Config};
use systemscope_rv32::projection::{self, HALT_TRAP, Rv32Projection};
use systemscope_verify::case::{BackendId, BackendRunConfig, Scenario, StatePatch};
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Requirement, StreamSpec, Support,
};
use systemscope_verify::runtime::{Projected, Projection, RuntimeBackend};

use super::Fixture;
use super::portable::Platform;

/// The CPU's path, which names the patchable fields: `soc.cpu0/pc`, `soc.cpu0/x1` to
/// `soc.cpu0/x31`, and `soc.cpu0/stvec`.
pub const CPU_PATH: &str = m3ref::PATHS[0];

/// Each `effect` and `os` boundary: its record kind, stream, boundary kind, and fields.
const RECORDS: [(&str, &str, &str, &[&str]); 9] = [
    ("platform.uart.tx", "effect", "uart_tx", &["byte"]),
    (
        "platform.disk.write",
        "effect",
        "disk_write",
        &["lba", "outcome"],
    ),
    ("os.shutdown", "effect", "shutdown", &["reason", "detail"]),
    (
        "os.syscall.enter",
        "os",
        "syscall_enter",
        &["pid", "nr", "a0", "a1", "a2"],
    ),
    (
        "os.syscall.exit",
        "os",
        "syscall_exit",
        &["pid", "nr", "ret"],
    ),
    ("os.process.switch", "os", "switch", &["from", "to"]),
    (
        "os.process.create",
        "os",
        "create",
        &["pid", "entry", "root", "error"],
    ),
    ("os.process.exit", "os", "exit", &["pid", "status"]),
    (
        "os.process.fault",
        "os",
        "fault",
        &["pid", "cause", "epc", "tval"],
    ),
];

/// The `m3-reference` SystemScope ↔ SystemScope profile, version 1: primary `arch`,
/// auxiliary `effect` and `os`, every field required, ending at the halting trap.
pub fn profile() -> ComparisonProfile {
    let stream = |name: &str| {
        let mut kinds: Vec<KindSpec> = RECORDS
            .iter()
            .filter(|r| r.1 == name)
            .map(|&(_, _, kind, fields)| KindSpec {
                kind: kind.to_owned(),
                fields: fields
                    .iter()
                    .map(|f| FieldSpec {
                        name: (*f).to_owned(),
                        requirement: Requirement::Required,
                        may_be_absent: false,
                    })
                    .collect(),
                excluded: Vec::new(),
            })
            .collect();
        kinds.sort_by(|a, b| a.kind.cmp(&b.kind));
        StreamSpec {
            stream: name.to_owned(),
            kinds,
            window: String::new(),
        }
    };
    ComparisonProfile {
        name: "m3-reference".to_owned(),
        version: 1,
        primary: projection::arch_m3(),
        auxiliary: vec![stream("effect"), stream("os")],
        stop_rule: HALT_TRAP.to_owned(),
        report_window: 3,
    }
}

/// `rv32.*` records through [`Rv32Projection`]; `effect` and `os` records copied field
/// by field, which must be exactly the fields [`RECORDS`] names.
#[derive(Default)]
pub struct M3Projection {
    rv32: Rv32Projection,
}

impl Projection for M3Projection {
    fn support(&self) -> Support {
        let mut s = Support::default();
        s.streams
            .extend(["arch", "effect", "os"].map(str::to_owned));
        s.fields.extend(projection::support());
        for (_, stream, kind, fields) in RECORDS {
            s.fields
                .extend(fields.iter().map(|f| format!("{stream}/{kind}/{f}")));
        }
        s
    }

    fn begin(&mut self, profile: &ComparisonProfile) -> Result<(), String> {
        self.rv32.begin(profile)
    }

    fn project(&mut self, record: &TraceRecord, out: &mut Vec<Projected>) -> Result<(), String> {
        let Some(&(_, stream, kind, fields)) = RECORDS.iter().find(|r| r.0 == record.kind) else {
            return self.rv32.project(record, out);
        };
        let names: Vec<&str> = record.fields.iter().map(|(n, _)| *n).collect();
        if names != fields {
            return Err(format!("{}: unexpected fields {names:?}", record.kind));
        }
        out.push(Projected {
            stream: stream.to_owned(),
            kind: kind.to_owned(),
            fields: record
                .fields
                .iter()
                .map(|(n, v)| ((*n).to_owned(), ObservedValue::Present(v.clone())))
                .collect(),
        });
        Ok(())
    }

    fn finished(&self) -> bool {
        self.rv32.finished()
    }
}

/// The scenario of `fixture`.
pub fn scenario(fixture: &Fixture) -> Scenario {
    Scenario {
        platform: super::SCENARIO.to_owned(),
        topology_hash: fixture.platform().topology_hash(),
        cpu_profile: "M3".to_owned(),
        program: fixture.disk_blake3,
        locator: fixture.scenario.disk_file().to_owned(),
        seed: Config::frozen().seed,
    }
}

/// The canonical run configuration: the M3 event watchdog.
pub fn config() -> BackendRunConfig {
    BackendRunConfig {
        budget: m3::EVENT_BUDGET,
    }
}

/// A SystemScope backend on the frozen `m3-reference` with `fixture`'s disk, able to
/// apply state patches to the CPU.
pub fn backend(fixture: &Fixture) -> RuntimeBackend {
    let fixture = fixture.clone();
    RuntimeBackend::new(
        BackendId {
            name: "systemscope".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: super::SCENARIO.to_owned(),
        },
        Box::new(move |_| Ok(fixture.platform())),
        Box::new(|| Box::new(M3Projection::default())),
    )
    .with_patch(Box::new(patch))
}

/// Patches one 32-bit CPU field of `m3-reference` snapshot bytes, found by the portable
/// decoder, after checking its old value.
pub fn patch(bytes: &mut [u8], p: &StatePatch) -> Result<(), String> {
    let field = p
        .field
        .strip_prefix(CPU_PATH)
        .and_then(|f| f.strip_prefix('/'))
        .ok_or_else(|| format!("{} is not a CPU field", p.field))?;
    let patchable = field == "pc"
        || field == "stvec"
        || field
            .strip_prefix('x')
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| (1..32).contains(&n));
    if !patchable {
        return Err(format!("{} is not patchable", p.field));
    }
    let word = |v: &Value| match v {
        Value::U64(v) => u32::try_from(*v).map_err(|_| format!("{v:#x} is not 32 bits")),
        other => Err(format!("{other:?} is not a U64")),
    };
    let (old, new) = (word(&p.old)?, word(&p.new)?);
    let at = Platform::decode(bytes)?
        .offset(&format!("cpu.{field}"))
        .ok_or_else(|| format!("the decoder marks no cpu.{field}"))?;
    let held = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    if held != old {
        return Err(format!("{} is {held:#x}, not {old:#x}", p.field));
    }
    bytes[at..at + 4].copy_from_slice(&new.to_le_bytes());
    Ok(())
}
