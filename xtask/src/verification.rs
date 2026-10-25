//! The M4 and M5 verification tasks: the corpus, expected streams, source mutants,
//! coverage, and reproducers.

use super::*;

pub(crate) fn m4_fixture() -> Result<M3Fixture, Vec<String>> {
    M3Fixture::read(&root(), Scenario::Reference).map_err(|e| vec![e])
}

pub(crate) fn corpus_replay() -> ExitCode {
    match m4_fixture().and_then(|f| corpus::replay(&f, &root())) {
        Ok(entries) => {
            for (name, outcome) in &entries {
                println!("{name}: {outcome:?}");
            }
            report(
                Ok(()),
                &format!(
                    "{} corpus entries in {} replay byte for byte",
                    entries.len(),
                    corpus::DIR
                ),
            )
        }
        Err(errors) => report(Err(errors), ""),
    }
}

pub(crate) fn corpus_check(dir: &Path) -> ExitCode {
    let result = m4_fixture().and_then(|f| corpus::check_dir(&f, dir).map_err(|e| vec![e]));
    match result {
        Ok(outcome) => report(
            Ok(()),
            &format!(
                "{} can be a corpus entry ({outcome:?}); promote it by a reviewed commit",
                dir.display()
            ),
        ),
        Err(errors) => report(Err(errors), ""),
    }
}

pub(crate) fn mutants_run(targets: &'static [m4m::Target], name: &str, out: &str) -> ExitCode {
    let Some(target) = targets.iter().find(|t| t.name == name) else {
        let names: Vec<&str> = targets.iter().map(|t| t.name).collect();
        eprintln!(
            "no source-mutant target {name}: one of {}",
            names.join(", ")
        );
        return ExitCode::FAILURE;
    };
    // The baseline cargo-mutants skips: the case agrees on the unmutated tree.
    let baseline = Command::new("cargo")
        .args(["nextest", "run", "-p", "systemscope-acceptance"])
        .args(target.test)
        .current_dir(root())
        .status();
    if !baseline.is_ok_and(|s| s.success()) {
        eprintln!("the source-mutant case fails on the unmutated tree");
        return ExitCode::FAILURE;
    }
    let out = format!("{out}/{}", target.name);
    let _ = fs::remove_dir_all(root().join(&out));
    // cargo-mutants creates `<out>/mutants.out`, but not `<out>`.
    if let Err(e) = fs::create_dir_all(root().join(&out)) {
        eprintln!("{out}: {e}");
        return ExitCode::FAILURE;
    }
    match Command::new("cargo")
        .args(target.args(&out))
        .current_dir(root())
        .status()
    {
        // 0: all caught; 2: missed; 3: timeouts. The summary judges them.
        Ok(s) if matches!(s.code(), Some(0 | 2 | 3)) => {
            println!("{}: cargo-mutants exited {s}; summarize {out}", target.name);
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("{}: cargo-mutants failed: {s}", target.name);
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("cannot run cargo-mutants: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn mutants_summarize(targets: &'static [m4m::Target], dir: &Path) -> ExitCode {
    let root = root();
    let equivalents = fs::read_to_string(root.join(m4m::EQUIVALENTS))
        .map_err(|e| format!("{}: {e}", m4m::EQUIVALENTS))
        .and_then(|t| m4m::parse_equivalents(&t));
    let equivalents = match equivalents {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut runs = Vec::new();
    for t in targets {
        let out = dir.join(t.name);
        if !out.join("mutants.out").exists() {
            println!("{}: not run", t.name);
            continue;
        }
        match m4m::read_outcomes(&out) {
            Ok(m) => runs.push((t, m.into_iter().filter(|m| t.selects(&m.name)).collect())),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if runs.is_empty() {
        eprintln!("no source-mutant run in {}", dir.display());
        return ExitCode::FAILURE;
    }
    println!("{}", m4m::render(&runs));
    report(
        match m4m::violations(&runs, &equivalents) {
            v if v.is_empty() => Ok(()),
            v => Err(v),
        },
        "every source mutant is detected or listed as equivalent, and the locator equals \
         the naive oracle",
    )
}

pub(crate) fn m4_expected_bless() -> ExitCode {
    let result = m4_fixture().and_then(|f| m4x::bless(&f, &root()).map_err(|e| vec![e]));
    if result.is_err() {
        return report(result, "");
    }
    m4_expected_verify()
}

pub(crate) fn m4_expected_verify() -> ExitCode {
    let result = m4_fixture().and_then(|f| m4x::verify(&f, &root()));
    report(
        result,
        &format!("this build reproduces the expected streams in {}", m4x::DIR),
    )
}

pub(crate) fn m5_expected(bless: bool) -> ExitCode {
    let root = root();
    let result = m5g::Fixture::read(&root)
        .map_err(|e| vec![e])
        .and_then(|f| {
            if bless {
                m5x::bless(&f, &root).map_err(|e| vec![e])?;
            }
            m5x::verify(&f, &root)
        });
    report(
        result,
        &format!("this build reproduces the expectations in {}", m5x::DIR),
    )
}

pub(crate) fn m5_coverage(bless: bool, reports: m5cov::Reports) -> ExitCode {
    let root = root();
    let fixtures = m4_fixture().and_then(|m3f| {
        m5g::Fixture::read(&root)
            .map(|m5f| (m3f, m5f))
            .map_err(|e| vec![e])
    });
    let result = fixtures.and_then(|(m3f, m5f)| {
        if bless {
            m5cov::bless(&m3f, &m5f, &root, reports).map_err(|e| vec![e])?;
        }
        m5cov::verify(&m3f, &m5f, &root, reports)
    });
    report(
        result,
        &format!(
            "this build reproduces the {reports:?} coverage reports in {}",
            m5cov::DIR
        ),
    )
}

pub(crate) fn m4_emit(dir: &Path) -> ExitCode {
    let result = m4_fixture().and_then(|f| m4r::emit(&f, dir).map_err(|e| vec![e]));
    report(
        result,
        &format!(
            "wrote this machine's M4 reproducers into {}: {}",
            dir.display(),
            m4r::NAMES.join(", ")
        ),
    )
}

pub(crate) fn m4_check(dir: &Path) -> ExitCode {
    let result = m4_fixture().and_then(|f| m4r::check(&f, dir));
    report(
        result,
        "the foreign M4 reproducers equal this machine's byte for byte and replay here",
    )
}

pub(crate) fn m4_replay(dir: &Path) -> ExitCode {
    match m4_fixture().and_then(|f| m4r::replay_all(&f, dir)) {
        Ok(n) => report(Ok(()), &format!("{n} reproducers replay byte for byte")),
        Err(errors) => report(Err(errors), ""),
    }
}

pub(crate) fn m5_repro_emit(dir: &Path) -> ExitCode {
    let result = m5g::Fixture::read(&root()).and_then(|f| m5r::emit(&f, dir));
    report(
        result.map_err(|e| vec![e]),
        &format!("wrote this machine's M5 reproducers into {}", dir.display()),
    )
}

pub(crate) fn m5_repro_check(dir: &Path) -> ExitCode {
    let result = m5g::Fixture::read(&root())
        .map_err(|e| vec![e])
        .and_then(|f| m5r::check(&f, dir));
    report(
        result,
        "the foreign M5 reproducers equal this machine's byte for byte and replay here",
    )
}

pub(crate) fn m5_repro_replay(dir: &Path) -> ExitCode {
    match m5g::Fixture::read(&root())
        .map_err(|e| vec![e])
        .and_then(|f| m5r::replay_all(&f, dir))
    {
        Ok(n) => report(Ok(()), &format!("{n} reproducers replay to their results")),
        Err(errors) => report(Err(errors), ""),
    }
}
