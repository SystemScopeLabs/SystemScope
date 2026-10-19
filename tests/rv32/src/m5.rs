//! The M5 workload on `m5-reference` (`docs/m5-design.md` §12).
//!
//! ```text
//! m5/firmware.S ─────────build-m5.sh (Linux)──▶ m5/m5-firmware.elf
//! m5/{counter,reader,spinner,handoff}.S ───────▶ m5/<name>.elf
//! the user ELFs + data blocks ──m5ref::disk_image──▶ m5/disk.img
//! a run of the disk ──m3::os_trace──▶ m5/os-trace.txt
//! all of it ──▶ m5/manifest.json
//! ```
//!
//! - The firmware is `m3-firmware.elf` with the SEI delegation and the idle action
//!   (§9, §11.3). It is the only thing the host places in RAM.
//! - The disk holds the six entries of §12.2 ([`ENTRIES`]): two `counter`s, two `reader`s,
//!   `spinner`, and `handoff`, then the readers' data blocks at [`m5ref::DATA_LBA`].
//! - The expected `os.*` sequence is a committed file keyed by the disk's `image_hash`,
//!   recorded from a run that passed every other check, and compared exactly.
//! - [`judge`] is the reference run's rule: the SRST halt with reason 0; the UART lines,
//!   each computed here from the programs' definitions; the `os.*` records; the final
//!   shared words `[2N, K, 0]`; one `EnvironmentCallFromU` per syscall and no other
//!   exception; no rejected command or bus fault; after shutdown every frame free and
//!   every process exited 0; and the [`Coverage`] the golden run must show.
//!
//! Tests only read the committed fixtures. `cargo xtask rv32-fixtures build` rebuilds
//! them on Linux, `cargo xtask m5-reference manifest` rewrites the disk, the trace, and
//! the manifest from the committed ELFs, and `cargo xtask m5-reference verify` checks
//! the manifest and runs the disk twice.

use std::fs;
use std::path::Path;

use serde_json::Value;
use systemscope_contracts::observe::Observer;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value as TraceValue};
use systemscope_elf::LoadImage;
use systemscope_os::interrupts::{BLOCK_KIND, FINAL_KIND, INTERRUPT_KIND, PREEMPT_KIND, WAKE_KIND};
use systemscope_os::procop::{EXIT_KIND, FAULT_KIND, SWITCH_KIND, SYSCALL_ENTER_KIND};
use systemscope_platform::bus::FAULT_KIND as BUS_FAULT_KIND;
use systemscope_platform::dma::{COMMAND_KIND, REJECTED_KIND};
use systemscope_platform::uart::TX_KIND;
use systemscope_rv32i::cpu::{EXCEPTION_KIND, INTERRUPT_KIND as CPU_INTERRUPT_KIND};

use crate::hello::uart_output;
use crate::m3::{
    FIRMWARE_FLAGS, HALT_CAUSE, Pinned, PinnedDisk, USER_FLAGS, is_os_record, os_trace,
};
use crate::m3ref::{POOL_FRAMES, SRST};
use crate::m5ref::{self, CPU, Config, DATA_BLOCKS, DATA_LBA, KERNEL};
use crate::manifest::{Tool, addr, array, digest, dir_entries, load, q, s};
use crate::runner::{self, End, Finished, Start};
use crate::{RAM_BASE, RAM_SIZE, TOOLCHAIN, TOOLCHAIN_DISTRO, hex, unhex32};

/// The directory holding the sources, the build script, the ELFs, the disk, the expected
/// trace, and the manifest, relative to the workspace root.
pub const M5_DIR: &str = "tests/rv32/m5";
/// The firmware's file name in [`M5_DIR`].
pub const FIRMWARE_ELF: &str = "m5-firmware.elf";
/// The user programs, each `<name>.elf` in [`M5_DIR`].
pub const PROGRAMS: [&str; 4] = ["counter", "reader", "spinner", "handoff"];
/// The disk's entries in table order (§12.2).
pub const ENTRIES: [&str; 6] = [
    "counter", "counter", "reader", "reader", "spinner", "handoff",
];
/// The disk's file name in [`M5_DIR`].
pub const DISK_FILE: &str = "disk.img";
/// The expected trace's file name in [`M5_DIR`].
pub const TRACE_FILE: &str = "os-trace.txt";
/// The manifest, relative to the workspace root.
pub const M5_MANIFEST: &str = "tests/rv32/m5/manifest.json";
/// The build script, relative to the workspace root.
pub const M5_SCRIPT: &str = "tests/rv32/m5/build-m5.sh";
/// The files the ELFs are built from besides the toolchain, relative to the workspace
/// root.
pub const M5_INPUTS: [&str; 9] = [
    "tests/rv32/m5/build-m5.sh",
    "tests/rv32/m5/counter.S",
    "tests/rv32/m5/firmware.S",
    "tests/rv32/m5/firmware.ld",
    "tests/rv32/m5/handoff.S",
    "tests/rv32/m5/reader.S",
    "tests/rv32/m5/spinner.S",
    "tests/rv32/m5/user-bss.ld",
    "tests/rv32/m5/user.ld",
];
/// Everything [`M5_DIR`] holds, in name order.
pub const M5_FILES: [&str; 17] = [
    "build-m5.sh",
    "counter.S",
    "counter.elf",
    "disk.img",
    "firmware.S",
    "firmware.ld",
    "handoff.S",
    "handoff.elf",
    "m5-firmware.elf",
    "manifest.json",
    "os-trace.txt",
    "reader.S",
    "reader.elf",
    "spinner.S",
    "spinner.elf",
    "user-bss.ld",
    "user.ld",
];

/// The manifest format version.
pub const SCHEMA: u64 = 1;

/// The most events a run may take; a run that never shuts down stops here.
pub const EVENT_BUDGET: u64 = 3_000_000;

/// The first line of the expected trace file.
pub const TRACE_HEADER: &str = "# m5-reference os.* trace (docs/m5-design.md §12.2)";

/// `counter`'s `ROUNDS`: each instance's increments.
pub const COUNTER_ROUNDS: u32 = 150;
/// `handoff`'s `COUNT`: the values sent through the mailbox.
pub const HANDOFF_COUNT: u32 = 8;
/// `spinner`'s `STEPS`: its xorshift steps.
pub const SPINNER_STEPS: u32 = 500;
/// `spinner`'s `SEED`.
pub const SPINNER_SEED: u32 = 0x9E37_79B9;

/// The least preemptions of each `counter` a reference run must show.
pub const MIN_COUNTER_PREEMPTS: usize = 5;

/// The final shared words: the counter, the mailbox's data, and its flag.
pub fn expected_final() -> [u64; 3] {
    [u64::from(2 * COUNTER_ROUNDS), u64::from(HANDOFF_COUNT), 0]
}

fn mix(d: u32, v: u32) -> u32 {
    d.wrapping_mul(31).wrapping_add(v)
}

/// The reads `reader` makes of the block at `lba`: one for the first, two for the
/// second.
pub fn reader_reads(lba: u32) -> u32 {
    lba - DATA_LBA + 1
}

/// `reader`'s line for the block at `lba`.
pub fn reader_line(lba: u32) -> String {
    let block = m5ref::data_block(lba);
    let mut d = 0u32;
    for _ in 0..reader_reads(lba) {
        d = block.iter().fold(d, |d, &b| mix(d, u32::from(b)));
    }
    format!("reader {d:08x}\n")
}

/// `spinner`'s line.
pub fn spinner_line() -> String {
    let d = (1..=HANDOFF_COUNT).fold(0, mix);
    let mut x = SPINNER_SEED;
    for _ in 0..SPINNER_STEPS {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
    }
    format!("spinner {d:08x} {x:08x}\n")
}

/// The lines the UART must print, sorted: each program prints only its own line, so
/// their order is the schedule's, which the expected trace pins.
pub fn expected_lines() -> Vec<String> {
    let mut lines: Vec<String> = (DATA_LBA..DATA_LBA + DATA_BLOCKS)
        .map(reader_line)
        .chain([spinner_line()])
        .collect();
    lines.sort();
    lines
}

/// Reads the firmware under `root` as the host loader places it.
pub fn read_firmware(root: &Path) -> Result<LoadImage, String> {
    let path = root.join(M5_DIR).join(FIRMWARE_ELF);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    load("m5-firmware", &bytes)
}

/// The disk from the user ELFs under `root`: [`ENTRIES`] in table order and the data
/// blocks.
pub fn disk(root: &Path) -> Result<Vec<u8>, String> {
    let programs = ENTRIES
        .iter()
        .map(|name| {
            let path = root.join(M5_DIR).join(format!("{name}.elf"));
            fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let files: Vec<&[u8]> = programs.iter().map(Vec::as_slice).collect();
    m5ref::disk_image(&files)
}

/// Writes the disk under `root`, as `cargo xtask rv32-fixtures build` does after the
/// ELFs.
pub fn write_disk(root: &Path) -> Result<(), String> {
    let path = root.join(M5_DIR).join(DISK_FILE);
    fs::write(&path, disk(root)?).map_err(|e| format!("{}: {e}", path.display()))
}

/// The expected trace file: [`TRACE_HEADER`], the disk's name and `image_hash`, then the
/// records.
pub fn render_trace_file(disk_hash: &[u8; 32], records: &str) -> String {
    format!(
        "{TRACE_HEADER}\n# disk {DISK_FILE} image_hash {}\n{records}",
        hex(disk_hash)
    )
}

/// Reads [`render_trace_file`]'s output: the disk's `image_hash` and the records.
pub fn parse_trace_file(text: &str) -> Result<([u8; 32], String), String> {
    let mut lines = text.splitn(3, '\n');
    if lines.next() != Some(TRACE_HEADER) {
        return Err(format!("{TRACE_FILE}: no header"));
    }
    let key = lines.next().unwrap_or_default();
    let prefix = format!("# disk {DISK_FILE} image_hash ");
    let hash = key
        .strip_prefix(&prefix)
        .and_then(unhex32)
        .ok_or_else(|| format!("{TRACE_FILE}: the second line is {key:?}"))?;
    Ok((hash, lines.next().unwrap_or_default().to_owned()))
}

/// Reads the expected trace under `root`, provided its key is `disk`'s `image_hash`.
pub fn read_expected_trace(root: &Path, disk: &[u8]) -> Result<String, String> {
    let path = root.join(M5_DIR).join(TRACE_FILE);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (hash, records) = parse_trace_file(&text)?;
    let actual = *blake3::hash(disk).as_bytes();
    if hash != actual {
        return Err(format!(
            "{TRACE_FILE} is keyed by image_hash {}, the disk's is {}",
            hex(&hash),
            hex(&actual)
        ));
    }
    Ok(records)
}

/// Runs the disk under `root` and writes its expected trace. The run must pass every
/// other check first.
pub fn write_trace(root: &Path) -> Result<(), String> {
    let firmware = read_firmware(root)?;
    let disk = disk(root)?;
    let run = run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
    judge_without_trace(&run)?;
    let records = os_trace(run.finished.trace.as_ref().expect("traced"));
    let text = render_trace_file(blake3::hash(&disk).as_bytes(), &records);
    let path = root.join(M5_DIR).join(TRACE_FILE);
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The contents of [`M5_MANIFEST`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M5Manifest {
    /// [`SCHEMA`].
    pub schema: u64,
    /// Where the toolchain packages come from.
    pub distro: String,
    /// The toolchain packages.
    pub toolchain: Vec<Tool>,
    /// The firmware's compiler flags.
    pub firmware_flags: Vec<String>,
    /// The user programs' compiler flags.
    pub user_flags: Vec<String>,
    /// The RAM base: the firmware's entry point.
    pub ram_base: u32,
    /// The RAM size the firmware is checked against.
    pub ram_size: u32,
    /// BLAKE3 of each build input in [`M5_INPUTS`].
    pub inputs: Vec<(String, [u8; 32])>,
    /// The firmware ELF.
    pub firmware: Pinned,
    /// The loader's `image_hash` of the firmware.
    pub firmware_image_hash: [u8; 32],
    /// The firmware's entry point.
    pub firmware_entry: u32,
    /// The user ELFs, in [`PROGRAMS`] order.
    pub programs: Vec<Pinned>,
    /// The disk.
    pub disk: PinnedDisk,
    /// The expected trace.
    pub trace: Pinned,
}

fn pin(root: &Path, file: &str) -> Result<(Pinned, Vec<u8>), String> {
    let path = root.join(M5_DIR).join(file);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((
        Pinned {
            file: file.to_owned(),
            blake3: *blake3::hash(&bytes).as_bytes(),
        },
        bytes,
    ))
}

impl M5Manifest {
    /// The manifest for the files under `root`, with today's pins.
    pub fn generate(root: &Path) -> Result<M5Manifest, String> {
        let inputs = M5_INPUTS
            .iter()
            .map(|&path| {
                let bytes = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                Ok((path.to_owned(), *blake3::hash(&bytes).as_bytes()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let (firmware, bytes) = pin(root, FIRMWARE_ELF)?;
        let image = load("m5-firmware", &bytes)?;
        let programs = PROGRAMS
            .iter()
            .map(|name| Ok(pin(root, &format!("{name}.elf"))?.0))
            .collect::<Result<Vec<_>, String>>()?;
        let (disk, bytes) = pin(root, DISK_FILE)?;
        if !bytes.len().is_multiple_of(512) {
            return Err(format!(
                "{DISK_FILE} is {} bytes, not a whole number of blocks",
                bytes.len()
            ));
        }
        Ok(M5Manifest {
            schema: SCHEMA,
            distro: TOOLCHAIN_DISTRO.to_owned(),
            toolchain: TOOLCHAIN
                .iter()
                .map(|p| Tool {
                    name: p.name.to_owned(),
                    version: p.version.to_owned(),
                    deb_sha256: p.deb_sha256.to_owned(),
                    version_line: p.version_line.to_owned(),
                })
                .collect(),
            firmware_flags: FIRMWARE_FLAGS.iter().map(|&f| f.to_owned()).collect(),
            user_flags: USER_FLAGS.iter().map(|&f| f.to_owned()).collect(),
            ram_base: RAM_BASE,
            ram_size: RAM_SIZE,
            inputs,
            firmware,
            firmware_image_hash: image.image_hash,
            firmware_entry: image.entry,
            programs,
            disk: PinnedDisk {
                file: disk.file,
                blocks: (bytes.len() / 512) as u64,
                programs: ENTRIES.iter().map(|&p| p.to_owned()).collect(),
                blake3: disk.blake3,
            },
            trace: pin(root, TRACE_FILE)?.0,
        })
    }

    /// Reads the committed manifest under `root`.
    pub fn read(root: &Path) -> Result<M5Manifest, String> {
        let text = fs::read_to_string(root.join(M5_MANIFEST))
            .map_err(|e| format!("{M5_MANIFEST}: {e}"))?;
        M5Manifest::parse(&text).map_err(|e| format!("{M5_MANIFEST}: {e}"))
    }

    /// The file contents: stable, pretty-printed JSON with a trailing newline.
    pub fn render(&self) -> String {
        let tools: Vec<String> = self
            .toolchain
            .iter()
            .map(|t| {
                format!(
                    "      {{\n        \"name\": {},\n        \"version\": {},\n        \
                     \"deb_sha256\": {},\n        \"version_line\": {}\n      }}",
                    q(&t.name),
                    q(&t.version),
                    q(&t.deb_sha256),
                    q(&t.version_line)
                )
            })
            .collect();
        let flags = |fs: &[String]| -> String {
            fs.iter()
                .map(|f| format!("    {}", q(f)))
                .collect::<Vec<_>>()
                .join(",\n")
        };
        let pinned = |p: &Pinned| {
            format!(
                "{{ \"file\": {}, \"blake3\": {} }}",
                q(&p.file),
                q(&hex(&p.blake3))
            )
        };
        let programs: Vec<String> = self
            .programs
            .iter()
            .map(|p| format!("    {}", pinned(p)))
            .collect();
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .map(|(path, hash)| {
                format!(
                    "    {{ \"path\": {}, \"blake3\": {} }}",
                    q(path),
                    q(&hex(hash))
                )
            })
            .collect();
        let d = &self.disk;
        let entries: Vec<String> = d.programs.iter().map(|p| q(p)).collect();
        format!(
            "{{\n  \"schema\": {},\n  \"toolchain\": {{\n    \"distro\": {},\n    \
             \"packages\": [\n{}\n    ]\n  }},\n  \"firmware_flags\": [\n{}\n  ],\n  \
             \"user_flags\": [\n{}\n  ],\n  \
             \"ram\": {{ \"base\": {}, \"size\": {} }},\n  \"inputs\": [\n{}\n  ],\n  \
             \"firmware\": {{ \"file\": {}, \"blake3\": {}, \"image_hash\": {}, \"entry\": {} }},\n  \
             \"programs\": [\n{}\n  ],\n  \
             \"disk\": {{ \"file\": {}, \"blocks\": {}, \"programs\": [{}], \"blake3\": {} }},\n  \
             \"trace\": {}\n}}\n",
            self.schema,
            q(&self.distro),
            tools.join(",\n"),
            flags(&self.firmware_flags),
            flags(&self.user_flags),
            q(&format!("{:#010x}", self.ram_base)),
            q(&format!("{:#010x}", self.ram_size)),
            inputs.join(",\n"),
            q(&self.firmware.file),
            q(&hex(&self.firmware.blake3)),
            q(&hex(&self.firmware_image_hash)),
            q(&format!("{:#010x}", self.firmware_entry)),
            programs.join(",\n"),
            q(&d.file),
            d.blocks,
            entries.join(", "),
            q(&hex(&d.blake3)),
            pinned(&self.trace),
        )
    }

    /// Reads [`M5Manifest::render`]'s output.
    pub fn parse(json: &str) -> Result<M5Manifest, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let schema = root["schema"].as_u64().ok_or("no schema")?;
        if schema != SCHEMA {
            return Err(format!("schema {schema}, expected {SCHEMA}"));
        }
        let toolchain = &root["toolchain"];
        let strings =
            |v: &Value| -> Result<Vec<String>, String> { array(v)?.iter().map(s).collect() };
        let pinned = |v: &Value| -> Result<Pinned, String> {
            Ok(Pinned {
                file: s(&v["file"])?,
                blake3: digest(&v["blake3"])?,
            })
        };
        let firmware = &root["firmware"];
        let disk = &root["disk"];
        Ok(M5Manifest {
            schema,
            distro: s(&toolchain["distro"])?,
            toolchain: array(&toolchain["packages"])?
                .iter()
                .map(|t| {
                    Ok(Tool {
                        name: s(&t["name"])?,
                        version: s(&t["version"])?,
                        deb_sha256: s(&t["deb_sha256"])?,
                        version_line: s(&t["version_line"])?,
                    })
                })
                .collect::<Result<_, String>>()?,
            firmware_flags: strings(&root["firmware_flags"])?,
            user_flags: strings(&root["user_flags"])?,
            ram_base: addr(&root["ram"]["base"])?,
            ram_size: addr(&root["ram"]["size"])?,
            inputs: array(&root["inputs"])?
                .iter()
                .map(|i| Ok((s(&i["path"])?, digest(&i["blake3"])?)))
                .collect::<Result<_, String>>()?,
            firmware: pinned(firmware)?,
            firmware_image_hash: digest(&firmware["image_hash"])?,
            firmware_entry: addr(&firmware["entry"])?,
            programs: array(&root["programs"])?
                .iter()
                .map(pinned)
                .collect::<Result<_, String>>()?,
            disk: PinnedDisk {
                file: s(&disk["file"])?,
                blocks: disk["blocks"].as_u64().ok_or("no disk block count")?,
                programs: strings(&disk["programs"])?,
                blake3: digest(&disk["blake3"])?,
            },
            trace: pinned(&root["trace"])?,
        })
    }
}

/// Checks the committed M5 fixtures without a network, a compiler, or a run: the
/// manifest equals the one generated from the files on disk, byte for byte; the disk is
/// exactly the storage image of its entries and data blocks; the expected trace is keyed
/// by the disk's `image_hash`; and [`M5_DIR`] holds exactly [`M5_FILES`].
pub fn verify(root: &Path) -> Result<M5Manifest, Vec<String>> {
    let committed = M5Manifest::read(root).map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    match M5Manifest::generate(root) {
        Err(e) => errors.push(e),
        Ok(actual) if actual != committed => errors.push(format!(
            "{M5_MANIFEST} differs from the files on disk; rebuild the M5 fixtures:\n  \
             manifest {committed:?}\n  actual   {actual:?}"
        )),
        Ok(actual) => {
            let text = fs::read_to_string(root.join(M5_MANIFEST)).unwrap_or_default();
            if text != actual.render() {
                errors.push(format!(
                    "{M5_MANIFEST} is not formatted as `cargo xtask rv32-fixtures` writes it"
                ));
            }
        }
    }
    let path = root.join(M5_DIR).join(DISK_FILE);
    let read = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()));
    match (read, disk(root)) {
        (Err(e), _) | (_, Err(e)) => errors.push(e),
        (Ok(bytes), Ok(expected)) => {
            if bytes != expected {
                errors.push(format!(
                    "{DISK_FILE} is not the storage image of {ENTRIES:?} and the data blocks"
                ));
            }
            if let Err(e) = read_expected_trace(root, &bytes) {
                errors.push(e);
            }
        }
    }
    match dir_entries(&root.join(M5_DIR)) {
        Err(e) => errors.push(e),
        Ok(entries) if entries != M5_FILES => errors.push(format!(
            "{M5_DIR} holds {entries:?}, not exactly {M5_FILES:?}"
        )),
        Ok(_) => {}
    }
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

/// The verified fixture under `root`: the firmware, the disk, and the expected `os.*`
/// records, each checked against the manifest.
pub fn read_fixture(root: &Path) -> Result<(LoadImage, Vec<u8>, String), String> {
    let manifest = M5Manifest::read(root)?;
    let (firmware, bytes) = pin(root, FIRMWARE_ELF)?;
    if firmware != manifest.firmware {
        return Err(format!("{FIRMWARE_ELF} differs from the manifest"));
    }
    let image = load("m5-firmware", &bytes)?;
    if image.image_hash != manifest.firmware_image_hash || image.entry != manifest.firmware_entry {
        return Err(format!(
            "{FIRMWARE_ELF}: the loader's image differs from the manifest"
        ));
    }
    let (disk, bytes) = pin(root, DISK_FILE)?;
    if manifest.disk.blake3 != disk.blake3 || manifest.disk.blocks * 512 != bytes.len() as u64 {
        return Err(format!("{DISK_FILE} differs from the manifest"));
    }
    let (trace, _) = pin(root, TRACE_FILE)?;
    if trace != manifest.trace {
        return Err(format!("{TRACE_FILE} differs from the manifest"));
    }
    let records = read_expected_trace(root, &bytes)?;
    Ok((image, bytes, records))
}

/// A run of the workload and what it left behind.
#[derive(Debug)]
pub struct M5Run {
    /// The session, run to its end.
    pub finished: Finished,
    /// The runtime snapshot after the last event; `None` if the session faulted.
    pub snapshot: Option<Vec<u8>>,
    /// The UART's output, from its view after the last event.
    pub output: Result<Vec<u8>, String>,
}

/// Runs `firmware` with the raw disk image `disk` on `m5-reference` configured as
/// `config`, started as `start` says, for at most [`EVENT_BUDGET`] events.
pub fn run_with(
    firmware: &LoadImage,
    disk: &[u8],
    config: &Config,
    start: Start,
    observers: Vec<Box<dyn Observer>>,
) -> M5Run {
    let rt = m5ref::build(firmware, disk, config).expect("the platform builds");
    let (finished, rt) = runner::execute_bounded(rt, start, observers, Some(EVENT_BUDGET));
    M5Run {
        snapshot: rt.snapshot().ok(),
        output: uart_output(&finished.views),
        finished,
    }
}

/// Runs `firmware` with `disk` on the frozen `m5-reference`.
pub fn run(
    firmware: &LoadImage,
    disk: &[u8],
    start: Start,
    observers: Vec<Box<dyn Observer>>,
) -> M5Run {
    run_with(firmware, disk, &Config::frozen(), start, observers)
}

fn register(run: &M5Run, name: &str) -> Option<u64> {
    match run.finished.views.get(CPU.0 as usize)?.get(name) {
        Some(TraceValue::U64(v)) => Some(*v),
        _ => None,
    }
}

fn kernel_field<'a>(run: &'a M5Run, name: &str) -> Option<&'a TraceValue> {
    run.finished.views.get(KERNEL.0 as usize)?.get(name)
}

fn field<'a>(r: &'a TraceRecord, name: &str) -> Option<&'a TraceValue> {
    r.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
}

fn text(r: &TraceRecord, name: &str) -> String {
    match field(r, name) {
        Some(TraceValue::Str(s)) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn number(r: &TraceRecord, name: &str) -> Option<u64> {
    match field(r, name) {
        Some(TraceValue::U64(v)) => Some(*v),
        Some(TraceValue::I64(v)) => u64::try_from(*v).ok(),
        _ => None,
    }
}

/// What the reference run must exercise, counted from its trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Preemptions of each `counter` instance (pids 1 and 2).
    pub counter_preempts: [usize; 2],
    /// The longest the `blk_read` FIFO got: blocked processes not yet woken.
    pub max_blocked: usize,
    /// Interrupts taken from S: the idle spin.
    pub idle_interrupts: usize,
    /// Interrupt traps that found both sources pending.
    pub both_sources: usize,
}

impl Coverage {
    /// The coverage of a traced run's component records.
    pub fn of(records: &[&TraceRecord]) -> Coverage {
        let mut c = Coverage::default();
        let mut blocked = 0usize;
        for r in records {
            match r.kind {
                PREEMPT_KIND => {
                    if let Some(pid @ 1..=2) = number(r, "pid") {
                        c.counter_preempts[pid as usize - 1] += 1;
                    }
                }
                BLOCK_KIND => {
                    blocked += 1;
                    c.max_blocked = c.max_blocked.max(blocked);
                }
                WAKE_KIND => blocked = blocked.saturating_sub(1),
                INTERRUPT_KIND if number(r, "sources") == Some(0b11) => c.both_sources += 1,
                CPU_INTERRUPT_KIND if text(r, "from") == "S" => c.idle_interrupts += 1,
                _ => {}
            }
        }
        c
    }

    /// Why the coverage falls short of a reference run's, if it does.
    pub fn check(&self) -> Result<(), String> {
        if self
            .counter_preempts
            .iter()
            .any(|&n| n < MIN_COUNTER_PREEMPTS)
            || self.max_blocked < 2
            || self.idle_interrupts == 0
            || self.both_sources == 0
        {
            return Err(format!(
                "the run does not cover the workload: {self:?}; it needs at least \
                 {MIN_COUNTER_PREEMPTS} preemptions of each counter, two blocked readers \
                 at once, an idle interrupt, and an interrupt with both sources"
            ));
        }
        Ok(())
    }
}

/// The run's component records.
fn component_records(run: &M5Run) -> Result<Vec<&TraceRecord>, String> {
    let trace = run
        .finished
        .trace
        .as_ref()
        .ok_or("the run was not traced")?;
    Ok(trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component)
        .collect())
}

/// Every check [`judge`] makes but the equality of the `os.*` records with the committed
/// ones.
pub fn judge_without_trace(run: &M5Run) -> Result<(), String> {
    match &run.finished.outcome.end {
        End::Trap { cause, .. } if cause == HALT_CAUSE => {}
        other => return Err(format!("ended with {other}, not Trap({HALT_CAUSE})")),
    }
    let regs = [
        ("a7", "x17", u64::from(SRST)),
        ("a6", "x16", 0),
        ("a0", "x10", 0),
        ("a1", "x11", 0),
    ];
    for (abi, reg, want) in regs {
        let got = register(run, reg);
        if got != Some(want) {
            return Err(format!("{abi} = {got:?} at the halt, not {want:#x}"));
        }
    }
    let output = run.output.as_ref().map_err(Clone::clone)?;
    let printed = String::from_utf8_lossy(output);
    let mut lines: Vec<String> = printed.split_inclusive('\n').map(str::to_owned).collect();
    lines.sort();
    if lines != expected_lines() {
        return Err(format!(
            "the UART printed {printed:?}, not the lines {:?}",
            expected_lines()
        ));
    }
    let records = component_records(run)?;
    let of = |kind: &str| -> Vec<&TraceRecord> {
        records.iter().copied().filter(|r| r.kind == kind).collect()
    };
    let exceptions = of(EXCEPTION_KIND);
    let syscalls = of(SYSCALL_ENTER_KIND).len();
    let calls = exceptions
        .iter()
        .filter(|e| text(e, "cause") == "EnvironmentCallFromU" && text(e, "from") == "U")
        .count();
    if calls != syscalls || exceptions.len() != calls {
        return Err(format!(
            "{calls} EnvironmentCallFromU exceptions for {syscalls} syscalls, of {} \
             exceptions",
            exceptions.len()
        ));
    }
    for kind in [FAULT_KIND, REJECTED_KIND, BUS_FAULT_KIND] {
        if let Some(r) = of(kind).first() {
            return Err(format!("the run traced {r:?}"));
        }
    }
    let tx = of(TX_KIND).len();
    if tx != output.len() {
        return Err(format!("{tx} UART TX records for {} bytes", output.len()));
    }
    let exits: Vec<(Option<u64>, Option<u64>)> = of(EXIT_KIND)
        .iter()
        .map(|r| (number(r, "pid"), number(r, "status")))
        .collect();
    if exits.len() != ENTRIES.len() || exits.iter().any(|&(_, status)| status != Some(0)) {
        return Err(format!("the exits are {exits:?}"));
    }
    let finals: Vec<(Option<u64>, Option<u64>)> = of(FINAL_KIND)
        .iter()
        .map(|r| (number(r, "word"), number(r, "value")))
        .collect();
    let want: Vec<(Option<u64>, Option<u64>)> = expected_final()
        .iter()
        .enumerate()
        .map(|(i, &v)| (Some(i as u64), Some(v)))
        .collect();
    if finals != want {
        return Err(format!("the final words are {finals:?}, not {want:?}"));
    }
    Coverage::of(&records).check()?;
    // After shutdown: every frame free, every process exited, nothing queued or blocked.
    let free = kernel_field(run, "free_frames");
    if free != Some(&TraceValue::U64(POOL_FRAMES)) {
        return Err(format!(
            "{free:?} free frames after shutdown, not {POOL_FRAMES}"
        ));
    }
    let state = |name: &str| match kernel_field(run, name) {
        Some(TraceValue::Str(s)) => s.clone(),
        other => format!("{other:?}"),
    };
    if (
        state("life").as_str(),
        state("running").as_str(),
        state("queue").as_str(),
    ) != ("down", "none", "")
    {
        return Err(format!(
            "the kernel is {}, running {}, queue {:?} after shutdown",
            state("life"),
            state("running"),
            state("queue")
        ));
    }
    let processes = state("processes");
    let table: Vec<&str> = processes.split(';').collect();
    if table.len() != ENTRIES.len() || !table.iter().all(|p| p.contains(":exited(0)")) {
        return Err(format!("the processes after shutdown are {processes:?}"));
    }
    Ok(())
}

/// The reference run's rule: `Ok` if the run passed, otherwise why it failed. `expected`
/// is the committed `os.*` records.
pub fn judge(run: &M5Run, expected: &str) -> Result<(), String> {
    judge_without_trace(run)?;
    let actual = os_trace(run.finished.trace.as_ref().expect("checked"));
    if actual != expected {
        let line = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| actual.lines().count().min(expected.lines().count()));
        return Err(format!(
            "the os.* records differ from the committed ones at record {}: {:?} vs {:?}",
            line + 1,
            actual.lines().nth(line),
            expected.lines().nth(line)
        ));
    }
    Ok(())
}

/// What a run did, counted from its outcome and trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// Events dispatched.
    pub events: u64,
    /// Instructions retired.
    pub instret: u64,
    /// `os.*` records.
    pub os_records: usize,
    /// `os.syscall.enter` records.
    pub syscalls: usize,
    /// `os.process.switch` records, the first dispatch included.
    pub switches: usize,
    /// `os.process.preempt` records.
    pub preempts: usize,
    /// `os.interrupt` records.
    pub interrupts: usize,
    /// Block controller commands.
    pub dma_commands: usize,
    /// UART bytes.
    pub uart_bytes: usize,
    /// The coverage of the run.
    pub coverage: Coverage,
    /// `ExecutionDigest`.
    pub execution: [u8; 32],
    /// `StateDigest`, if the session did not fault.
    pub state: Option<[u8; 32]>,
    /// `TraceDigest`, if the run was traced.
    pub trace: Option<[u8; 32]>,
}

impl Metrics {
    /// The metrics of `run`.
    pub fn of(run: &M5Run) -> Metrics {
        let outcome = &run.finished.outcome;
        let records = component_records(run).unwrap_or_default();
        let count = |pred: &dyn Fn(&TraceRecord) -> bool| {
            run.finished
                .trace
                .as_ref()
                .map_or(0, |t| t.records.iter().filter(|r| pred(r)).count())
        };
        let kind = |k: &'static str| {
            move |r: &TraceRecord| r.origin == TraceOrigin::Component && r.kind == k
        };
        Metrics {
            events: outcome.events,
            instret: outcome.instret,
            os_records: count(&is_os_record),
            syscalls: count(&kind(SYSCALL_ENTER_KIND)),
            switches: count(&kind(SWITCH_KIND)),
            preempts: count(&kind(PREEMPT_KIND)),
            interrupts: count(&kind(INTERRUPT_KIND)),
            dma_commands: count(&kind(COMMAND_KIND)),
            uart_bytes: count(&kind(TX_KIND)),
            coverage: Coverage::of(&records),
            execution: outcome.execution,
            state: outcome.state,
            trace: outcome.trace,
        }
    }

    /// One line for a report.
    pub fn line(&self) -> String {
        let c = &self.coverage;
        format!(
            "events {} instret {} os-records {} syscalls {} switches {} preempts {} \
             interrupts {} dma-commands {} uart-bytes {} counter-preempts {}/{} \
             max-blocked {} idle-interrupts {} both-sources {} execution {} state {} trace {}",
            self.events,
            self.instret,
            self.os_records,
            self.syscalls,
            self.switches,
            self.preempts,
            self.interrupts,
            self.dma_commands,
            self.uart_bytes,
            c.counter_preempts[0],
            c.counter_preempts[1],
            c.max_blocked,
            c.idle_interrupts,
            c.both_sources,
            hex(&self.execution),
            self.state.map_or("none".to_owned(), |s| hex(&s)),
            self.trace.map_or("none".to_owned(), |t| hex(&t)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_are_in_name_order_and_hold_every_input_and_output() {
        let mut sorted = M5_FILES;
        sorted.sort();
        assert_eq!(sorted, M5_FILES);
        for input in M5_INPUTS {
            let name = input.strip_prefix("tests/rv32/m5/").unwrap();
            assert!(M5_FILES.contains(&name), "{name}");
        }
        for name in [DISK_FILE, TRACE_FILE, FIRMWARE_ELF] {
            assert!(M5_FILES.contains(&name), "{name}");
        }
        for name in PROGRAMS {
            assert!(M5_FILES.contains(&format!("{name}.elf").as_str()));
            assert!(ENTRIES.contains(&name));
        }
    }

    #[test]
    fn the_expected_lines_follow_the_programs() {
        assert_eq!(reader_line(200).len(), 16);
        assert_ne!(reader_line(200), reader_line(201));
        assert_eq!((reader_reads(200), reader_reads(201)), (1, 2));
        let spinner = spinner_line();
        assert_eq!(spinner.len(), 26);
        // The UART's view holds every byte.
        assert!(expected_lines().concat().len() <= 64);
        // 1..=8 folded by d = 31d + v.
        let d = (1..=8u32).fold(0u32, |d, v| d.wrapping_mul(31).wrapping_add(v));
        assert!(spinner.starts_with(&format!("spinner {d:08x} ")));
        assert_eq!(expected_lines().len(), 3);
        assert_eq!(expected_final(), [300, 8, 0]);
    }

    #[test]
    fn a_trace_file_round_trips_and_names_its_disk() {
        let hash = [7u8; 32];
        let text = render_trace_file(&hash, "os.boot entries=6\n");
        assert_eq!(
            parse_trace_file(&text),
            Ok((hash, "os.boot entries=6\n".to_owned()))
        );
        assert!(parse_trace_file("os.boot\n").is_err());
    }
}
