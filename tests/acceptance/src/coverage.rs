//! Architectural coverage of an `arch` stream (`docs/m5-design.md` §17): what a workload
//! exercised, read from its observations alone. It is a report, never a gate.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use systemscope_contracts::trace::Value;
use systemscope_verify::observation::{Observation, ObservedValue};

/// The text of a field: a number in decimal, a string as it is.
fn text(o: &Observation, name: &str) -> Option<String> {
    match o.field(name)? {
        ObservedValue::Present(Value::U64(v)) => Some(v.to_string()),
        ObservedValue::Present(Value::I64(v)) => Some(v.to_string()),
        ObservedValue::Present(Value::Bool(v)) => Some(v.to_string()),
        ObservedValue::Present(Value::Str(s)) => Some(s.clone()),
        ObservedValue::Present(Value::Bytes(b)) => Some(format!("{b:02x?}")),
        ObservedValue::Absent => None,
    }
}

fn number(o: &Observation, name: &str) -> Option<u64> {
    match o.field(name)? {
        ObservedValue::Present(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

/// A privilege mode's letter, from its encoding or its name.
fn mode(text: &str) -> String {
    match text {
        "0" => "U".to_owned(),
        "1" => "S".to_owned(),
        "3" => "M".to_owned(),
        other => other.to_owned(),
    }
}

/// An interrupt cause's name, from its `mcause` or `scause` value or its name.
fn interrupt(text: &str) -> String {
    const INTERRUPT: u64 = 1 << 31;
    match text.parse::<u64>() {
        Ok(v) if v & INTERRUPT != 0 => match v & !INTERRUPT {
            1 => "SupervisorSoftware".to_owned(),
            3 => "MachineSoftware".to_owned(),
            5 => "SupervisorTimer".to_owned(),
            7 => "MachineTimer".to_owned(),
            9 => "SupervisorExternal".to_owned(),
            11 => "MachineExternal".to_owned(),
            code => format!("interrupt {code}"),
        },
        _ => text.to_owned(),
    }
}

/// The mnemonic of an RV32I, Zicsr, or privileged instruction word, or `unknown`.
pub fn mnemonic(insn: u32) -> &'static str {
    let f3 = (insn >> 12) & 7;
    let f7 = insn >> 25;
    match insn & 0x7f {
        0x37 => "lui",
        0x17 => "auipc",
        0x6f => "jal",
        0x67 => "jalr",
        0x63 => [
            "beq", "bne", "unknown", "unknown", "blt", "bge", "bltu", "bgeu",
        ][f3 as usize],
        0x03 => [
            "lb", "lh", "lw", "unknown", "lbu", "lhu", "unknown", "unknown",
        ][f3 as usize],
        0x23 => [
            "sb", "sh", "sw", "unknown", "unknown", "unknown", "unknown", "unknown",
        ][f3 as usize],
        0x13 => match (f3, f7) {
            (1, _) => "slli",
            (5, 0x20) => "srai",
            (5, _) => "srli",
            _ => ["addi", "", "slti", "sltiu", "xori", "", "ori", "andi"][f3 as usize],
        },
        0x33 => match (f3, f7) {
            (0, 0x20) => "sub",
            (5, 0x20) => "sra",
            _ => ["add", "sll", "slt", "sltu", "xor", "srl", "or", "and"][f3 as usize],
        },
        0x0f => "fence",
        0x73 => match (f3, insn) {
            (0, 0x0000_0073) => "ecall",
            (0, 0x0010_0073) => "ebreak",
            (0, 0x1020_0073) => "sret",
            (0, 0x3020_0073) => "mret",
            (0, 0x1050_0073) => "wfi",
            (0, _) if f7 == 0x09 => "sfence.vma",
            (0, _) => "unknown",
            _ => [
                "", "csrrw", "csrrs", "csrrc", "unknown", "csrrwi", "csrrsi", "csrrci",
            ][f3 as usize],
        },
        _ => "unknown",
    }
}

/// Counts, by key.
type Counts = BTreeMap<String, u64>;

fn bump(counts: &mut Counts, key: String) {
    *counts.entry(key).or_default() += 1;
}

/// The architectural coverage of `arch`, an `arch` stream: the instructions retired, by
/// mnemonic and by mode; the CSRs written; the exception and interrupt causes, each with
/// the mode it was taken in; and the Sv32 accesses, those a walk translated to another
/// address, and the page faults.
pub fn arch(arch: &[Observation]) -> Json {
    let (mut retired, mut by_mode, mut csrs) = (Counts::new(), Counts::new(), Counts::new());
    let (mut exceptions, mut interrupts, mut translated) =
        (Counts::new(), Counts::new(), Counts::new());
    let mut page_faults = Counts::new();
    for o in arch {
        match o.kind.as_str() {
            "retire" => {
                let insn = number(o, "insn").unwrap_or_default();
                bump(&mut retired, mnemonic(insn as u32).to_owned());
                if let Some(m) = text(o, "priv") {
                    bump(&mut by_mode, mode(&m));
                }
                if let Some(csr) = number(o, "csr") {
                    bump(&mut csrs, format!("{csr:#05x}"));
                }
                if let (Some(mem), Some(addr), Some(paddr)) =
                    (text(o, "mem"), number(o, "addr"), number(o, "paddr"))
                    && addr != paddr
                {
                    bump(&mut translated, mem);
                }
            }
            "exception" => {
                let cause = text(o, "cause").unwrap_or_default();
                let to = mode(&text(o, "to").unwrap_or_default());
                if cause.contains("PageFault") {
                    bump(&mut page_faults, cause.clone());
                }
                bump(&mut exceptions, format!("{cause} to {to}"));
            }
            "interrupt" => {
                let cause = interrupt(&text(o, "cause").unwrap_or_default());
                let to = mode(&text(o, "to").unwrap_or_default());
                bump(&mut interrupts, format!("{cause} to {to}"));
            }
            _ => {}
        }
    }
    json!({
        "retired": retired.values().sum::<u64>(),
        "instructions": retired,
        "modes": by_mode,
        "csrs_written": csrs,
        "exceptions": exceptions,
        "interrupts": interrupts,
        "sv32": {
            "translated": translated,
            "page_faults": page_faults,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(kind: &str, fields: &[(&str, Value)]) -> Observation {
        Observation {
            stream: "arch".to_owned(),
            ordinal: 0,
            kind: kind.to_owned(),
            fields: fields
                .iter()
                .map(|(n, v)| ((*n).to_owned(), ObservedValue::Present(v.clone())))
                .collect(),
        }
    }

    #[test]
    fn the_report_counts_each_boundary() {
        let s = |t: &str| Value::Str(t.to_owned());
        let stream = [
            obs(
                "retire",
                &[
                    ("insn", Value::U64(0x0000_2503)),
                    ("priv", Value::U64(0)),
                    ("mem", s("load")),
                    ("addr", Value::U64(0x1000)),
                    ("paddr", Value::U64(0x8000_1000)),
                ],
            ),
            obs(
                "retire",
                &[
                    ("insn", Value::U64(0x00a1_2023)),
                    ("priv", Value::U64(1)),
                    ("mem", s("store")),
                    ("addr", Value::U64(8)),
                    ("paddr", Value::U64(8)),
                ],
            ),
            obs(
                "retire",
                &[
                    ("insn", Value::U64(0x3000_1573)),
                    ("priv", Value::U64(3)),
                    ("csr", Value::U64(0x300)),
                ],
            ),
            obs(
                "exception",
                &[("cause", s("LoadPageFault")), ("to", s("S"))],
            ),
            obs(
                "interrupt",
                &[("cause", Value::U64(0x8000_0009)), ("to", s("S"))],
            ),
        ];
        assert_eq!(
            arch(&stream),
            json!({
                "retired": 3,
                "instructions": {"csrrw": 1, "lw": 1, "sw": 1},
                "modes": {"M": 1, "S": 1, "U": 1},
                "csrs_written": {"0x300": 1},
                "exceptions": {"LoadPageFault to S": 1},
                "interrupts": {"SupervisorExternal to S": 1},
                "sv32": {"translated": {"load": 1}, "page_faults": {"LoadPageFault": 1}},
            })
        );
    }

    #[test]
    fn mnemonics_cover_the_instruction_set() {
        for (word, name) in [
            (0x0000_0537, "lui"),
            (0x0000_0517, "auipc"),
            (0x0000_006f, "jal"),
            (0x0000_8067, "jalr"),
            (0x0000_0463, "beq"),
            (0x0000_7463, "bgeu"),
            (0x0000_2503, "lw"),
            (0x0000_4503, "lbu"),
            (0x00a1_2023, "sw"),
            (0x0010_0513, "addi"),
            (0x0015_1513, "slli"),
            (0x4015_5513, "srai"),
            (0x0015_5513, "srli"),
            (0x40b5_0533, "sub"),
            (0x00b5_0533, "add"),
            (0x00b5_7533, "and"),
            (0x0ff0_000f, "fence"),
            (0x0000_0073, "ecall"),
            (0x0010_0073, "ebreak"),
            (0x1020_0073, "sret"),
            (0x3020_0073, "mret"),
            (0x1050_0073, "wfi"),
            (0x1200_0073, "sfence.vma"),
            (0x3000_1573, "csrrw"),
            (0x1000_7573, "csrrci"),
            (0x0000_0000, "unknown"),
        ] {
            assert_eq!(mnemonic(word), name, "{word:#010x}");
        }
    }
}
