//! Golden files and the every-event resumes: `bless`, `m1-golden` to `m3-golden`,
//! `m5-golden`, and their `every-event` sweeps.

use super::*;

pub(crate) fn bless() -> ExitCode {
    let root = root();
    let json_path = root.join(GOLDEN_PATH);
    let snap_path = root.join(MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running m0-reference for every fixed seed...");
    let (new, snapshot) = Golden::generate();
    let json = new.render();

    let mut changes = describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {GOLDEN_PATH}: formatting only"));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {MID_SNAPSHOT_PATH}: bytes differ from the recorded hash"
        ));
    }
    if changes.is_empty() {
        println!("golden files are up to date");
        return ExitCode::SUCCESS;
    }

    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {GOLDEN_PATH} and {MID_SNAPSHOT_PATH}");
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

pub(crate) fn m1_bless() -> ExitCode {
    let root = root();
    let json_path = root.join(m1::GOLDEN_PATH);
    let snap_path = root.join(m1::MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| m1::Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running every committed program on m1-reference...");
    let (new, snapshot) = match m1::Golden::generate(&root) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("not blessed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = new.render();

    let mut changes = m1::describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {}: formatting only", m1::GOLDEN_PATH));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {}: bytes differ from the recorded hash",
            m1::MID_SNAPSHOT_PATH
        ));
    }
    if changes.is_empty() {
        println!("M1 golden files are up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the M1 golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("M1 golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {} and {}", m1::GOLDEN_PATH, m1::MID_SNAPSHOT_PATH);
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

/// The committed M1 golden file and portable snapshot.
pub(crate) fn m1_committed(root: &Path) -> Result<(String, Vec<u8>), String> {
    let json = fs::read_to_string(root.join(m1::GOLDEN_PATH))
        .map_err(|e| format!("{}: {e}", m1::GOLDEN_PATH))?;
    let snap = fs::read(root.join(m1::MID_SNAPSHOT_PATH))
        .map_err(|e| format!("{}: {e}", m1::MID_SNAPSHOT_PATH))?;
    Ok((json, snap))
}

pub(crate) fn m1_verify() -> ExitCode {
    let root = root();
    let result = m1_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| m1::Golden::verify(&root, &json, &snap));
    report(
        result,
        &format!(
            "every committed program matches {}, and {} restores to it",
            m1::GOLDEN_PATH,
            m1::MID_SNAPSHOT_PATH
        ),
    )
}

pub(crate) fn m1_emit(dir: &Path) -> ExitCode {
    let (golden, snapshot) = match m1::Golden::generate(&root()) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join(M1_RESULT), golden.render()))
        .and_then(|()| fs::write(dir.join(m1::MID_FILE), &snapshot))
    {
        eprintln!("cannot write into {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!(
        "wrote this machine's M1 result: {M1_RESULT} and {} ({} bytes, BLAKE3 {})",
        m1::MID_FILE,
        golden.mid.size,
        systemscope_rv32::hex(&golden.mid.blake3)
    );
    ExitCode::SUCCESS
}

pub(crate) fn m1_check(dir: &Path) -> ExitCode {
    let root = root();
    let result = m1_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| {
            let read_error = |e: std::io::Error| vec![format!("{}: {e}", dir.display())];
            let foreign_json = fs::read_to_string(dir.join(M1_RESULT)).map_err(read_error)?;
            let foreign_snap = fs::read(dir.join(m1::MID_FILE)).map_err(read_error)?;
            m1::Golden::check_foreign(&root, (&json, &snap), (&foreign_json, &foreign_snap))
        });
    report(
        result,
        "the foreign M1 result equals the committed golden files and this machine's run, \
         and its snapshot restores here to the golden end",
    )
}

pub(crate) fn m2_bless() -> ExitCode {
    let root = root();
    let json_path = root.join(m2::GOLDEN_PATH);
    let snap_path = root.join(m2::MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| m2::Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running block_irq.elf on m2-reference...");
    let (new, snapshot) = match m2::Golden::generate(&root) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("not blessed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = new.render();

    let mut changes = m2::describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {}: formatting only", m2::GOLDEN_PATH));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {}: bytes differ from the recorded hash",
            m2::MID_SNAPSHOT_PATH
        ));
    }
    if changes.is_empty() {
        println!("M2 golden files are up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the M2 golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("M2 golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {} and {}", m2::GOLDEN_PATH, m2::MID_SNAPSHOT_PATH);
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

/// The committed M2 golden file and portable snapshot.
pub(crate) fn m2_committed(root: &Path) -> Result<(String, Vec<u8>), String> {
    let json = fs::read_to_string(root.join(m2::GOLDEN_PATH))
        .map_err(|e| format!("{}: {e}", m2::GOLDEN_PATH))?;
    let snap = fs::read(root.join(m2::MID_SNAPSHOT_PATH))
        .map_err(|e| format!("{}: {e}", m2::MID_SNAPSHOT_PATH))?;
    Ok((json, snap))
}

pub(crate) fn m2_verify() -> ExitCode {
    let root = root();
    let result = m2_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| m2::Golden::verify(&root, &json, &snap));
    report(
        result,
        &format!(
            "block_irq.elf matches {}, and {} restores to it",
            m2::GOLDEN_PATH,
            m2::MID_SNAPSHOT_PATH
        ),
    )
}

pub(crate) fn m2_emit(dir: &Path) -> ExitCode {
    let (golden, snapshot) = match m2::Golden::generate(&root()) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join(M2_RESULT), golden.render()))
        .and_then(|()| fs::write(dir.join(m2::MID_FILE), &snapshot))
    {
        eprintln!("cannot write into {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!(
        "wrote this machine's M2 result: {M2_RESULT} and {} ({} bytes, BLAKE3 {})",
        m2::MID_FILE,
        golden.mid.size,
        systemscope_rv32::hex(&golden.mid.blake3)
    );
    ExitCode::SUCCESS
}

pub(crate) fn m2_check(dir: &Path) -> ExitCode {
    let root = root();
    let result = m2_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| {
            let read_error = |e: std::io::Error| vec![format!("{}: {e}", dir.display())];
            let foreign_json = fs::read_to_string(dir.join(M2_RESULT)).map_err(read_error)?;
            let foreign_snap = fs::read(dir.join(m2::MID_FILE)).map_err(read_error)?;
            m2::Golden::check_foreign(&root, (&json, &snap), (&foreign_json, &foreign_snap))
        });
    report(
        result,
        "the foreign M2 result equals the committed golden files and this machine's run, \
         and its snapshot restores here to the golden end",
    )
}

pub(crate) fn m3_bless() -> ExitCode {
    let root = root();
    let json_path = root.join(m3g::GOLDEN_PATH);
    let snap_path = root.join(m3g::MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| m3g::Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running both M3 disks on m3-reference...");
    let (new, snapshot) = match m3g::Golden::generate(&root) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("not blessed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = new.render();

    let mut changes = m3g::describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {}: formatting only", m3g::GOLDEN_PATH));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {}: bytes differ from the recorded hash",
            m3g::MID_SNAPSHOT_PATH
        ));
    }
    if changes.is_empty() {
        println!("M3 golden files are up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the M3 golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("M3 golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {} and {}", m3g::GOLDEN_PATH, m3g::MID_SNAPSHOT_PATH);
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

/// The committed M3 golden file and portable snapshot.
pub(crate) fn m3_committed(root: &Path) -> Result<(String, Vec<u8>), String> {
    let json = fs::read_to_string(root.join(m3g::GOLDEN_PATH))
        .map_err(|e| format!("{}: {e}", m3g::GOLDEN_PATH))?;
    let snap = fs::read(root.join(m3g::MID_SNAPSHOT_PATH))
        .map_err(|e| format!("{}: {e}", m3g::MID_SNAPSHOT_PATH))?;
    Ok((json, snap))
}

pub(crate) fn m3_golden_verify() -> ExitCode {
    let root = root();
    if !m3_verify_fixtures(&root) {
        return ExitCode::FAILURE;
    }
    let result = m3_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| m3g::Golden::verify(&root, &json, &snap));
    report(
        result,
        &format!(
            "both M3 disks match {}, and {} restores to it",
            m3g::GOLDEN_PATH,
            m3g::MID_SNAPSHOT_PATH
        ),
    )
}

pub(crate) fn m3_emit(dir: &Path) -> ExitCode {
    let (golden, snapshot) = match m3g::Golden::generate(&root()) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join(M3_RESULT), golden.render()))
        .and_then(|()| fs::write(dir.join(m3g::MID_FILE), &snapshot))
    {
        eprintln!("cannot write into {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!(
        "wrote this machine's M3 result: {M3_RESULT} and {} ({} bytes, BLAKE3 {})",
        m3g::MID_FILE,
        golden.mid.size,
        systemscope_rv32::hex(&golden.mid.blake3)
    );
    ExitCode::SUCCESS
}

pub(crate) fn m3_check(dir: &Path) -> ExitCode {
    let root = root();
    let result = m3_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| {
            let read_error = |e: std::io::Error| vec![format!("{}: {e}", dir.display())];
            let foreign_json = fs::read_to_string(dir.join(M3_RESULT)).map_err(read_error)?;
            let foreign_snap = fs::read(dir.join(m3g::MID_FILE)).map_err(read_error)?;
            m3g::Golden::check_foreign(&root, (&json, &snap), (&foreign_json, &foreign_snap))
        });
    report(
        result,
        "the foreign M3 result equals the committed golden files and this machine's run, \
         and its snapshot restores here to the golden end",
    )
}

pub(crate) fn m5_bless() -> ExitCode {
    let root = root();
    let json_path = root.join(m5g::GOLDEN_PATH);
    let snap_path = root.join(m5g::MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running the M5 disk on m5-reference...");
    let (new, snapshot) = match m5g::Golden::generate(&root) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("not blessed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = new.render();
    let mut changes = m5g::describe_changes(old_json.as_deref(), &new);
    if old_json.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {}: formatting only", m5g::GOLDEN_PATH));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && changes.is_empty() {
        changes.push(format!(
            "~ {}: bytes differ from the recorded hash",
            m5g::MID_SNAPSHOT_PATH
        ));
    }
    if changes.is_empty() {
        println!("M5 golden files are up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the M5 golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("M5 golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {} and {}", m5g::GOLDEN_PATH, m5g::MID_SNAPSHOT_PATH);
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

/// The committed M5 golden file and mid snapshot.
pub(crate) fn m5_committed(root: &Path) -> Result<(String, Vec<u8>), String> {
    let json = fs::read_to_string(root.join(m5g::GOLDEN_PATH))
        .map_err(|e| format!("{}: {e}", m5g::GOLDEN_PATH))?;
    let snap = fs::read(root.join(m5g::MID_SNAPSHOT_PATH))
        .map_err(|e| format!("{}: {e}", m5g::MID_SNAPSHOT_PATH))?;
    Ok((json, snap))
}

pub(crate) fn m5_golden_verify() -> ExitCode {
    let root = root();
    if !m5_verify_fixtures(&root) {
        return ExitCode::FAILURE;
    }
    let result = m5_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| m5g::Golden::verify(&root, &json, &snap));
    report(
        result,
        &format!(
            "the M5 disk matches {}, and {} resumes to its end",
            m5g::GOLDEN_PATH,
            m5g::MID_SNAPSHOT_PATH
        ),
    )
}

pub(crate) fn m5_emit(dir: &Path) -> ExitCode {
    let (golden, snapshot) = match m5g::Golden::generate(&root()) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join(M5_RESULT), golden.render()))
        .and_then(|()| fs::write(dir.join(m5g::MID_FILE), &snapshot))
    {
        eprintln!("cannot write into {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!(
        "wrote this machine's M5 result: {M5_RESULT} and {} ({} bytes, BLAKE3 {})",
        m5g::MID_FILE,
        snapshot.len(),
        golden.get("mid_blake3").unwrap_or_default()
    );
    ExitCode::SUCCESS
}

pub(crate) fn m5_check(dir: &Path) -> ExitCode {
    let root = root();
    let result = m5_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| {
            let read_error = |e: std::io::Error| vec![format!("{}: {e}", dir.display())];
            let foreign_json = fs::read_to_string(dir.join(M5_RESULT)).map_err(read_error)?;
            let foreign_snap = fs::read(dir.join(m5g::MID_FILE)).map_err(read_error)?;
            m5g::Golden::check_foreign(&root, (&json, &snap), (&foreign_json, &foreign_snap))
        });
    report(
        result,
        "the foreign M5 result equals the committed golden files and this machine's run, \
         and its snapshot resumes here to the golden end",
    )
}

/// The checkpoints `m3-golden every-event` resumes.
pub(crate) enum Checkpoints {
    All,
    Range(std::ops::Range<usize>),
    /// Shard `.0` of `.1` shards of about equal cost.
    Shard(usize, usize),
}

pub(crate) fn m3_every_event(checkpoints: Checkpoints) -> ExitCode {
    let root = root();
    let result = M3Fixture::read(&root, Scenario::Reference).and_then(|fixture| {
        let reference = m3c::reference(&fixture)?;
        let all = 0..reference.events() + 1;
        let range = match checkpoints {
            Checkpoints::All => all,
            Checkpoints::Range(range) => range,
            Checkpoints::Shard(i, n) => m3c::balanced(all, reference.events(), n)
                .get(i)
                .cloned()
                .unwrap_or(0..0),
        };
        let threads = sweep::threads();
        eprintln!(
            "resuming checkpoints {}..{} of {} events on {threads} threads...",
            range.start,
            range.end,
            reference.events()
        );
        let sweep = m3c::every_event(&fixture, &reference, range, threads);
        println!(
            "{} checkpoints resumed, {} failed",
            sweep.checked,
            sweep.failures.len()
        );
        if sweep.checked == 0 {
            return Err("no checkpoint in range".to_owned());
        }
        match sweep.failures.first() {
            None => Ok(()),
            Some((k, e)) => Err(format!("first failure at checkpoint {k}: {e}")),
        }
    });
    report(
        result.map_err(|e| vec![e]),
        "every checkpoint replays the reference run without a reissue and ends in its state",
    )
}

/// The campaign mechanism a command-line letter names.
pub(crate) fn mechanism(letter: &str) -> Option<m5c::Mechanism> {
    match letter {
        "a" => Some(m5c::Mechanism::A),
        "b" => Some(m5c::Mechanism::B),
        "c" => Some(m5c::Mechanism::C),
        _ => None,
    }
}

/// `m5-golden every-event`, on the frozen platform or, with `mechanism`, on the one
/// perturbed by its seed 0.
pub(crate) fn m5_every_event(
    mechanism: Option<m5c::Mechanism>,
    checkpoints: Checkpoints,
) -> ExitCode {
    let result = m5g::Fixture::read(&root()).and_then(|fixture| {
        let config = match mechanism {
            None => systemscope_rv32::m5ref::Config::frozen(),
            Some(m) => m5v::defective_config(&m5c::perturbation(m, 0), None),
        };
        let events = match mechanism {
            None => m5g::reference_events(&fixture)?,
            Some(_) => m5g::events_under(&fixture, &config)?,
        };
        let all = 0..events + 1;
        let range = match checkpoints {
            Checkpoints::All => all,
            Checkpoints::Range(range) => range.start..range.end.min(events + 1),
            Checkpoints::Shard(i, n) => sweep::split(all, n).get(i).cloned().unwrap_or(0..0),
        };
        let threads = sweep::threads();
        eprintln!(
            "resuming checkpoints {}..{} of {events} events on {threads} threads...",
            range.start, range.end
        );
        let sweep = m5g::every_event_under(&fixture, &config, range, threads);
        println!(
            "{} checkpoints resumed, {} failed",
            sweep.checked,
            sweep.failures.len()
        );
        if sweep.checked == 0 {
            return Err("no checkpoint in range".to_owned());
        }
        match sweep.failures.first() {
            None => Ok(()),
            Some((k, e)) => Err(format!("first failure at checkpoint {k}: {e}")),
        }
    });
    report(
        result.map_err(|e| vec![e]),
        "every checkpoint restores, snapshots back to itself, and reaches the next one",
    )
}
