//! The `arch` projection of `rv32.*` trace records for the M4 verification engine
//! (`docs/m4-design.md` §6.3, §8.2, §8.4).
//!
//! `rv32.commit` is a `retire`, `rv32.exception` an `exception`, `rv32.interrupt` an
//! `interrupt`, and the halting `rv32.trap` the final `exception`, taken to M, which is
//! the `halt-trap` stop rule. Every other record is ignored; an `rv32.*` record out of
//! shape, the instruction limit, or a record after the trap is an error.

use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_rv32i::cpu::{COMMIT_KIND, EXCEPTION_KIND, HALT_KIND, INTERRUPT_KIND, TRAP_KIND};
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Requirement, StreamSpec,
};
use systemscope_verify::runtime::Projected;

/// The stop rule: the CPU halts on a trap taken in M.
pub const HALT_TRAP: &str = "halt-trap";

/// The `retire` fields, in observation order.
pub const RETIRE_FIELDS: [&str; 12] = [
    "pc",
    "insn",
    "priv",
    "rd",
    "rd_value",
    "mem",
    "addr",
    "width",
    "value",
    "paddr",
    "csr",
    "csr_value",
];

/// The `exception` fields, in observation order.
pub const EXCEPTION_FIELDS: [&str; 5] = ["pc", "insn", "cause", "tval", "to"];

/// The `interrupt` fields, in observation order.
pub const INTERRUPT_FIELDS: [&str; 3] = ["pc", "cause", "to"];

/// Fields that are `Absent` where they do not apply.
const MAY_BE_ABSENT: [&str; 9] = [
    "rd",
    "rd_value",
    "addr",
    "width",
    "value",
    "paddr",
    "csr",
    "csr_value",
    "priv",
];

/// Every `arch` field the projection reports, as `stream/kind/field`.
pub fn support() -> impl Iterator<Item = String> {
    let kinds = [
        ("retire", &RETIRE_FIELDS[..]),
        ("exception", &EXCEPTION_FIELDS[..]),
        ("interrupt", &INTERRUPT_FIELDS[..]),
    ];
    kinds
        .into_iter()
        .flat_map(|(k, fields)| fields.iter().map(move |f| format!("arch/{k}/{f}")))
}

/// The M3 `arch` stream: `retire` and `exception`, every field required. M3 code takes
/// no interrupt in `m3-reference` (§8.2), so `interrupt` is not compared.
pub fn arch_m3() -> StreamSpec {
    let spec = |name: &str| FieldSpec {
        name: name.to_owned(),
        requirement: Requirement::Required,
        may_be_absent: MAY_BE_ABSENT.contains(&name),
    };
    StreamSpec {
        stream: "arch".to_owned(),
        kinds: vec![
            KindSpec {
                kind: "exception".to_owned(),
                fields: EXCEPTION_FIELDS.map(spec).to_vec(),
                excluded: vec!["from".to_owned()],
            },
            KindSpec {
                kind: "retire".to_owned(),
                fields: RETIRE_FIELDS.map(spec).to_vec(),
                excluded: vec!["next_pc".to_owned()],
            },
        ],
        window: String::new(),
    }
}

/// Reads `rv32.*` records into `arch` boundaries.
#[derive(Clone, Debug, Default)]
pub struct Rv32Projection {
    halted: bool,
}

impl Rv32Projection {
    /// Accepts profiles whose `arch` stream starts at the first boundary and that stop at
    /// [`HALT_TRAP`].
    pub fn begin(&mut self, profile: &ComparisonProfile) -> Result<(), String> {
        if profile.stop_rule != HALT_TRAP {
            return Err(format!("stop rule {}", profile.stop_rule));
        }
        match profile.streams().find(|s| s.stream == "arch") {
            Some(s) if !s.window.is_empty() => Err(format!("window {}", s.window)),
            _ => Ok(()),
        }
    }

    /// Whether the halting trap has been seen.
    pub fn finished(&self) -> bool {
        self.halted
    }

    /// Projects one record; records of other components pass through untouched.
    pub fn project(
        &mut self,
        record: &TraceRecord,
        out: &mut Vec<Projected>,
    ) -> Result<(), String> {
        if !record.kind.starts_with("rv32.") {
            return Ok(());
        }
        let at = |why: &str| format!("{}: {why}", record.kind);
        if self.halted {
            return Err(at("after the halting trap"));
        }
        let get = |name: &str| {
            record
                .fields
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v)
        };
        let need = |name: &str| get(name).cloned().ok_or_else(|| at(&format!("no {name}")));
        let u64_of = |name: &str| match get(name) {
            Some(Value::U64(v)) => Ok(*v),
            _ => Err(at(&format!("{name} is not a U64"))),
        };
        let present = ObservedValue::Present;
        let opt = |name: &str| get(name).cloned().map_or(ObservedValue::Absent, present);
        let str_value = |s: &str| present(Value::Str(s.to_owned()));
        let names: Vec<&str> = record.fields.iter().map(|(n, _)| *n).collect();
        let (kind, fields) = match record.kind {
            k if k == COMMIT_KIND => {
                const KNOWN: [&str; 12] = [
                    "pc",
                    "insn",
                    "rd",
                    "rd_value",
                    "next_pc",
                    "priv",
                    "addr",
                    "paddr",
                    "width",
                    "value",
                    "csr",
                    "csr_value",
                ];
                if let Some(n) = names.iter().find(|n| !KNOWN.contains(n)) {
                    return Err(at(&format!("unexpected field {n}")));
                }
                let rd = u64_of("rd")?;
                let wrote = |v: ObservedValue| if rd == 0 { ObservedValue::Absent } else { v };
                let mem = match (get("addr"), get("width")) {
                    (None, None) => "none",
                    (Some(_), None) => "load",
                    (Some(_), Some(_)) => "store",
                    (None, Some(_)) => return Err(at("a width without an address")),
                };
                // A CSR instruction whose write is suppressed names its CSR only; only a
                // stored value is an architectural write.
                let (csr, csr_value) = match get("csr_value") {
                    Some(v) => (present(need("csr")?), present(v.clone())),
                    None => (ObservedValue::Absent, ObservedValue::Absent),
                };
                let fields = vec![
                    ("pc", present(need("pc")?)),
                    ("insn", present(need("insn")?)),
                    ("priv", opt("priv")),
                    ("rd", wrote(present(Value::U64(rd)))),
                    ("rd_value", wrote(present(need("rd_value")?))),
                    ("mem", str_value(mem)),
                    ("addr", opt("addr")),
                    ("width", opt("width")),
                    ("value", opt("value")),
                    ("paddr", opt("paddr")),
                    ("csr", csr),
                    ("csr_value", csr_value),
                ];
                ("retire", fields)
            }
            k if k == EXCEPTION_KIND => {
                if names != ["pc", "insn", "cause", "tval", "from", "to"] {
                    return Err(at(&format!("unexpected fields {names:?}")));
                }
                let fields = vec![
                    ("pc", present(need("pc")?)),
                    ("insn", present(need("insn")?)),
                    ("cause", present(need("cause")?)),
                    ("tval", present(need("tval")?)),
                    ("to", present(need("to")?)),
                ];
                ("exception", fields)
            }
            k if k == TRAP_KIND => {
                if names != ["pc", "insn", "cause", "tval"] {
                    return Err(at(&format!("unexpected fields {names:?}")));
                }
                self.halted = true;
                let fields = vec![
                    ("pc", present(need("pc")?)),
                    ("insn", present(need("insn")?)),
                    ("cause", present(need("cause")?)),
                    ("tval", present(need("tval")?)),
                    ("to", str_value("M")),
                ];
                ("exception", fields)
            }
            k if k == INTERRUPT_KIND => {
                let fields = vec![
                    ("pc", present(need("mepc")?)),
                    ("cause", present(need("mcause")?)),
                    ("to", str_value("M")),
                ];
                ("interrupt", fields)
            }
            k if k == HALT_KIND => return Err(at("the instruction limit")),
            _ => return Err(at("unknown CPU record kind")),
        };
        out.push(Projected {
            stream: "arch".to_owned(),
            kind: kind.to_owned(),
            fields: fields.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
        });
        Ok(())
    }
}
