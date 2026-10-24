//! Source-mutant validation (`docs/m4-design.md` §25.2) without running cargo-mutants:
//! the targets exist and select their mutants, the equivalents parse, and the summary
//! reads each fate and refuses a gap, a stale listing, or an oracle mismatch.

use std::fs;

use systemscope_acceptance::m3::expected::{DETECTED, ORACLE_MISMATCH};
use systemscope_acceptance::m3::mutants::{
    EQUIVALENTS, Equivalent, Fate, Mutant, TARGETS, caught, parse_equivalents, read_outcomes,
    render, target, violations,
};
use systemscope_contracts::trace::Value;
use systemscope_rv32::workspace_root;
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::recorded::stream_from_bytes;

#[test]
fn every_target_function_exists_in_its_file() {
    let root = workspace_root();
    assert_eq!(TARGETS.len(), 9);
    for t in &TARGETS {
        let text = fs::read_to_string(root.join(t.file)).unwrap();
        for f in t.functions {
            let name = f.rsplit("::").next().unwrap();
            assert!(text.contains(&format!("fn {name}(")), "{}: {f}", t.file);
        }
        let package = t.file.split('/').nth(1).unwrap();
        assert_eq!(t.package, format!("systemscope-{package}"));
        assert_eq!(target(t.name), Some(t));
    }
    assert_eq!(target("nothing"), None);
    let names: std::collections::BTreeSet<_> = TARGETS.iter().map(|t| t.name).collect();
    assert_eq!(names.len(), TARGETS.len());
}

#[test]
fn a_target_selects_its_functions_and_runs_the_case() {
    let t = target("schedule").unwrap();
    assert_eq!(
        t.re(),
        r"(in |replace )(Processes::schedule_next|Processes::yield_current)\b"
    );
    let args = t.args("target/m4-mutants/schedule");
    let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(at("-f"), t.file);
    assert_eq!(at("-F"), t.re());
    assert_eq!(at("-o"), "target/m4-mutants/schedule");
    assert_eq!(at("--test-package"), "systemscope-acceptance");
    for flag in ["--no-config", "--in-place", "--baseline=skip"] {
        assert!(args.iter().any(|a| a == flag), "{flag}");
    }
    let dashes = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(
        args[dashes + 1..],
        ["--test", "m4_expected", "-E", "test(=source_mutant_case)"]
    );
}

#[test]
fn a_caught_mutant_is_classified_by_its_log() {
    let log = |line: &str| format!("running 1 test\n    {line}\nnote: backtrace\n");
    assert_eq!(
        caught(&log(&format!("{DETECTED} Diverged at arch[64], retire pc"))),
        Fate::Detected("Diverged at arch[64], retire pc".to_owned())
    );
    assert_eq!(
        caught(&log(&format!("{ORACLE_MISMATCH} the locator finds x"))),
        Fate::OracleMismatch("the locator finds x".to_owned())
    );
    assert_eq!(caught(&log("thread panicked at cpu.rs")), Fate::Crashed);
    assert!(Fate::Crashed.detected() && Fate::Hung.detected());
    assert!(!Fate::Missed.detected() && !Fate::Unviable.detected());
    assert!(!Fate::OracleMismatch(String::new()).detected());
}

#[test]
fn equivalents_need_a_reason_each() {
    let text =
        "# header\n\n# why a\n# and more\na.rs:1:1: m1\na.rs:2:1: m2\n\n# why b\nb.rs:3:1: m3\n";
    let e = parse_equivalents(text).unwrap();
    assert_eq!(
        e,
        [
            Equivalent {
                name: "a.rs:1:1: m1".to_owned(),
                reason: "why a and more".to_owned()
            },
            Equivalent {
                name: "a.rs:2:1: m2".to_owned(),
                reason: "why a and more".to_owned()
            },
            Equivalent {
                name: "b.rs:3:1: m3".to_owned(),
                reason: "why b".to_owned()
            },
        ]
    );
    assert!(
        parse_equivalents("# header\n\nx.rs:1:1: m\n")
            .unwrap_err()
            .contains("no reason")
    );
    assert!(
        parse_equivalents("# r\nx\nx\n")
            .unwrap_err()
            .contains("twice")
    );
    let committed = fs::read_to_string(workspace_root().join(EQUIVALENTS)).unwrap();
    parse_equivalents(&committed).unwrap();
}

#[test]
fn the_summary_refuses_gaps_stale_listings_and_mismatches() {
    let t = target("sret").unwrap();
    let file = t.file;
    let m = |n: u32, fate: Fate| Mutant {
        name: format!("{file}:{n}:9: replace M3State::sret -> u32 with {n}"),
        fate,
    };
    let detected = Fate::Detected("Diverged at arch[64], kind retire/exception".to_owned());
    let runs = vec![(
        t,
        vec![
            m(0, detected.clone()),
            m(1, Fate::Unviable),
            m(2, Fate::Hung),
        ],
    )];
    assert_eq!(violations(&runs, &[]), Vec::<String>::new());
    let table = render(&runs);
    assert!(
        table.contains("| sret | privilege return | 3 | 1 | 0 | 0 | 0 | 0 | 1 | 1 | 0 |"),
        "{table}"
    );
    assert!(table.contains("with 0`: Diverged at arch[64]"), "{table}");
    let missed = vec![(t, vec![m(0, Fate::Missed)])];
    assert!(violations(&missed, &[])[0].contains("not listed as equivalent"));
    let listed = |n: u32| Equivalent {
        name: m(n, Fate::Missed).name,
        reason: "r".to_owned(),
    };
    assert!(violations(&missed, &[listed(0)]).is_empty());
    // A listed mutant that is now detected, or no longer generated.
    let now = vec![(t, vec![m(0, detected)])];
    assert!(violations(&now, &[listed(0)])[0].contains("listed as equivalent, but"));
    assert!(violations(&missed, &[listed(0), listed(5)])[0].contains("not generated"));
    // A listing for a target that did not run is not judged.
    let other = target("alu").unwrap();
    assert!(violations(&[(other, vec![m(0, Fate::Unviable)])], &[listed(5)]).is_empty());
    let mismatch = vec![(t, vec![m(0, Fate::OracleMismatch("x".to_owned()))])];
    assert_eq!(violations(&mismatch, &[]).len(), 1);
    assert!(violations(&[(t, Vec::new())], &[])[0].contains("no mutants"));
}

#[test]
fn outcomes_are_read_with_their_logs() {
    let dir = workspace_root().join("target/m4-mutants-test");
    let _ = fs::remove_dir_all(&dir);
    let out = dir.join("mutants.out");
    fs::create_dir_all(out.join("log")).unwrap();
    let outcome = |name: &str, summary: &str, log: &str| {
        format!(
            r#"{{"scenario": {{"Mutant": {{"name": "{name}"}}}}, "summary": "{summary}", "log_path": "{log}"}}"#
        )
    };
    let json = format!(
        r#"{{"outcomes": [{{"scenario": "Baseline", "summary": "Success"}}, {}, {}, {}, {}, {}]}}"#,
        outcome("a.rs:1:1: one", "CaughtMutant", "log/one.log"),
        outcome("a.rs:2:1: two", "CaughtMutant", "log/two.log"),
        outcome("a.rs:3:1: three", "MissedMutant", "log/three.log"),
        outcome("a.rs:4:1: four", "Timeout", "log/four.log"),
        outcome("a.rs:5:1: five", "Unviable", "log/five.log"),
    );
    fs::write(out.join("outcomes.json"), json).unwrap();
    fs::write(
        out.join("log/one.log"),
        format!("{DETECTED} BackendFailed: budget\n"),
    )
    .unwrap();
    fs::write(out.join("log/two.log"), "panicked\n").unwrap();
    let fates: Vec<Fate> = read_outcomes(&dir)
        .unwrap()
        .into_iter()
        .map(|m| m.fate)
        .collect();
    assert_eq!(
        fates,
        [
            Fate::Detected("BackendFailed: budget".to_owned()),
            Fate::Crashed,
            Fate::Missed,
            Fate::Hung,
            Fate::Unviable
        ]
    );
    fs::remove_file(out.join("log/two.log")).unwrap();
    assert!(read_outcomes(&dir).unwrap_err().contains("two.log"));
    fs::write(
        out.join("outcomes.json"),
        r#"{"outcomes": [{"scenario": {"Mutant": {"name": "x"}}, "summary": "Weird"}]}"#,
    )
    .unwrap();
    assert!(read_outcomes(&dir).unwrap_err().contains("Weird"));
    let _ = fs::remove_dir_all(&dir);
}

/// The instruction classes the equivalent-mutant reasons rely on (`equivalent-mutants.txt`):
/// read from the committed `arch` stream, m3-reference's ALU instructions are LUI, AUIPC,
/// and ADDI, its only branch is BNE, its loads are LW and LBU, and it retires no JALR.
#[test]
fn m3_reference_retires_only_the_listed_instructions() {
    let path = workspace_root().join("tests/verification/expected/m3-reference/arch.obs");
    let arch = stream_from_bytes(&fs::read(path).unwrap()).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for o in arch.iter().filter(|o| o.kind == "retire") {
        let Some(ObservedValue::Present(Value::U64(insn))) = o.field("insn") else {
            panic!("arch[{}] has no insn", o.ordinal)
        };
        let (opcode, funct3) = (insn & 0x7f, (insn >> 12) & 7);
        let class = match (opcode, funct3) {
            (0x37, _) => "lui",
            (0x17, _) => "auipc",
            (0x13, 0) => "addi",
            (0x13, _) => "other op-imm or shift",
            (0x33, _) => "op",
            (0x67, _) => "jalr",
            (0x63, 1) => "bne",
            (0x63, _) => "other branch",
            (0x03, 2) => "lw",
            (0x03, 4) => "lbu",
            (0x03, _) => "other load",
            _ => "unlisted",
        };
        seen.insert(class);
    }
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        ["addi", "auipc", "bne", "lbu", "lui", "lw", "unlisted"]
    );
}
