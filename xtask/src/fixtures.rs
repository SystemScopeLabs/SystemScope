//! The committed rv32 fixtures and the M3 and M5 reference disks: `rv32-fixtures`,
//! `m3-reference`, and `m5-reference`.

use super::*;

pub(crate) fn rv32_build() -> ExitCode {
    if !cfg!(target_os = "linux") {
        eprintln!("rv32-fixtures build runs on Linux only (docs/m1-design.md §10.6)");
        return ExitCode::FAILURE;
    }
    let root = root();
    let mut rv32ui = vec![RV32_CACHE, FIXTURE_DIR];
    rv32ui.extend(SELECTED);
    for (script, args) in [
        (BUILD_SCRIPT, rv32ui),
        (HELLO_SCRIPT, vec![RV32_CACHE, HELLO_DIR]),
        (BLOCK_IRQ_SCRIPT, vec![RV32_CACHE, BLOCK_IRQ_DIR]),
        (M3_SCRIPT, vec![RV32_CACHE, M3_DIR]),
        (M5_SCRIPT, vec![RV32_CACHE, M5_DIR]),
    ] {
        match Command::new("bash")
            .arg(script)
            .args(&args)
            .current_dir(&root)
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("{script} failed: {s}");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("cannot run {script}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    rv32_manifest(&root.join(RV32_CACHE))
}

pub(crate) fn rv32_manifest(cache: &Path) -> ExitCode {
    let root = root();
    if let Err(errors) = upstream::check(&cache.join("riscv-tests")) {
        eprintln!("the riscv-tests checkout in {} fails:", cache.display());
        for e in errors {
            eprintln!("  {e}");
        }
        return ExitCode::FAILURE;
    }
    let old = Manifest::read(&root).ok();
    let new = match Manifest::generate(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot generate the manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(root.join(MANIFEST_PATH), new.render()) {
        eprintln!("cannot write {MANIFEST_PATH}: {e}");
        return ExitCode::FAILURE;
    }
    let old_hello = HelloManifest::read(&root).ok();
    let hello = match HelloManifest::generate(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot generate the hello manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(root.join(HELLO_MANIFEST), hello.render()) {
        eprintln!("cannot write {HELLO_MANIFEST}: {e}");
        return ExitCode::FAILURE;
    }
    if old_hello.as_ref() == Some(&hello) {
        println!("{HELLO_MANIFEST} is up to date");
    } else {
        println!("wrote {HELLO_MANIFEST}");
    }
    if let Err(e) = block_irq::write_disk(&root) {
        eprintln!("cannot write the block_irq disk fixture: {e}");
        return ExitCode::FAILURE;
    }
    let old_block_irq = BlockIrqManifest::read(&root).ok();
    let block_irq = match BlockIrqManifest::generate(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot generate the block_irq manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(root.join(BLOCK_IRQ_MANIFEST), block_irq.render()) {
        eprintln!("cannot write {BLOCK_IRQ_MANIFEST}: {e}");
        return ExitCode::FAILURE;
    }
    if old_block_irq.as_ref() == Some(&block_irq) {
        println!("{BLOCK_IRQ_MANIFEST} is up to date");
    } else {
        println!("wrote {BLOCK_IRQ_MANIFEST}");
    }
    if let Err(e) = m3_write(&root).and_then(|()| m5_write(&root)) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    match &old {
        None => println!("wrote a new {MANIFEST_PATH}"),
        Some(old) => {
            let changes = systemscope_rv32::manifest::describe_differences(old, &new);
            if changes.is_empty() {
                println!("{MANIFEST_PATH} is up to date");
            } else {
                println!("{MANIFEST_PATH} changed:");
                for change in changes {
                    println!("  {change}");
                }
            }
        }
    }
    rv32_verify()
}

pub(crate) fn rv32_verify() -> ExitCode {
    let root = root();
    let mut ok = true;
    match verify(&root) {
        Ok(manifest) => println!(
            "{} fixtures match {MANIFEST_PATH} (riscv-tests {}, {} excluded)",
            manifest.selected.len(),
            manifest.commit,
            manifest.excluded.len()
        ),
        Err(errors) => {
            ok = false;
            eprintln!("the fixtures do not match {MANIFEST_PATH}:");
            for e in errors {
                eprintln!("  {e}");
            }
        }
    }
    match hello::verify(&root) {
        Ok(manifest) => println!(
            "{} matches {HELLO_MANIFEST} (BLAKE3 {})",
            manifest.elf,
            systemscope_rv32::hex(&manifest.blake3)
        ),
        Err(errors) => {
            ok = false;
            eprintln!("hello.elf does not match {HELLO_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
        }
    }
    match block_irq::verify(&root) {
        Ok(manifest) => println!(
            "{} and {} match {BLOCK_IRQ_MANIFEST} (BLAKE3 {}, {})",
            manifest.elf,
            manifest.disk,
            systemscope_rv32::hex(&manifest.blake3),
            systemscope_rv32::hex(&manifest.disk_blake3)
        ),
        Err(errors) => {
            ok = false;
            eprintln!("block_irq.elf does not match {BLOCK_IRQ_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
        }
    }
    ok &= m3_verify_fixtures(&root);
    ok &= m5_verify_fixtures(&root);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Writes the M3 disks, their expected traces, and the M3 manifest under `root`.
pub(crate) fn m3_write(root: &Path) -> Result<(), String> {
    m3::write_disks(root).map_err(|e| format!("cannot write the M3 disks: {e}"))?;
    eprintln!("running the M3 disks on m3-reference...");
    m3::write_traces(root).map_err(|e| format!("cannot write the M3 traces: {e}"))?;
    let old = M3Manifest::read(root).ok();
    let new =
        M3Manifest::generate(root).map_err(|e| format!("cannot generate the M3 manifest: {e}"))?;
    fs::write(root.join(M3_MANIFEST), new.render())
        .map_err(|e| format!("cannot write {M3_MANIFEST}: {e}"))?;
    if old.as_ref() == Some(&new) {
        println!("{M3_MANIFEST} is up to date");
    } else {
        println!("wrote {M3_MANIFEST}");
    }
    Ok(())
}

pub(crate) fn m3_verify_fixtures(root: &Path) -> bool {
    match m3::verify(root) {
        Ok(manifest) => {
            println!(
                "{}, {} user programs, {} disks, and {} traces match {M3_MANIFEST}",
                manifest.firmware.file,
                manifest.programs.len(),
                manifest.disks.len(),
                manifest.traces.len()
            );
            true
        }
        Err(errors) => {
            eprintln!("the M3 fixtures do not match {M3_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
            false
        }
    }
}

pub(crate) fn m3_manifest() -> ExitCode {
    let root = root();
    if let Err(e) = m3_write(&root) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    if m3_verify_fixtures(&root) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Runs and judges every M3 disk; returns each one's metrics, or `None` if one failed.
pub(crate) fn m3_runs(root: &Path) -> Option<Vec<Metrics>> {
    let mut all = Vec::new();
    let mut ok = true;
    for scenario in Scenario::ALL {
        let (firmware, disk, expected) = match m3::read_fixture(root, scenario) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{}: {e}", scenario.name());
                return None;
            }
        };
        let run = m3::run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
        let metrics = Metrics::of(&run);
        match m3::judge(&run, scenario, &expected) {
            Ok(()) => println!(
                "{} ({}): PASS, output {:?}
  {}",
                scenario.name(),
                scenario.disk_file(),
                String::from_utf8_lossy(scenario.expected_output()),
                metrics.line()
            ),
            Err(e) => {
                ok = false;
                eprintln!("{} ({}): FAIL: {e}", scenario.name(), scenario.disk_file());
            }
        }
        all.push(metrics);
    }
    ok.then_some(all)
}

pub(crate) fn m3_run() -> ExitCode {
    match m3_runs(&root()) {
        Some(_) => ExitCode::SUCCESS,
        None => ExitCode::FAILURE,
    }
}

pub(crate) fn m3_verify() -> ExitCode {
    let root = root();
    if !m3_verify_fixtures(&root) {
        return ExitCode::FAILURE;
    }
    let (Some(first), Some(second)) = (m3_runs(&root), m3_runs(&root)) else {
        return ExitCode::FAILURE;
    };
    if first != second {
        eprintln!(
            "two runs of the M3 disks differ:
  {first:?}
  {second:?}"
        );
        return ExitCode::FAILURE;
    }
    println!("two runs of each M3 disk are identical");
    ExitCode::SUCCESS
}

/// Writes the M5 disk, its expected trace, and the M5 manifest under `root`.
pub(crate) fn m5_write(root: &Path) -> Result<(), String> {
    m5::write_disk(root).map_err(|e| format!("cannot write the M5 disk: {e}"))?;
    eprintln!("running the M5 disk on m5-reference...");
    m5::write_trace(root).map_err(|e| format!("cannot write the M5 trace: {e}"))?;
    let old = M5Manifest::read(root).ok();
    let new =
        M5Manifest::generate(root).map_err(|e| format!("cannot generate the M5 manifest: {e}"))?;
    fs::write(root.join(M5_MANIFEST), new.render())
        .map_err(|e| format!("cannot write {M5_MANIFEST}: {e}"))?;
    if old.as_ref() == Some(&new) {
        println!("{M5_MANIFEST} is up to date");
    } else {
        println!("wrote {M5_MANIFEST}");
    }
    Ok(())
}

pub(crate) fn m5_verify_fixtures(root: &Path) -> bool {
    match m5::verify(root) {
        Ok(manifest) => {
            println!(
                "{}, {} user programs, {}, and {} match {M5_MANIFEST}",
                manifest.firmware.file,
                manifest.programs.len(),
                manifest.disk.file,
                manifest.trace.file
            );
            true
        }
        Err(errors) => {
            eprintln!("the M5 fixtures do not match {M5_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
            false
        }
    }
}

pub(crate) fn m5_manifest() -> ExitCode {
    let root = root();
    if let Err(e) = m5_write(&root) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    if m5_verify_fixtures(&root) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Runs and judges the M5 disk; returns its metrics, or `None` if it failed.
pub(crate) fn m5_runs(root: &Path) -> Option<m5::Metrics> {
    let (firmware, disk, expected) = match m5::read_fixture(root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("m5-reference: {e}");
            return None;
        }
    };
    let run = m5::run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
    let metrics = m5::Metrics::of(&run);
    match m5::judge(&run, &expected) {
        Ok(()) => {
            println!(
                "m5-reference: PASS, output {:?}\n  {}",
                run.output
                    .as_ref()
                    .map(|o| String::from_utf8_lossy(o).into_owned())
                    .unwrap_or_default(),
                metrics.line()
            );
            Some(metrics)
        }
        Err(e) => {
            eprintln!("m5-reference: FAIL: {e}\n  {}", metrics.line());
            None
        }
    }
}

pub(crate) fn m5_run() -> ExitCode {
    match m5_runs(&root()) {
        Some(_) => ExitCode::SUCCESS,
        None => ExitCode::FAILURE,
    }
}

pub(crate) fn m5_verify() -> ExitCode {
    let root = root();
    if !m5_verify_fixtures(&root) {
        return ExitCode::FAILURE;
    }
    let (Some(first), Some(second)) = (m5_runs(&root), m5_runs(&root)) else {
        return ExitCode::FAILURE;
    };
    if first != second {
        eprintln!("two runs of the M5 disk differ:\n  {first:?}\n  {second:?}");
        return ExitCode::FAILURE;
    }
    println!("two runs of the M5 disk are identical");
    ExitCode::SUCCESS
}
