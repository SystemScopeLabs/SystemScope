//! Source-mutant validation (`docs/m4-design.md` §25.2): the frozen targets, the
//! cargo-mutants run of each against the source-mutant case
//! ([`super::expected::judge`]), and the summary of its outcomes.
//!
//! Every generated mutant must be detected: `Diverged` (the locator equal to the naive
//! oracle), `AuxiliaryDivergence`, `BackendFailed`, or a crash or hang of the mutated
//! side before the stop rule. A missed mutant must be listed in [`EQUIVALENTS`] with the
//! reason it is equivalent, and every listed mutant must be missed.

use std::fs;
use std::path::Path;

use serde_json::Value as Json;

use super::expected::{DETECTED, ORACLE_MISMATCH};

/// The equivalent mutants: blocks of `#` comment lines, the reason, followed by the
/// mutants' names exactly as cargo-mutants prints them.
pub const EQUIVALENTS: &str = "tests/verification/equivalent-mutants.txt";

/// The test cargo-mutants runs for every mutant: [`super::expected::judge`].
pub const TEST: [&str; 4] = ["--test", "m4_expected", "-E", "test(=source_mutant_case)"];

/// A frozen source-mutation class and its target functions (§25.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The short name: the CI matrix entry and the output directory.
    pub name: &'static str,
    /// The §25.2 class.
    pub class: &'static str,
    /// The mutated package.
    pub package: &'static str,
    /// The file holding the functions.
    pub file: &'static str,
    /// The functions, as cargo-mutants names them.
    pub functions: &'static [&'static str],
}

impl Target {
    /// The regex selecting exactly this target's mutants by name.
    pub fn re(&self) -> String {
        format!(r"(in |replace )({})\b", self.functions.join("|"))
    }

    /// cargo-mutants' arguments for this target, writing into `out`. The baseline is
    /// skipped: cargo-mutants would test only the mutated package there, so the caller
    /// runs [`TEST`] unmutated first. The repository's mutants configuration is not read:
    /// every operator applies.
    pub fn args(&self, out: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "mutants",
            "--no-config",
            "--in-place",
            "--no-shuffle",
            "--baseline=skip",
            "--timeout",
            "300",
            "--test-tool",
            "nextest",
            "--test-package",
            "systemscope-acceptance",
            "-f",
            self.file,
            "-F",
        ]
        .map(str::to_owned)
        .into();
        args.push(self.re());
        args.extend(["-o".to_owned(), out.to_owned(), "--".to_owned()]);
        args.extend(TEST.map(str::to_owned));
        args
    }
}

/// The nine targets of §25.2. Where a frozen function only dispatches, its result is
/// computed by private helpers that nothing else calls, and those are part of the target
/// (§30.7): `op_imm`, `shift`, and `op_reg` for `execute_alu`, `taken` for
/// `execute_control`, and `complete_load` and `complete_store` for `complete_memory`.
pub const TARGETS: [Target; 9] = [
    Target {
        name: "alu",
        class: "ALU result",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/execute.rs",
        functions: &["execute_alu", "op_imm", "shift", "op_reg"],
    },
    Target {
        name: "control",
        class: "control flow",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/execute.rs",
        functions: &["execute_control", "taken"],
    },
    Target {
        name: "memory",
        class: "load and store completion",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/memory.rs",
        functions: &["complete_memory", "complete_load", "complete_store"],
    },
    Target {
        name: "commit",
        class: "retirement record fields",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/cpu.rs",
        functions: &["Rv32iCpu::commit_fields"],
    },
    Target {
        name: "exception",
        class: "exception delivery",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/privilege.rs",
        functions: &["M3State::take_exception"],
    },
    Target {
        name: "sret",
        class: "privilege return",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/privilege.rs",
        functions: &["M3State::sret"],
    },
    Target {
        name: "sv32",
        class: "Sv32 translation",
        package: "systemscope-rv32i",
        file: "components/rv32i/src/sv32.rs",
        functions: &["step", "sv32_translate"],
    },
    Target {
        name: "syscall",
        class: "syscall dispatch",
        package: "systemscope-os",
        file: "components/os/src/procop.rs",
        functions: &["Model::syscall"],
    },
    Target {
        name: "schedule",
        class: "scheduling",
        package: "systemscope-os",
        file: "components/os/src/process.rs",
        functions: &["Processes::schedule_next", "Processes::yield_current"],
    },
];

/// The target named `name`.
pub fn target(name: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|t| t.name == name)
}

/// What became of one mutant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The case found it; the text is what [`super::expected::judge`] reported after
    /// [`DETECTED`]: `Diverged at …`, `AuxiliaryDivergence in …`, or `BackendFailed: …`.
    Detected(String),
    /// The test failed without a verdict: the mutated side panicked.
    Crashed,
    /// The test did not finish within the timeout.
    Hung,
    /// The mutant does not build.
    Unviable,
    /// The case agrees: equivalent, or a gap.
    Missed,
    /// The locator and the naive oracle disagree: an engine bug.
    OracleMismatch(String),
}

impl Fate {
    /// Whether the mutant counts as detected (§25.2).
    pub fn detected(&self) -> bool {
        matches!(self, Fate::Detected(_) | Fate::Crashed | Fate::Hung)
    }
}

/// One mutant's name, as cargo-mutants prints it, and its fate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutant {
    /// `file:line:column: description`.
    pub name: String,
    /// Its fate.
    pub fate: Fate,
}

/// Reads the mutants of a cargo-mutants output directory `out` (holding `mutants.out`),
/// in its order, each fate from its outcome and, for a caught mutant, its log.
pub fn read_outcomes(out: &Path) -> Result<Vec<Mutant>, String> {
    let dir = out.join("mutants.out");
    let path = dir.join("outcomes.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let json: Json = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let outcomes = json["outcomes"]
        .as_array()
        .ok_or_else(|| format!("{}: no outcomes", path.display()))?;
    let mut mutants = Vec::new();
    for o in outcomes {
        let Some(name) = o["scenario"]["Mutant"]["name"].as_str() else {
            continue;
        };
        let fate = match o["summary"].as_str() {
            Some("CaughtMutant") => {
                let log = o["log_path"]
                    .as_str()
                    .ok_or("a caught mutant without a log")?;
                let log = dir.join(log);
                let text =
                    fs::read_to_string(&log).map_err(|e| format!("{}: {e}", log.display()))?;
                caught(&text)
            }
            Some("MissedMutant") => Fate::Missed,
            Some("Timeout") => Fate::Hung,
            Some("Unviable") => Fate::Unviable,
            other => return Err(format!("{name}: unexpected outcome {other:?}")),
        };
        mutants.push(Mutant {
            name: name.to_owned(),
            fate,
        });
    }
    Ok(mutants)
}

/// A caught mutant's fate from its log.
pub fn caught(log: &str) -> Fate {
    let after = |marker: &str| {
        log.lines()
            .find_map(|l| l.split_once(marker).map(|(_, rest)| rest.trim().to_owned()))
    };
    if let Some(why) = after(ORACLE_MISMATCH) {
        return Fate::OracleMismatch(why);
    }
    match after(DETECTED) {
        Some(what) => Fate::Detected(what),
        None => Fate::Crashed,
    }
}

/// One listed equivalent mutant and its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Equivalent {
    /// The mutant's name.
    pub name: String,
    /// Why it is equivalent.
    pub reason: String,
}

/// Parses [`EQUIVALENTS`]: every mutant must follow a reason, and none is listed twice.
pub fn parse_equivalents(text: &str) -> Result<Vec<Equivalent>, String> {
    let mut out: Vec<Equivalent> = Vec::new();
    let mut reason = String::new();
    let mut in_comment = false;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if let Some(c) = line.strip_prefix('#') {
            if !in_comment {
                reason.clear();
            }
            in_comment = true;
            if !reason.is_empty() {
                reason.push(' ');
            }
            reason.push_str(c.trim());
            continue;
        }
        in_comment = false;
        if line.is_empty() {
            reason.clear();
            continue;
        }
        if reason.is_empty() {
            return Err(format!("line {}: {line} has no reason", i + 1));
        }
        if out.iter().any(|e| e.name == line) {
            return Err(format!("line {}: {line} is listed twice", i + 1));
        }
        out.push(Equivalent {
            name: line.to_owned(),
            reason: reason.clone(),
        });
    }
    Ok(out)
}

/// Every violation of §25.2 in `runs`, each target's mutants: an oracle mismatch, a
/// missed mutant not listed as equivalent, or a listed one of a run target not missed.
pub fn violations(runs: &[(&Target, Vec<Mutant>)], equivalents: &[Equivalent]) -> Vec<String> {
    let mut errors = Vec::new();
    for (t, mutants) in runs {
        if mutants.is_empty() {
            errors.push(format!("{}: no mutants", t.name));
        }
        for m in mutants {
            let listed = equivalents.iter().any(|e| e.name == m.name);
            match &m.fate {
                Fate::OracleMismatch(why) => errors.push(format!("{}: {why}", m.name)),
                Fate::Missed if !listed => {
                    errors.push(format!("{}: missed, and not listed as equivalent", m.name));
                }
                f if listed && *f != Fate::Missed => {
                    errors.push(format!("{}: listed as equivalent, but {f:?}", m.name));
                }
                _ => {}
            }
        }
        for e in equivalents.iter().filter(|e| e.name.starts_with(t.file)) {
            let ran = t.functions.iter().any(|f| {
                e.name.ends_with(&format!(" in {f}")) || e.name.contains(&format!(" {f} -> "))
            });
            if ran && !mutants.iter().any(|m| m.name == e.name) {
                errors.push(format!(
                    "{}: listed as equivalent, but not generated",
                    e.name
                ));
            }
        }
    }
    errors
}

/// The Markdown summary: per target, the counts, then every mutant and its fate.
pub fn render(runs: &[(&Target, Vec<Mutant>)]) -> String {
    let count = |ms: &[Mutant], f: fn(&Fate) -> bool| ms.iter().filter(|m| f(&m.fate)).count();
    let mut o = String::from(
        "| Target | Class | Generated | Diverged | Auxiliary | Failed | Crashed | Hung | Unviable | Missed |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (t, ms) in runs {
        let starts = |m: &Mutant, p: &str| matches!(&m.fate, Fate::Detected(w) if w.starts_with(p));
        let by = |p: &str| ms.iter().filter(|m| starts(m, p)).count();
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            t.name,
            t.class,
            ms.len(),
            by("Diverged"),
            by("AuxiliaryDivergence"),
            by("BackendFailed"),
            count(ms, |f| *f == Fate::Crashed),
            count(ms, |f| *f == Fate::Hung),
            count(ms, |f| *f == Fate::Unviable),
            count(ms, |f| *f == Fate::Missed),
        ));
    }
    for (t, ms) in runs {
        o.push_str(&format!("\n### {}\n\n", t.name));
        for m in ms {
            let fate = match &m.fate {
                Fate::Detected(w) => w.clone(),
                f => format!("{f:?}"),
            };
            o.push_str(&format!("- `{}`: {fate}\n", m.name));
        }
    }
    o
}
