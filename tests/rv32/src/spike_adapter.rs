//! Spike under the M4 verification engine (`docs/m4-design.md` §8.2, §9.2, §28.1): the
//! Spike adapter, a post-run backend over the existing strict commit-log parsers; the
//! four Spike profile versions, whose rules are the existing differentials'; the
//! SystemScope side, a runtime backend on the runner's platform; and SK-1, the Spike-side
//! controlled divergence (§25.1).
//!
//! The adapter never re-executes and shares no decoding code with the SystemScope side
//! (§8.4): Spike's stream is read from its log alone.

use std::fs;
use std::path::{Path, PathBuf};

use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_runtime::snapshot::component_states;
use systemscope_rv32i::Rv32iProfile;
use systemscope_verify::backend::{Backend, Capabilities, Capability, Side};
use systemscope_verify::bundle::{Bundle, Case, read_dir, replay, write_dir};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario, StatePatch};
use systemscope_verify::compare::{Comparison, Divergence, StreamResult, Verdict};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{Located, locate};
use systemscope_verify::minimize::{Minimized, minimize};
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Plan, Requirement, StreamSpec, Support, plan,
};
use systemscope_verify::report::{Builder, Make};
use systemscope_verify::result::VerificationResult;
use systemscope_verify::runtime::RuntimeBackend;

use crate::manifest::load;
use crate::projection::{EXCEPTION_FIELDS, HALT_TRAP, RETIRE_FIELDS, Rv32Projection, WRITE_TOHOST};
use crate::runner::{self, CPU, SEED};
use crate::spike::{
    Event, Mem, Retire, SPIKE_CAUSES, SPIKE_CAUSES_M3, SPIKE_COMMIT, SPIKE_INSTRUCTIONS_M3,
    SPIKE_ISA, SPIKE_ISA_M2, SPIKE_PRIV, SPIKE_PRIV_M3, SpikeTrap, elf_symbol, parse_spike_l_log,
    parse_spike_log, parse_spike_m3_log, run_spike, spike_args,
};
use crate::{FIXTURE_DIR, MAX_INSTRUCTIONS, hex};

/// The SystemScope side's safety budget, in events: far above any program's run to its
/// instruction limit.
pub const EVENT_BUDGET: u64 = 64 * MAX_INSTRUCTIONS;

/// The four Spike profile versions: the existing differentials' rules, unchanged
/// (§6.5, §8.2). The first comparison is at the entry, `x0` writes do not count, and
/// `interrupt` boundaries are not compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suite {
    /// rv32ui and the generated programs: M1 retirements to `write_tohost`, from a log
    /// without `-l`.
    Rv32ui,
    /// The misaligned-access programs: M1 retirements, then the trap taken in M, from a
    /// `-l` log.
    Misaligned,
    /// The M2 CSR programs: retirements with their CSR writes, to `write_tohost` or the
    /// trap taken in M before it, from a `-l` log. A program without a `tohost` ends at its
    /// trap.
    M2,
    /// The M3 privilege and Sv32 programs: retirements with their mode, and delegated
    /// exceptions, to the exception taken in M. `paddr` is not compared: Spike's log names
    /// the virtual address.
    M3,
}

impl Suite {
    /// Every suite.
    pub const ALL: [Suite; 4] = [Suite::Rv32ui, Suite::Misaligned, Suite::M2, Suite::M3];

    /// The CPU profile its programs run with.
    pub fn cpu(self) -> Rv32iProfile {
        match self {
            Suite::Rv32ui | Suite::Misaligned => Rv32iProfile::M1,
            Suite::M2 => Rv32iProfile::M2,
            Suite::M3 => Rv32iProfile::M3,
        }
    }

    /// The compared `retire` fields, in observation order.
    fn retire(self) -> &'static [&'static str] {
        const M1: [&str; 8] = [
            "pc", "insn", "rd", "rd_value", "mem", "addr", "width", "value",
        ];
        const M2: [&str; 10] = [
            "pc",
            "insn",
            "rd",
            "rd_value",
            "mem",
            "addr",
            "width",
            "value",
            "csr",
            "csr_value",
        ];
        const M3: [&str; 11] = [
            "pc",
            "insn",
            "priv",
            "rd",
            "rd_value",
            "mem",
            "addr",
            "width",
            "value",
            "csr",
            "csr_value",
        ];
        match self {
            Suite::Rv32ui | Suite::Misaligned => &M1,
            Suite::M2 => &M2,
            Suite::M3 => &M3,
        }
    }

    /// The profile version: primary `arch`, every field required, report window 3.
    pub fn profile(self) -> ComparisonProfile {
        const ABSENT: [&str; 7] = [
            "rd",
            "rd_value",
            "addr",
            "width",
            "value",
            "csr",
            "csr_value",
        ];
        let spec = |name: &&str| FieldSpec {
            name: (*name).to_owned(),
            requirement: Requirement::Required,
            may_be_absent: ABSENT.contains(name),
        };
        let retire = self.retire();
        let mut excluded = vec!["next_pc".to_owned()];
        excluded.extend(
            RETIRE_FIELDS
                .iter()
                .filter(|f| !retire.contains(f))
                .map(|f| (*f).to_owned()),
        );
        let mut kinds = vec![KindSpec {
            kind: "retire".to_owned(),
            fields: retire.iter().map(spec).collect(),
            excluded,
        }];
        if self != Suite::Rv32ui {
            kinds.insert(
                0,
                KindSpec {
                    kind: "exception".to_owned(),
                    fields: EXCEPTION_FIELDS.iter().map(spec).collect(),
                    excluded: vec!["from".to_owned()],
                },
            );
        }
        let (name, stop) = match self {
            Suite::Rv32ui => ("spike-rv32ui", WRITE_TOHOST),
            Suite::Misaligned => ("spike-misaligned", HALT_TRAP),
            Suite::M2 => ("spike-m2-csr", WRITE_TOHOST),
            Suite::M3 => ("spike-m3-privilege", HALT_TRAP),
        };
        ComparisonProfile {
            name: name.to_owned(),
            version: 1,
            primary: StreamSpec {
                stream: "arch".to_owned(),
                kinds,
                window: String::new(),
            },
            auxiliary: Vec::new(),
            stop_rule: stop.to_owned(),
            report_window: 3,
        }
    }

    /// Spike's semantic options, its ISA and privilege modes: part of its identity.
    fn spike_build(self) -> String {
        let (isa, privilege) = match self {
            Suite::Rv32ui | Suite::Misaligned => (SPIKE_ISA, SPIKE_PRIV),
            Suite::M2 => (SPIKE_ISA_M2, SPIKE_PRIV),
            Suite::M3 => (SPIKE_ISA_M2, SPIKE_PRIV_M3),
        };
        format!("--isa={isa} --priv={privilege}")
    }

    /// Spike's canonical run configuration: its instruction limit.
    pub fn spike_config(self) -> BackendRunConfig {
        BackendRunConfig {
            budget: match self {
                Suite::M3 => SPIKE_INSTRUCTIONS_M3,
                _ => MAX_INSTRUCTIONS,
            },
        }
    }
}

/// The SystemScope side's canonical run configuration: [`EVENT_BUDGET`].
pub fn config() -> BackendRunConfig {
    BackendRunConfig {
        budget: EVENT_BUDGET,
    }
}

/// Where the Spike adapter finds Spike's commit log.
#[derive(Clone, Debug)]
pub enum Source {
    /// Runs `spike` with `args` in `root`; they must write the log to `log`, relative to
    /// `root`.
    Run {
        /// The Spike executable.
        spike: PathBuf,
        /// Where Spike runs.
        root: PathBuf,
        /// Spike's command line, after the executable.
        args: Vec<String>,
        /// The log path, relative to `root`.
        log: String,
    },
    /// A log Spike already wrote.
    Log(String),
    /// A log already parsed: its events, and the trap taken in M, if any.
    Parsed(Vec<Event>, Option<SpikeTrap>),
}

/// The Spike adapter: Spike's `arch` stream, read whole after the run (§9.2).
pub struct SpikeBackend {
    suite: Suite,
    source: Source,
    tohost: Option<u32>,
    stream: Option<Vec<Observation>>,
}

impl SpikeBackend {
    /// An adapter reading `suite`'s log from `source`, for a program whose `tohost`
    /// symbol, if any, is `tohost`.
    pub fn new(suite: Suite, source: Source, tohost: Option<u32>) -> SpikeBackend {
        SpikeBackend {
            suite,
            source,
            tohost,
            stream: None,
        }
    }

    /// Spike's events and the trap taken in M, parsed as the suite's differential parses
    /// them.
    fn events(&self) -> Result<(Vec<Event>, Option<SpikeTrap>), String> {
        let text = match &self.source {
            Source::Parsed(events, trap) => return Ok((events.clone(), trap.clone())),
            Source::Log(text) => text.clone(),
            Source::Run {
                spike,
                root,
                args,
                log,
            } => run_spike(root, spike, args, log, &mut false)?,
        };
        let m = |retires: Vec<Retire>| retires.into_iter().map(machine).collect();
        match self.suite {
            Suite::Rv32ui => parse_spike_log(&text).map(|r| (m(r), None)),
            Suite::Misaligned | Suite::M2 => parse_spike_l_log(&text).map(|(r, t)| (m(r), t)),
            Suite::M3 => parse_spike_m3_log(&text).map(|(e, t)| (e, Some(t))),
        }
    }
}

/// A machine-mode retirement, as every M1 and M2 commit line is.
pub fn machine(retire: Retire) -> Event {
    Event::Retire {
        privilege: 3,
        retire,
    }
}

impl Backend for SpikeBackend {
    fn describe(&self) -> BackendId {
        BackendId {
            name: "spike".to_owned(),
            version: SPIKE_COMMIT.to_owned(),
            build: self.suite.spike_build(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::from([Capability::ArchStream, Capability::DeterministicReplay])
    }

    fn support(&self) -> Support {
        let mut s = Support::default();
        s.streams.insert("arch".to_owned());
        let retire = RETIRE_FIELDS.iter().filter(|f| **f != "paddr");
        s.fields.extend(retire.map(|f| format!("arch/retire/{f}")));
        s.fields.extend(
            EXCEPTION_FIELDS
                .iter()
                .map(|f| format!("arch/exception/{f}")),
        );
        s
    }

    /// Reads Spike's whole stream, running Spike first for [`Source::Run`]. Spike runs
    /// only the committed program: any input variant is [`EngineError::Unsupported`].
    fn start(
        &mut self,
        _: &Scenario,
        input: &InputVariant,
        _: &BackendRunConfig,
        profile: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        self.stream = None;
        if *input != InputVariant::None {
            return Err(EngineError::Unsupported(
                "an input variant on Spike".to_owned(),
            ));
        }
        if profile.name != self.suite.profile().name {
            return Err(EngineError::Internal(format!(
                "the {:?} adapter cannot read profile {}",
                self.suite, profile.name
            )));
        }
        let (events, trap) = self.events().map_err(EngineError::BackendFailed)?;
        let stream = observations(self.suite, profile, self.tohost, events, trap)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn next(&mut self, _: &str) -> Result<Option<Observation>, EngineError> {
        Err(EngineError::Unsupported("step_boundary".to_owned()))
    }

    fn record(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        if stream != "arch" {
            return Err(EngineError::Internal(format!(
                "{stream} is not a compared stream"
            )));
        }
        self.stream
            .take()
            .ok_or_else(|| EngineError::Internal("record before start".to_owned()))
    }
}

/// Spike's events as `arch` observations under `profile`, ending at its stop rule: the
/// trap taken in M, appended as the final `exception`, or `write_tohost` for a program
/// with a `tohost`.
fn observations(
    suite: Suite,
    profile: &ComparisonProfile,
    tohost: Option<u32>,
    events: Vec<Event>,
    trap: Option<SpikeTrap>,
) -> Result<Vec<Observation>, EngineError> {
    let written = match profile.stop_rule.as_str() {
        HALT_TRAP => false,
        WRITE_TOHOST => true,
        rule => {
            return Err(EngineError::Internal(format!(
                "the Spike adapter cannot apply stop rule {rule}"
            )));
        }
    };
    let at_tohost = tohost.filter(|_| written);
    let mut out = Vec::new();
    let mut place = |kind: &str, fields: Vec<(&str, ObservedValue)>| {
        let Some(spec) = profile.primary.kind(kind) else {
            return Ok(());
        };
        let mut kept = Vec::with_capacity(spec.fields.len());
        for f in &spec.fields {
            let Some((_, v)) = fields.iter().find(|(n, _)| *n == f.name) else {
                return Err(EngineError::Internal(format!(
                    "the Spike adapter reports no arch/{kind}/{}",
                    f.name
                )));
            };
            kept.push((f.name.clone(), v.clone()));
        }
        out.push(Observation {
            ordinal: out.len() as u64,
            stream: "arch".to_owned(),
            kind: kind.to_owned(),
            fields: kept,
        });
        Ok(())
    };
    let u = |v: u32| ObservedValue::Present(Value::U64(v.into()));
    let s = |v: &str| ObservedValue::Present(Value::Str(v.to_owned()));
    let mut armed = false;
    for event in events {
        match event {
            Event::Retire { privilege, retire } => {
                place("retire", retire_fields(privilege, &retire))?;
                if let (Some(t), Mem::Store { addr, width: 4, .. }) = (at_tohost, retire.mem) {
                    if addr == t {
                        armed = true;
                    } else if armed && addr == t.wrapping_add(4) {
                        return Ok(out);
                    }
                }
            }
            Event::Exception {
                pc,
                insn,
                cause,
                tval,
            } => place(
                "exception",
                vec![
                    ("pc", u(pc)),
                    ("insn", u(insn)),
                    ("cause", s(&cause)),
                    ("tval", u(tval)),
                    ("to", s("S")),
                ],
            )?,
        }
    }
    let Some(trap) = trap else {
        return Err(EngineError::BackendFailed(match at_tohost {
            Some(_) => "Spike's stream ends before write_tohost".to_owned(),
            None => "Spike's stream ends before a trap taken in M".to_owned(),
        }));
    };
    let causes = match suite {
        Suite::M3 => &SPIKE_CAUSES_M3[..],
        _ => &SPIKE_CAUSES[..],
    };
    let cause = causes
        .iter()
        .find(|(_, theirs)| *theirs == trap.cause)
        .map(|(ours, _)| *ours)
        .ok_or_else(|| EngineError::BackendFailed(format!("no cause named {}", trap.cause)))?;
    place(
        "exception",
        vec![
            ("pc", u(trap.pc)),
            ("insn", u(trap.insn)),
            ("cause", s(cause)),
            ("tval", u(trap.tval)),
            ("to", s("M")),
        ],
    )?;
    Ok(out)
}

/// Every `retire` field Spike reports for a retirement in mode `privilege`.
fn retire_fields(privilege: u8, r: &Retire) -> Vec<(&'static str, ObservedValue)> {
    let u = |v: u32| ObservedValue::Present(Value::U64(v.into()));
    let absent = ObservedValue::Absent;
    let (rd, rd_value) = r
        .reg_write
        .map_or((absent.clone(), absent.clone()), |(rd, v)| {
            (u(rd.into()), u(v))
        });
    let (mem, addr, width, value) = match r.mem {
        Mem::None => ("none", absent.clone(), absent.clone(), absent.clone()),
        Mem::Load { addr } => ("load", u(addr), absent.clone(), absent.clone()),
        Mem::Store { addr, width, value } => ("store", u(addr), u(width.into()), u(value)),
    };
    let (csr, csr_value) = r
        .csr
        .map_or((absent.clone(), absent), |(csr, v)| (u(csr.into()), u(v)));
    vec![
        ("pc", u(r.pc)),
        ("insn", u(r.insn)),
        ("priv", u(privilege.into())),
        ("rd", rd),
        ("rd_value", rd_value),
        ("mem", ObservedValue::Present(Value::Str(mem.to_owned()))),
        ("addr", addr),
        ("width", width),
        ("value", value),
        ("csr", csr),
        ("csr_value", csr_value),
    ]
}

/// The scenario of `image` on the runner's platform with `suite`'s CPU profile.
pub fn scenario(suite: Suite, image: &LoadImage) -> Scenario {
    let cpu = suite.cpu();
    Scenario {
        platform: "m1-reference".to_owned(),
        topology_hash: runner::platform_with_profile(image, false, SEED, cpu).topology_hash(),
        cpu_profile: format!("{cpu:?}"),
        program: image.image_hash,
        locator: hex(&image.image_hash),
        seed: SEED,
    }
}

/// The CPU's path on the runner's platform, which names the patchable fields:
/// `soc.cpu0/pc` and `soc.cpu0/x1` to `soc.cpu0/x31`.
pub const CPU_PATH: &str = "soc.cpu0";

/// Patches one 32-bit CPU field of runner-platform snapshot bytes after checking its old
/// value. The CPU's state starts with its configuration (clock, entry, instruction limit:
/// 16 bytes), then `pc` and `x1` to `x31`, each a little-endian word.
pub fn patch(bytes: &mut [u8], p: &StatePatch) -> Result<(), String> {
    let field = p
        .field
        .strip_prefix(CPU_PATH)
        .and_then(|f| f.strip_prefix('/'))
        .ok_or_else(|| format!("{} is not a CPU field", p.field))?;
    let word = match field {
        "pc" => 0,
        x => x
            .strip_prefix('x')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..32).contains(n))
            .ok_or_else(|| format!("{} is not patchable", p.field))?,
    };
    let value = |v: &Value| match v {
        Value::U64(v) => u32::try_from(*v).map_err(|_| format!("{v:#x} is not 32 bits")),
        other => Err(format!("{other:?} is not a U64")),
    };
    let (old, new) = (value(&p.old)?, value(&p.new)?);
    let states = component_states(bytes).map_err(|e| e.to_string())?;
    let (_, _, cpu) = states
        .iter()
        .find(|(id, _, _)| *id == CPU.0)
        .ok_or("the snapshot has no CPU")?;
    let at = cpu.as_ptr() as usize - bytes.as_ptr() as usize + 16 + 4 * word;
    let held = bytes
        .get(at..at + 4)
        .map(|w| u32::from_le_bytes(w.try_into().expect("four bytes")))
        .ok_or("the CPU state is too short")?;
    if held != old {
        return Err(format!("{} is {held:#x}, not {old:#x}", p.field));
    }
    bytes[at..at + 4].copy_from_slice(&new.to_le_bytes());
    Ok(())
}

/// The SystemScope side: `image` on the runner's platform with `suite`'s CPU profile,
/// read by [`Rv32Projection`] for a program whose `tohost`, if any, is `tohost`, and able
/// to apply state patches to the CPU.
pub fn systemscope(suite: Suite, image: &LoadImage, tohost: Option<u32>) -> RuntimeBackend {
    let (image, cpu) = (image.clone(), suite.cpu());
    RuntimeBackend::new(
        BackendId {
            name: "systemscope".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: format!("m1-reference/{cpu:?}"),
        },
        Box::new(move |_, input| match input {
            InputVariant::Media(_) => Err("the runner's program is never patched".to_owned()),
            _ => Ok(runner::platform_with_profile(&image, false, SEED, cpu)),
        }),
        Box::new(move || Box::new(Rv32Projection::new(tohost))),
    )
    .with_patch(Box::new(patch))
}

/// Locates SystemScope's run of `image`, the actual side `ours`, against Spike, the
/// expected side, under `suite`'s profile: a stream compare, since Spike only records.
pub fn located(
    suite: Suite,
    image: &LoadImage,
    spike: &mut SpikeBackend,
    ours: (&mut RuntimeBackend, InputVariant),
) -> Result<Located, String> {
    let plan = plan(&suite.profile(), &spike.support(), &ours.0.support())
        .map_err(|e| format!("Spike and SystemScope are incomparable: {e:?}"))?;
    let expected = Side {
        backend: spike,
        input: InputVariant::None,
        config: suite.spike_config(),
    };
    let actual = Side {
        backend: ours.0,
        input: ours.1,
        config: config(),
    };
    locate(&plan, &scenario(suite, image), expected, actual, None).map_err(|e| e.to_string())
}

/// The M4 comparison a differential requires: SystemScope's run of `image` agrees with
/// Spike's stream from `source` under `suite`'s profile. Returns the observations
/// compared.
pub fn agreed(
    suite: Suite,
    image: &LoadImage,
    source: Source,
    tohost: Option<u32>,
) -> Result<u64, String> {
    let mut spike = SpikeBackend::new(suite, source, tohost);
    let mut ours = systemscope(suite, image, tohost);
    let found = located(suite, image, &mut spike, (&mut ours, InputVariant::None))?;
    match &found.verdict {
        Verdict::Compared(c) if c.failures.is_empty() => match &c.primary {
            StreamResult::Agree { compared, .. } => Ok(*compared),
            diverged @ StreamResult::Diverged { .. } => Err(format!(
                "SystemScope diverges from Spike: {diverged:?}, around it {:?}",
                found.window
            )),
        },
        other => Err(format!("Spike and SystemScope do not compare: {other:?}")),
    }
}

/// `rs1` of an injective instruction (§25.1): an `ADD`, `SUB`, `XOR`, `ADDI`, or `XORI`
/// with `rd` and `rs1` not `x0`, and `rs2 ≠ rs1` for `SUB` and `XOR`. Flipping bit 0 of
/// `x[rs1]` always changes its `rd_value`.
pub fn injective_rs1(insn: u32) -> Option<u32> {
    let (opcode, rd, funct3) = (insn & 0x7f, (insn >> 7) & 0x1f, (insn >> 12) & 0x7);
    let (rs1, rs2, funct7) = ((insn >> 15) & 0x1f, (insn >> 20) & 0x1f, insn >> 25);
    let ok = match (opcode, funct3, funct7) {
        (0x13, 0 | 4, _) => true,
        (0x33, 0, 0) => true,
        (0x33, 0, 0x20) | (0x33, 4, 0) => rs2 != rs1,
        _ => false,
    };
    (ok && rd != 0 && rs1 != 0).then_some(rs1)
}

/// The SK-1 fixture.
pub const SK1_ELF: &str = "rv32ui-add.elf";

/// The SK-1 reproducer's checkpoint density: a SystemScope checkpoint after every
/// boundary, so it starts exactly the report window before the divergence.
pub const SK1_EVERY: u64 = 1;

/// A case of SystemScope against Spike under the rv32ui profile: Spike the expected side,
/// from its initial state, SystemScope the actual side, each with its input variant.
#[derive(Clone, Debug)]
pub struct SpikeCase {
    /// The program's image.
    pub image: LoadImage,
    /// Its `tohost`.
    pub tohost: u32,
    /// Spike's and SystemScope's input variants.
    pub inputs: [InputVariant; 2],
}

impl SpikeCase {
    /// The scenario.
    pub fn scenario(&self) -> Scenario {
        scenario(Suite::Rv32ui, &self.image)
    }

    /// The plan.
    pub fn plan(&self) -> Result<Plan, String> {
        let spike = SpikeBackend::new(Suite::Rv32ui, Source::Log(String::new()), None);
        let ours = systemscope(Suite::Rv32ui, &self.image, Some(self.tohost));
        plan(&Suite::Rv32ui.profile(), &spike.support(), &ours.support())
            .map_err(|e| format!("{e:?}"))
    }

    /// Calls `f` with both sides: Spike reading its log from `source`, and SystemScope.
    fn with_sides<T>(&self, source: &Source, f: impl FnOnce([Builder<'_>; 2]) -> T) -> T {
        let tohost = Some(self.tohost);
        let spike = || -> Box<dyn Backend> {
            Box::new(SpikeBackend::new(Suite::Rv32ui, source.clone(), tohost))
        };
        let ours = || systemscope(Suite::Rv32ui, &self.image, tohost);
        f([
            Builder {
                make: Make::Other(&spike),
                config: Suite::Rv32ui.spike_config(),
            },
            Builder {
                make: Make::Runtime(&ours),
                config: config(),
            },
        ])
    }

    /// The reproducer, minimized with a SystemScope checkpoint every `every` boundaries;
    /// Spike, which cannot restore, starts from its initial state (§17.2).
    pub fn minimized(&self, source: &Source, every: u64) -> Result<Minimized, String> {
        let (plan, scenario) = (self.plan()?, self.scenario());
        self.with_sides(source, |sides| {
            minimize(&plan, &scenario, sides, self.inputs.clone(), every).map_err(|e| e.to_string())
        })
    }

    /// Replays `bundle` with Spike reading its log from `source`.
    pub fn replay(&self, bundle: &Bundle, source: &Source) -> Result<VerificationResult, String> {
        let (plan, scenario) = (self.plan()?, self.scenario());
        self.with_sides(source, |sides| {
            let case = Case {
                plan: &plan,
                scenario: &scenario,
                sides,
            };
            replay(bundle, case).map_err(|e| format!("{e:?}"))
        })
    }
}

/// SK-1 (§25.1): `rv32ui-add` under the rv32ui Spike profile, Spike the expected side from
/// its initial state, SystemScope the actual side with `x[rs1] ^= 1` after boundary
/// `r − 1`, where `r` is the first retirement of an injective instruction. The expected
/// first divergence is `arch[r]`, `retire`, field `rd_value`.
#[derive(Clone, Debug)]
pub struct Sk1 {
    /// The fixture's image.
    pub image: LoadImage,
    /// Its `tohost`.
    pub tohost: u32,
    /// The patched boundary.
    pub r: u64,
    /// The SystemScope side's input variant.
    pub patch: InputVariant,
}

impl Sk1 {
    /// Selects SK-1 from SystemScope's unmodified `arch` stream of the committed fixture
    /// under `root`, a pure function of the stream: reset leaves the registers 0, and only
    /// retirements write them.
    pub fn select(root: &Path) -> Result<Sk1, String> {
        let path = root.join(FIXTURE_DIR).join(SK1_ELF);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let image = load("add", &bytes)?;
        let tohost = elf_symbol(&bytes, "tohost").ok_or("rv32ui-add has no tohost")?;
        let suite = Suite::Rv32ui;
        let mut b = systemscope(suite, &image, Some(tohost));
        let e = |e: EngineError| e.to_string();
        b.start(
            &scenario(suite, &image),
            &InputVariant::None,
            &config(),
            &suite.profile(),
        )
        .map_err(e)?;
        let word = |o: &Observation, field: &str| match o.field(field) {
            Some(ObservedValue::Present(Value::U64(v))) => Ok(*v as u32),
            other => Err(format!("arch[{}].{field} is {other:?}", o.ordinal)),
        };
        let mut x = [0u32; 32];
        while let Some(o) = b.next("arch").map_err(e)? {
            if let Some(rs1) = injective_rs1(word(&o, "insn")?).filter(|_| o.ordinal > 0) {
                let event_index = b
                    .event_index("arch", o.ordinal - 1)
                    .ok_or("no event before the patched boundary")?;
                let old = x[rs1 as usize];
                return Ok(Sk1 {
                    image,
                    tohost,
                    r: o.ordinal,
                    patch: InputVariant::State(StatePatch {
                        event_index,
                        field: format!("{CPU_PATH}/x{rs1}"),
                        old: Value::U64(old.into()),
                        new: Value::U64((old ^ 1).into()),
                    }),
                });
            }
            if let Some(ObservedValue::Present(Value::U64(rd))) = o.field("rd") {
                x[*rd as usize] = word(&o, "rd_value")?;
            }
        }
        Err("rv32ui-add retires no injective instruction".to_owned())
    }

    /// Both sides' input variants.
    pub fn inputs(&self) -> [InputVariant; 2] {
        [InputVariant::None, self.patch.clone()]
    }

    /// The scenario.
    pub fn scenario(&self) -> Scenario {
        scenario(Suite::Rv32ui, &self.image)
    }

    /// The case: SystemScope's side patched.
    pub fn case(&self) -> SpikeCase {
        SpikeCase {
            image: self.image.clone(),
            tohost: self.tohost,
            inputs: self.inputs(),
        }
    }

    /// The plan.
    pub fn plan(&self) -> Result<Plan, String> {
        self.case().plan()
    }

    /// The reproducer, minimized with a SystemScope checkpoint every `every` boundaries;
    /// Spike, which cannot restore, starts from its initial state (§17.2).
    pub fn minimized(&self, source: &Source, every: u64) -> Result<Minimized, String> {
        self.case().minimized(source, every)
    }

    /// Requires `result` to be the selector's divergence: `arch[r]`, `retire`, field
    /// `rd_value`.
    pub fn check(&self, result: &VerificationResult) -> Result<(), String> {
        if let Ok(Verdict::Compared(Comparison {
            primary:
                StreamResult::Diverged {
                    ordinal,
                    divergence: Divergence::Fields { kind, fields },
                    ..
                },
            ..
        })) = &result.verdict
            && *ordinal == self.r
            && kind == "retire"
            && fields.iter().map(|f| f.name.as_str()).eq(["rd_value"])
        {
            return Ok(());
        }
        Err(format!(
            "SK-1 must diverge at arch[{}], retire, rd_value, not as {:?}",
            self.r, result.verdict
        ))
    }

    /// Replays `bundle` with Spike reading its log from `source`.
    pub fn replay(&self, bundle: &Bundle, source: &Source) -> Result<VerificationResult, String> {
        self.case().replay(bundle, source)
    }
}

/// SK-1 against the Spike executable `spike`, run in `root` with its log under `logs`
/// (relative to `root`): the reproducer minimized by [`Sk1::minimized`] must diverge as
/// the selector says, and, written to `logs/sk1.repro` and read back, replay with a second
/// run of Spike. Returns what it checked.
pub fn run_sk1(root: &Path, spike: &Path, logs: &str) -> Result<String, String> {
    let sk1 = Sk1::select(root)?;
    let log = format!("{logs}/sk1/add.log");
    let elf = format!("{FIXTURE_DIR}/{SK1_ELF}");
    let source = Source::Run {
        spike: spike.to_owned(),
        root: root.to_owned(),
        args: spike_args(sk1.image.entry, &elf, &log),
        log,
    };
    let m = sk1.minimized(&source, SK1_EVERY)?;
    sk1.check(&m.result)?;
    let dir = root.join(logs).join("sk1.repro");
    let _ = fs::remove_dir_all(&dir);
    write_dir(&m.bundle, &dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let bundle = read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let again = sk1.replay(&bundle, &source)?;
    if again != m.result {
        return Err("the replayed reproducer's result differs".to_owned());
    }
    let InputVariant::State(p) = &sk1.patch else {
        return Err("SK-1 patches no state".to_owned());
    };
    Ok(format!(
        "rv32ui-add with {} ^= 1 after boundary {} is located at arch[{}], retire, rd_value, \
         by stream compare against Spike {SPIKE_COMMIT}; its reproducer, SystemScope from \
         boundary {} and Spike from its initial state, replays from {}",
        p.field,
        sk1.r - 1,
        sk1.r,
        m.origins[1].map_or("0".to_owned(), |c| c.to_string()),
        dir.display()
    ))
}

/// The progen seeds of the Spike campaign (§25.3): the 64 after
/// [`crate::progen::FIXED_SEEDS`].
pub const CAMPAIGN_SEEDS: std::ops::Range<u64> = 64..128;

/// The campaign's checkpoint density for a reproducer: every boundary, as SK-1's.
pub const CAMPAIGN_EVERY: u64 = 1;

/// One campaign program against Spike's stream from `source`: the observations compared
/// when every stream agrees. Otherwise the case is minimized, written to `repro` under
/// `root`, read back, and replayed, and the error names the reproducer.
pub fn campaign_case(
    root: &Path,
    case: &SpikeCase,
    source: &Source,
    repro: &str,
) -> Result<u64, String> {
    let (plan, scenario) = (case.plan()?, case.scenario());
    let mut spike = SpikeBackend::new(Suite::Rv32ui, source.clone(), Some(case.tohost));
    let mut ours = systemscope(Suite::Rv32ui, &case.image, Some(case.tohost));
    let found = locate(
        &plan,
        &scenario,
        Side {
            backend: &mut spike,
            input: case.inputs[0].clone(),
            config: Suite::Rv32ui.spike_config(),
        },
        Side {
            backend: &mut ours,
            input: case.inputs[1].clone(),
            config: config(),
        },
        None,
    )
    .map_err(|e| e.to_string())?;
    let what = match &found.verdict {
        Verdict::Compared(c) if c.failures.is_empty() && c.auxiliary.iter().all(|a| a.agrees()) => {
            match &c.primary {
                StreamResult::Agree { compared, .. } => return Ok(*compared),
                StreamResult::Diverged { ordinal, .. } => format!("arch[{ordinal}]"),
            }
        }
        other => format!("{other:?}"),
    };
    let m = case.minimized(source, CAMPAIGN_EVERY)?;
    let dir = root.join(repro);
    let _ = fs::remove_dir_all(&dir);
    write_dir(&m.bundle, &dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let bundle = read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let replays = match case.replay(&bundle, source)? == m.result {
        true => "replays",
        false => "does not replay",
    };
    Err(format!(
        "SystemScope diverges from Spike at {what}; the reproducer at {} {replays}",
        dir.display()
    ))
}

/// The Spike campaign (§25.3) against the Spike executable `spike`, run in `root` with its
/// logs and reproducers under `logs/campaign`: each seed's program name and the
/// observations compared, or every error.
pub fn run_campaign(
    root: &Path,
    spike: &Path,
    logs: &str,
    seeds: impl IntoIterator<Item = u64>,
) -> Result<Vec<(String, u64)>, Vec<String>> {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for seed in seeds {
        let program = crate::progen::generate(seed);
        let name = program.name.clone();
        match campaign_program(root, spike, logs, &program) {
            Ok(n) => ok.push((name, n)),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    match errors.is_empty() {
        true => Ok(ok),
        false => Err(errors),
    }
}

fn campaign_program(
    root: &Path,
    spike: &Path,
    logs: &str,
    program: &crate::progen::Program,
) -> Result<u64, String> {
    let name = &program.name;
    let elf = format!("{logs}/campaign/{name}.elf");
    let log = format!("{logs}/campaign/{name}.log");
    let path = root.join(&elf);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&path, &program.elf).map_err(|e| format!("{elf}: {e}"))?;
    let image = load(name, &program.elf)?;
    let tohost = elf_symbol(&program.elf, "tohost").ok_or("no tohost symbol")?;
    let source = Source::Run {
        spike: spike.to_owned(),
        root: root.to_owned(),
        args: spike_args(image.entry, &elf, &log),
        log,
    };
    let case = SpikeCase {
        image,
        tohost,
        inputs: [InputVariant::None, InputVariant::None],
    };
    campaign_case(
        root,
        &case,
        &source,
        &format!("{logs}/campaign/{name}.repro"),
    )
}
