//! Spike under the M4 verification engine (`docs/m4-design.md` §8.2, §9.2, §28.1): the
//! Spike adapter, a post-run backend over the existing strict commit-log parsers; the
//! four Spike profile versions, whose rules are the existing differentials'; and the
//! SystemScope side, a runtime backend on the runner's platform.
//!
//! The adapter never re-executes and shares no decoding code with the SystemScope side
//! (§8.4): Spike's stream is read from its log alone.

use std::path::PathBuf;

use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_rv32i::Rv32iProfile;
use systemscope_verify::backend::{Backend, Capabilities, Capability, Side};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario};
use systemscope_verify::compare::{StreamResult, Verdict};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{Located, locate};
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Requirement, StreamSpec, Support, plan,
};
use systemscope_verify::runtime::RuntimeBackend;

use crate::projection::{EXCEPTION_FIELDS, HALT_TRAP, RETIRE_FIELDS, Rv32Projection, WRITE_TOHOST};
use crate::runner::{self, SEED};
use crate::spike::{
    Event, Mem, Retire, SPIKE_CAUSES, SPIKE_CAUSES_M3, SPIKE_COMMIT, SPIKE_INSTRUCTIONS_M3,
    SPIKE_ISA, SPIKE_ISA_M2, SPIKE_PRIV, SPIKE_PRIV_M3, SpikeTrap, parse_spike_l_log,
    parse_spike_log, parse_spike_m3_log, run_spike,
};
use crate::{MAX_INSTRUCTIONS, hex};

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
    /// trap taken in M before it, from a `-l` log.
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
/// trap taken in M, appended as the final `exception`, or `write_tohost`.
fn observations(
    suite: Suite,
    profile: &ComparisonProfile,
    tohost: Option<u32>,
    events: Vec<Event>,
    trap: Option<SpikeTrap>,
) -> Result<Vec<Observation>, EngineError> {
    let at_tohost = match (profile.stop_rule.as_str(), tohost) {
        (HALT_TRAP, _) => None,
        (WRITE_TOHOST, Some(t)) => Some(t),
        (rule, _) => {
            return Err(EngineError::Internal(format!(
                "the Spike adapter cannot apply stop rule {rule} here"
            )));
        }
    };
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
            None => "Spike's log has no trap taken in M".to_owned(),
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

/// The SystemScope side: `image` on the runner's platform with `suite`'s CPU profile,
/// read by [`Rv32Projection`] for a program whose `tohost`, if any, is `tohost`.
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
