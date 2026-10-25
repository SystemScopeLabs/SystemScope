//! The external references: the pinned Spike (`spike`) and the ACT4 corpus (`act4`).

use super::*;

pub(crate) fn spike_task(task: &str, dir: &Path) -> ExitCode {
    match task {
        "build" => spike_build(dir),
        "verify" => {
            if spike_verify(dir) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "diff" => spike_diff(dir),
        "random" => spike_random(dir),
        "campaign" => spike_campaign(dir),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// The Spike to run: `SPIKE`, or the one installed in `dir`.
pub(crate) fn spike_binary(dir: &Path) -> PathBuf {
    std::env::var_os("SPIKE").map_or_else(|| dir.join("bin/spike"), PathBuf::from)
}

pub(crate) fn spike_build(dir: &Path) -> ExitCode {
    if !cfg!(target_os = "linux") {
        eprintln!("spike build runs on Linux only (docs/m1-design.md §10.3)");
        return ExitCode::FAILURE;
    }
    match Command::new("bash")
        .arg(SPIKE_SCRIPT)
        .arg(dir)
        .current_dir(root())
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("{SPIKE_SCRIPT} failed: {s}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot run {SPIKE_SCRIPT}: {e}");
            return ExitCode::FAILURE;
        }
    }
    spike_task("verify", dir)
}

pub(crate) fn spike_verify(dir: &Path) -> bool {
    let binary = spike_binary(dir);
    match spike::verify_install(&root(), dir, &binary) {
        Ok(checked) => {
            println!("Spike {SPIKE_COMMIT} in {} verified:", dir.display());
            for line in checked {
                println!("  {line}");
            }
            true
        }
        Err(errors) => {
            eprintln!("Spike in {} is not the pinned build:", dir.display());
            for e in errors {
                eprintln!("  {e}");
            }
            false
        }
    }
}

pub(crate) fn spike_diff(dir: &Path) -> ExitCode {
    // A Spike from a cache or elsewhere is checked before anything is compared with it.
    if !spike_verify(dir) {
        return ExitCode::FAILURE;
    }
    let report = match spike::run_differential(&root(), &spike_binary(dir), SPIKE_LOGS) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot run the differential: {e}");
            return ExitCode::FAILURE;
        }
    };
    let spike = spike_binary(dir);
    let generated = spike::run_generated(&root(), &spike, SPIKE_LOGS, &FIXED_SEEDS);
    let misaligned = spike::run_misaligned(&root(), &spike, SPIKE_LOGS);
    let parts = [
        ("rv32ui", &report, SELECTED.len()),
        ("generated", &generated, FIXED_SEEDS.len()),
        ("misaligned", &misaligned, MISALIGNED.len()),
    ];
    let mut accepted = true;
    for (what, report, expected) in parts {
        accepted &= print_diff_report("M1-A3", what, report, expected);
    }
    if !accepted {
        return ExitCode::FAILURE;
    }
    println!(
        "M1-A3: every selected rv32ui test and the generated program of every fixed seed \
         retire exactly as on Spike {SPIKE_COMMIT}, and both trap alike on every \
         misaligned access"
    );
    let m2 = spike::run_m2(&root(), &spike, SPIKE_LOGS);
    let expected = csrgen::pass_programs().len() + csrgen::ILLEGAL.len();
    if !print_diff_report("M2 CSR", "m2-csr", &m2, expected) {
        return ExitCode::FAILURE;
    }
    println!(
        "M2 CSR: every directed Zicsr, CSR, and MRET program retires exactly as on Spike \
         {SPIKE_COMMIT} with --isa={SPIKE_ISA_M2}, CSR writes included, and both trap \
         alike on every rejected CSR"
    );
    let m3 = spike::run_m3(&root(), &spike, SPIKE_LOGS);
    if !print_diff_report("M3 privilege", "m3-priv", &m3, privgen::programs().len()) {
        return ExitCode::FAILURE;
    }
    println!(
        "M3 privilege: every directed privilege, CSR, MRET/SRET, and delegation program \
         retires exactly as on Spike {SPIKE_COMMIT} with --isa={SPIKE_ISA_M2} \
         --priv={SPIKE_PRIV_M3}, modes, CSR writes, and delegated exceptions included, up \
         to the same exception taken in M"
    );
    let sv32 = spike::run_m3_sv32(&root(), &spike, SPIKE_LOGS);
    if !print_diff_report("M3 Sv32", "m3-sv32", &sv32, vmgen::programs().len()) {
        return ExitCode::FAILURE;
    }
    println!(
        "M3 Sv32: every directed Sv32 program (4 KiB pages, megapages, invalid PTEs,          permissions with SUM and MXR, Svade, page and access faults, SFENCE.VMA) retires          exactly as on Spike {SPIKE_COMMIT} with --priv={SPIKE_PRIV_M3}, delegated page          faults included, up to the same exception taken in M"
    );
    let m5 = spike::run_m5(&root(), &spike, SPIKE_LOGS);
    if !print_diff_report("M5 SEI", "m5-sei", &m5, spike::m5_programs().len()) {
        return ExitCode::FAILURE;
    }
    println!(
        "M5 SEI: every directed supervisor external interrupt program, and every M3.2 \
         program again, retires exactly as on Spike {SPIKE_COMMIT} with \
         --priv={SPIKE_PRIV_M3}, interrupts taken in S and in M included, up to the same \
         exception taken in M"
    );
    println!(
        "M4: each of the comparisons above agrees through the M4 stream compare, with \
         Spike's commit log as a post-run backend"
    );
    match spike_adapter::run_sk1(&root(), &spike_binary(dir), SPIKE_LOGS) {
        Ok(line) => println!("M4 SK-1: {line}"),
        Err(e) => {
            eprintln!("M4 SK-1 fails: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// Prints one part of `part_of`'s differential, then whether it is accepted.
pub(crate) fn print_diff_report(
    part_of: &str,
    what: &str,
    report: &DiffReport,
    expected: usize,
) -> bool {
    for result in &report.results {
        println!("{}", result.line());
    }
    println!(
        "{what}: selected {}, run by SystemScope {}, run by Spike {}, matched {}, \
         retirements compared {}",
        report.selected,
        report.systemscope_executed(),
        report.spike_executed(),
        report.passed(),
        report.compared()
    );
    match report.accept(expected) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{part_of} fails ({what}): {e}");
            false
        }
    }
}

pub(crate) fn spike_campaign(dir: &Path) -> ExitCode {
    if !spike_verify(dir) {
        return ExitCode::FAILURE;
    }
    let seeds = spike_adapter::CAMPAIGN_SEEDS;
    match spike_adapter::run_campaign(&root(), &spike_binary(dir), SPIKE_LOGS, seeds.clone()) {
        Ok(found) => {
            for (name, n) in &found {
                println!("{name}: agrees, {n} observations compared");
            }
            let total: u64 = found.iter().map(|(_, n)| n).sum();
            println!(
                "M4 Spike campaign: seeds {seeds:?}, {} programs agree with Spike \
                 {SPIKE_COMMIT}, {total} observations compared",
                found.len()
            );
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("{e}");
            }
            eprintln!("M4 Spike campaign fails: {} programs", errors.len());
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn spike_random(dir: &Path) -> ExitCode {
    let var = progen::SEED_VAR;
    let Some(seed) = std::env::var(var)
        .ok()
        .and_then(|text| systemscope_acceptance::parse_seed(&text))
    else {
        eprintln!("set {var} to the seed to test, e.g. {var}=0x1234");
        return ExitCode::FAILURE;
    };
    // Printed first, so a failing run names its seed.
    println!("{var}={seed:#x}");
    if !spike_verify(dir) {
        return ExitCode::FAILURE;
    }
    let report = spike::run_generated(&root(), &spike_binary(dir), SPIKE_LOGS, &[seed]);
    if print_diff_report("M1-A3", "random", &report, 1) {
        ExitCode::SUCCESS
    } else {
        eprintln!("the generated program for {var}={seed:#x} differs from Spike");
        ExitCode::FAILURE
    }
}

pub(crate) fn act4_build(cache: &Path) -> ExitCode {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        eprintln!("act4 build runs on Linux x86_64 only (docs/m1-design.md §10.4)");
        return ExitCode::FAILURE;
    }
    let root = root();
    let out = root.join(ACT4_OUT);
    match Command::new("bash")
        .arg(ACT4_SCRIPT)
        .arg(cache)
        .arg(&out)
        .current_dir(&root)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("{ACT4_SCRIPT} failed: {s}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot run {ACT4_SCRIPT}: {e}");
            return ExitCode::FAILURE;
        }
    }
    act4_install(&out)
}

pub(crate) fn act4_install(out: &Path) -> ExitCode {
    match act4::install(&root(), out) {
        Ok((new, None)) => println!("wrote a new {ACT4_MANIFEST} with {} tests", new.count),
        Ok((new, Some(old))) => {
            let changes = act4::describe_differences(&old, &new);
            if changes.is_empty() {
                println!("{ACT4_MANIFEST} is up to date");
            } else {
                println!("{ACT4_MANIFEST} changed:");
                for change in changes {
                    println!("  {change}");
                }
            }
        }
        Err(errors) => {
            eprintln!("cannot install the generation in {}:", out.display());
            for e in errors {
                eprintln!("  {e}");
            }
            return ExitCode::FAILURE;
        }
    }
    act4_verify()
}

pub(crate) fn act4_check(out: &Path) -> ExitCode {
    match act4::check_regenerated(&root(), out) {
        Ok(manifest) => {
            println!(
                "the generation in {} reproduces the committed ACT4 corpus: {} ELFs and \
                 {ACT4_MANIFEST}, byte for byte",
                out.display(),
                manifest.count
            );
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!(
                "the generation in {} does not reproduce the committed corpus:",
                out.display()
            );
            for e in errors {
                eprintln!("  {e}");
            }
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn act4_verify() -> ExitCode {
    match act4::verify(&root()) {
        Ok(manifest) => {
            print_act4_summary(&manifest);
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!("the ACT4 corpus does not match {ACT4_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
            ExitCode::FAILURE
        }
    }
}

/// The corpus's pins and instruction audit, as `verify` and `run` print them.
pub(crate) fn print_act4_summary(manifest: &act4::Act4Manifest) {
    println!(
        "{} ACT4 ELFs match {ACT4_MANIFEST} (ACT4 {}, Sail {}, extensions {})",
        manifest.count,
        act4::ACT4_COMMIT,
        act4::SAIL_VERSION,
        act4::EXTENSIONS
    );
    println!(
        "architectural capability {}; ACT4 adapter schema extensions {} (Sm is a schema \
         shim, not a capability); include_priv_tests {}",
        manifest.capability,
        manifest.adapter_extensions.join(" + "),
        manifest.include_priv_tests
    );
    let mut total = std::collections::BTreeMap::<&str, u64>::new();
    for t in &manifest.tests {
        for (m, n) in &t.recorded.instructions {
            *total.entry(m.as_str()).or_default() += n;
        }
    }
    let outside: u64 = total
        .iter()
        .filter(|(m, _)| !act4::ALLOWED_MNEMONICS.contains(m))
        .map(|(_, n)| n)
        .sum();
    println!(
        "instruction audit: {} distinct mnemonics; outside RV32I and ECALL (CSR, MRET, SRET, \
         WFI, M, A, C, Zifencei, ...): {outside}; ecall {}, fence.tso {}",
        total.len(),
        total.get("ecall").copied().unwrap_or(0),
        total.get("fence.tso").copied().unwrap_or(0)
    );
}

pub(crate) fn act4_run() -> ExitCode {
    let (manifest, report) = match act4::run_corpus(&root()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    print_act4_summary(&manifest);
    for result in &report.results {
        println!("{}", result.line());
    }
    println!(
        "manifest {}, fixtures {}, selected {}, executed {}, passed {}, failed {}",
        manifest.count,
        manifest.tests.len(),
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    match report.accept(manifest.count) {
        Ok(()) => {
            println!(
                "M1-A4: all {} ACT4 RV32I tests pass on m1-reference, each with its exact \
                 TEST PASSED summary",
                manifest.count
            );
        }
        Err(e) => {
            eprintln!("M1-A4 fails: {e}");
            return ExitCode::FAILURE;
        }
    }
    // docs/m2-design.md §15.4: the M2 CPU profile passes the same corpus.
    let report = match act4::run_corpus_with(&root(), Rv32iProfile::M2) {
        Ok((_, r)) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    for result in report.results.iter().filter(|r| r.verdict.is_err()) {
        println!("{}", result.line());
    }
    println!(
        "M2 CPU profile: selected {}, executed {}, passed {}, failed {}",
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    if let Err(e) = report.accept(manifest.count) {
        eprintln!("the M2 CPU profile fails the ACT4 corpus: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "all {} ACT4 RV32I tests also pass with the M2 CPU profile",
        manifest.count
    );
    // docs/m3-design.md §15.4: the M3 CPU profile passes it too, in M-mode.
    let report = match act4::run_corpus_with(&root(), Rv32iProfile::M3) {
        Ok((_, r)) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    for result in report.results.iter().filter(|r| r.verdict.is_err()) {
        println!("{}", result.line());
    }
    println!(
        "M3 CPU profile: selected {}, executed {}, passed {}, failed {}",
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    match report.accept(manifest.count) {
        Ok(()) => {
            println!(
                "all {} ACT4 RV32I tests also pass with the M3 CPU profile",
                manifest.count
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("the M3 CPU profile fails the ACT4 corpus: {e}");
            ExitCode::FAILURE
        }
    }
}
