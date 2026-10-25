//! `m5-reference` under the M4 verification engine (`docs/m5-design.md` §6, §13, §15): the
//! timing-invariant `outcome` stream, the `schedule` stream, the `os` stream, the
//! `m5.outcome` and `m5.lockstep` profiles, and a runtime backend under a perturbation.
//!
//! `outcome` groups each process's observations in pid order, so it holds state across
//! event boundaries: the observations of a process that has not yet been released, and
//! the bytes of a `write` in progress. `schedule` remembers the latest interrupt's `epc`,
//! the preempted process's `sepc`. Both are saved into every checkpoint and loaded on
//! restore (`Projection::save`, `Projection::load`).

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_os::Defect;
use systemscope_os::interrupts::{
    BLOCK_KIND, FINAL_KIND, INTERRUPT_KIND as OS_INTERRUPT_KIND, PREEMPT_KIND, WAKE_KIND,
};
use systemscope_os::kernel::SHUTDOWN_KIND;
use systemscope_os::procop::{
    CREATE_KIND, EXIT_KIND, FAULT_KIND, SWITCH_KIND, SYSCALL_ENTER_KIND, SYSCALL_EXIT_KIND,
};
use systemscope_os::syscall::{SYS_BLK_READ, SYS_EXIT, SYS_EXIT_GROUP, SYS_GETPID, SYS_WRITE};
use systemscope_platform::uart::TX_KIND;
use systemscope_rv32::m5::{self, DISK_FILE};
use systemscope_rv32::m5ref::{self, Config};
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::projection::{self, HALT_TRAP, INTERRUPT_FIELDS, Rv32Projection};
use systemscope_rv32i::cpu::{HALT_KIND, INTERRUPT_KIND as CPU_INTERRUPT_KIND};
use systemscope_verify::case::{BackendId, BackendRunConfig, InputVariant, Scenario};
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, KindSpec, Requirement, StreamSpec, Support,
};
use systemscope_verify::runtime::{Projected, Projection, RuntimeBackend};

use super::{Fixture, SCENARIO};
use crate::hex;

/// The timing-invariant stream (§13.1).
pub const OUTCOME: &str = "outcome";
/// The scheduling decisions (§15).
pub const SCHEDULE: &str = "schedule";
/// The profile of every campaign: primary `outcome`.
pub const OUTCOME_PROFILE: &str = "m5.outcome";
/// The profile of an unperturbed run against itself: primary `arch`.
pub const LOCKSTEP_PROFILE: &str = "m5.lockstep";

/// The syscalls whose results do not depend on timing (§13.2). `sched_yield` always
/// returns 0, but how often a process calls it depends on the schedule, so it is left out.
pub const INVARIANT_SYSCALLS: [u32; 5] = [
    SYS_WRITE,
    SYS_BLK_READ,
    SYS_GETPID,
    SYS_EXIT,
    SYS_EXIT_GROUP,
];

/// Each `os` boundary: its record kind, boundary kind, and fields.
const OS_RECORDS: [(&str, &str, &[&str]); 12] = [
    (BLOCK_KIND, "block", &["pid", "lba"]),
    (CREATE_KIND, "create", &["pid", "entry", "root", "error"]),
    (EXIT_KIND, "exit", &["pid", "status"]),
    (FAULT_KIND, "fault", &["pid", "cause", "epc", "tval"]),
    (FINAL_KIND, "final", &["word", "value"]),
    (OS_INTERRUPT_KIND, "interrupt", &["sources"]),
    (PREEMPT_KIND, "preempt", &["pid"]),
    (SHUTDOWN_KIND, "shutdown", &["reason", "detail"]),
    (SWITCH_KIND, "switch", &["from", "to"]),
    (
        SYSCALL_ENTER_KIND,
        "syscall_enter",
        &["pid", "nr", "a0", "a1", "a2"],
    ),
    (SYSCALL_EXIT_KIND, "syscall_exit", &["pid", "nr", "ret"]),
    (WAKE_KIND, "wake", &["pid"]),
];

/// Each `outcome` boundary kind and its fields, in kind order.
const OUTCOME_KINDS: [(&str, &[&str]); 5] = [
    ("end", &["pid", "status", "cause", "epc", "tval"]),
    ("final", &["word", "value"]),
    ("limit", &["instret"]),
    ("out", &["pid", "bytes"]),
    ("ret", &["pid", "nr", "value"]),
];

/// The `end` fields that are `Absent`: a status after an exit, the rest after a fault.
const END_MAY_BE_ABSENT: [&str; 4] = ["status", "cause", "epc", "tval"];

/// Each `schedule` boundary: its record kind, boundary kind, and fields, in kind order.
const SCHEDULE_RECORDS: [(&str, &str, &[&str]); 4] = [
    (BLOCK_KIND, "block", &["pid", "lba"]),
    (PREEMPT_KIND, "preempt", &["pid", "sepc"]),
    (SWITCH_KIND, "switch", &["from", "to"]),
    (WAKE_KIND, "wake", &["pid"]),
];

fn required(names: &[&str], may_be_absent: &[&str]) -> Vec<FieldSpec> {
    names
        .iter()
        .map(|n| FieldSpec {
            name: (*n).to_owned(),
            requirement: Requirement::Required,
            may_be_absent: may_be_absent.contains(n),
        })
        .collect()
}

fn stream(
    name: &str,
    kinds: impl Iterator<Item = (&'static str, &'static [&'static str])>,
) -> StreamSpec {
    let mut kinds: Vec<KindSpec> = kinds
        .map(|(kind, fields)| KindSpec {
            kind: kind.to_owned(),
            fields: required(
                fields,
                if name == OUTCOME && kind == "end" {
                    &END_MAY_BE_ABSENT
                } else {
                    &[]
                },
            ),
            excluded: Vec::new(),
        })
        .collect();
    kinds.sort_by(|a, b| a.kind.cmp(&b.kind));
    StreamSpec {
        stream: name.to_owned(),
        kinds,
        window: String::new(),
    }
}

/// The `arch` stream of the `M5` profile: M3's, and every interrupt.
pub fn arch() -> StreamSpec {
    let mut s = projection::arch_m3();
    s.kinds.push(KindSpec {
        kind: "interrupt".to_owned(),
        fields: required(&INTERRUPT_FIELDS, &[]),
        excluded: vec!["handler".to_owned(), "from".to_owned()],
    });
    s.kinds.sort_by(|a, b| a.kind.cmp(&b.kind));
    s
}

/// The `os` stream: every kernel record but the boot's and the loader's.
pub fn os() -> StreamSpec {
    stream(
        "os",
        OS_RECORDS.iter().map(|&(_, kind, fields)| (kind, fields)),
    )
}

/// The `outcome` stream (§13.1).
pub fn outcome() -> StreamSpec {
    stream(OUTCOME, OUTCOME_KINDS.into_iter())
}

/// The `schedule` stream (§15).
pub fn schedule() -> StreamSpec {
    stream(
        SCHEDULE,
        SCHEDULE_RECORDS
            .iter()
            .map(|&(_, kind, fields)| (kind, fields)),
    )
}

/// `m5.outcome`, version 1: primary `outcome`; auxiliary `arch`, `os`, and `schedule`,
/// which a perturbation is expected to change (§13.3).
pub fn outcome_profile() -> ComparisonProfile {
    ComparisonProfile {
        name: OUTCOME_PROFILE.to_owned(),
        version: 1,
        primary: outcome(),
        auxiliary: vec![arch(), os(), schedule()],
        stop_rule: HALT_TRAP.to_owned(),
        report_window: 3,
    }
}

/// `m5.lockstep`, version 1: primary `arch`; auxiliary `os` and `schedule` (§13.3).
pub fn lockstep_profile() -> ComparisonProfile {
    ComparisonProfile {
        name: LOCKSTEP_PROFILE.to_owned(),
        version: 1,
        primary: arch(),
        auxiliary: vec![os(), schedule()],
        stop_rule: HALT_TRAP.to_owned(),
        report_window: 3,
    }
}

/// How a process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum End {
    Exit(i64),
    Fault { cause: String, epc: u64, tval: u64 },
}

/// One `outcome` observation of a process.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Out(Vec<u8>),
    Ret { nr: u64, value: u64 },
    End(End),
}

/// A created process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Proc {
    ended: bool,
    /// Whether its observations have been emitted; afterwards it has none.
    released: bool,
    /// Its observations, in event order, until released.
    held: Vec<Item>,
}

/// The state of the `outcome` and `schedule` projections across event boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    /// Every created process, by pid.
    procs: BTreeMap<u64, Proc>,
    /// The process whose `write` is in progress, and the bytes it has written so far.
    writing: Option<(u64, Vec<u8>)>,
    /// The latest interrupt's `epc`: the `sepc` of the process it preempts.
    epc: Option<u64>,
}

impl State {
    fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.len(self.procs.len());
        for (pid, p) in &self.procs {
            e.u64(*pid);
            e.bool(p.ended);
            e.bool(p.released);
            e.len(p.held.len());
            for item in &p.held {
                match item {
                    Item::Out(bytes) => {
                        e.u8(0);
                        e.bytes(bytes);
                    }
                    Item::Ret { nr, value } => {
                        e.u8(1);
                        e.u64(*nr);
                        e.u64(*value);
                    }
                    Item::End(End::Exit(status)) => {
                        e.u8(2);
                        e.i64(*status);
                    }
                    Item::End(End::Fault { cause, epc, tval }) => {
                        e.u8(3);
                        e.str(cause);
                        e.u64(*epc);
                        e.u64(*tval);
                    }
                }
            }
        }
        match &self.writing {
            None => e.u8(0),
            Some((pid, bytes)) => {
                e.u8(1);
                e.u64(*pid);
                e.bytes(bytes);
            }
        }
        match self.epc {
            None => e.u8(0),
            Some(epc) => {
                e.u8(1);
                e.u64(epc);
            }
        }
        e.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<State, DecodeError> {
        let mut d = Decoder::new(bytes);
        let mut s = State::default();
        // A process takes at least 14 bytes, an item 5.
        let count = d.len()?;
        if count > d.remaining() / 14 {
            return Err(DecodeError::Truncated);
        }
        for _ in 0..count {
            let pid = d.u64()?;
            let ended = d.bool()?;
            let released = d.bool()?;
            let items = d.len()?;
            if items > d.remaining() / 5 {
                return Err(DecodeError::Truncated);
            }
            let mut held = Vec::with_capacity(items);
            for _ in 0..items {
                held.push(match d.u8()? {
                    0 => Item::Out(d.bytes()?.to_vec()),
                    1 => Item::Ret {
                        nr: d.u64()?,
                        value: d.u64()?,
                    },
                    2 => Item::End(End::Exit(d.i64()?)),
                    3 => Item::End(End::Fault {
                        cause: d.str()?.to_owned(),
                        epc: d.u64()?,
                        tval: d.u64()?,
                    }),
                    tag => return Err(DecodeError::InvalidTag { what: "item", tag }),
                });
            }
            s.procs.insert(
                pid,
                Proc {
                    ended,
                    released,
                    held,
                },
            );
        }
        s.writing = match d.u8()? {
            0 => None,
            1 => Some((d.u64()?, d.bytes()?.to_vec())),
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "writing",
                    tag,
                });
            }
        };
        s.epc = match d.u8()? {
            0 => None,
            1 => Some(d.u64()?),
            tag => return Err(DecodeError::InvalidTag { what: "epc", tag }),
        };
        d.finish()?;
        Ok(s)
    }
}

/// `rv32.*` records through [`Rv32Projection`] into `arch`; kernel records copied into
/// `os`, and read into `outcome` and `schedule`; UART bytes read into `outcome`.
///
/// The CPU's instruction limit is a stop rule here, not an error (§20): every process
/// still held is released and `outcome` ends with `limit`. Only a run that never shuts
/// down reaches it, such as one under planted race R5.
#[derive(Default)]
pub struct M5Projection {
    rv32: Rv32Projection,
    state: State,
    limited: bool,
}

fn get<'a>(record: &'a TraceRecord, name: &str) -> Result<&'a Value, String> {
    record
        .fields
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
        .ok_or_else(|| format!("{}: no {name}", record.kind))
}

fn u64_of(record: &TraceRecord, name: &str) -> Result<u64, String> {
    match get(record, name)? {
        Value::U64(v) => Ok(*v),
        other => Err(format!("{}: {name} is {other:?}", record.kind)),
    }
}

fn boundary(stream: &str, kind: &str, fields: Vec<(&str, ObservedValue)>) -> Projected {
    Projected {
        stream: stream.to_owned(),
        kind: kind.to_owned(),
        fields: fields.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
    }
}

fn present(v: Value) -> ObservedValue {
    ObservedValue::Present(v)
}

impl Item {
    fn projected(&self, pid: u64) -> Projected {
        let u = |v: u64| present(Value::U64(v));
        match self {
            Item::Out(bytes) => boundary(
                OUTCOME,
                "out",
                vec![
                    ("pid", u(pid)),
                    ("bytes", present(Value::Bytes(bytes.clone()))),
                ],
            ),
            Item::Ret { nr, value } => boundary(
                OUTCOME,
                "ret",
                vec![("pid", u(pid)), ("nr", u(*nr)), ("value", u(*value))],
            ),
            Item::End(end) => {
                let (status, cause, epc, tval) = match end {
                    End::Exit(s) => (
                        present(Value::I64(*s)),
                        ObservedValue::Absent,
                        ObservedValue::Absent,
                        ObservedValue::Absent,
                    ),
                    End::Fault { cause, epc, tval } => (
                        ObservedValue::Absent,
                        present(Value::Str(cause.clone())),
                        u(*epc),
                        u(*tval),
                    ),
                };
                boundary(
                    OUTCOME,
                    "end",
                    vec![
                        ("pid", u(pid)),
                        ("status", status),
                        ("cause", cause),
                        ("epc", epc),
                        ("tval", tval),
                    ],
                )
            }
        }
    }
}

impl State {
    /// Appends `item` to `pid`'s observations.
    fn hold(&mut self, pid: u64, item: Item) -> Result<(), String> {
        match self.procs.get_mut(&pid) {
            Some(p) if !p.ended => {
                p.held.push(item);
                Ok(())
            }
            Some(_) => Err(format!("pid {pid} acts after its end")),
            None => Err(format!("pid {pid} was never created")),
        }
    }

    /// Emits, in pid order, every process whose own and lower pids have all ended; at the
    /// shutdown, every process.
    fn release(&mut self, all: bool, out: &mut Vec<Projected>) {
        for (pid, p) in &mut self.procs {
            if p.released {
                continue;
            }
            if !(p.ended || all) {
                break;
            }
            out.extend(p.held.drain(..).map(|item| item.projected(*pid)));
            p.released = true;
        }
    }

    fn end(&mut self, pid: u64, end: End, out: &mut Vec<Projected>) -> Result<(), String> {
        // A process that ends inside a `write` keeps the bytes it wrote.
        if let Some((_, bytes)) = self.writing.take_if(|(w, _)| *w == pid)
            && !bytes.is_empty()
        {
            self.hold(pid, Item::Out(bytes))?;
        }
        self.hold(pid, Item::End(end))?;
        self.procs.get_mut(&pid).expect("held").ended = true;
        self.release(false, out);
        Ok(())
    }

    /// Reads one kernel or UART record into `outcome` and `schedule`.
    fn take(&mut self, record: &TraceRecord, out: &mut Vec<Projected>) -> Result<(), String> {
        let u = |v: u64| present(Value::U64(v));
        let at = |why: String| format!("{}: {why}", record.kind);
        match record.kind {
            k if k == TX_KIND => match &mut self.writing {
                Some((_, bytes)) => {
                    let byte = u64_of(record, "byte")?;
                    bytes.push(u8::try_from(byte).map_err(|_| at(format!("byte {byte}")))?);
                }
                None => return Err(at("a UART byte outside a write".to_owned())),
            },
            k if k == CPU_INTERRUPT_KIND => self.epc = Some(u64_of(record, "epc")?),
            k if k == CREATE_KIND => {
                if matches!(get(record, "error")?, Value::Str(e) if e.is_empty()) {
                    let pid = u64_of(record, "pid")?;
                    if self.procs.insert(pid, Proc::default()).is_some() {
                        return Err(at(format!("pid {pid} created twice")));
                    }
                }
            }
            k if k == SYSCALL_ENTER_KIND => {
                if u64_of(record, "nr")? == u64::from(SYS_WRITE) {
                    let pid = u64_of(record, "pid")?;
                    if let Some((w, _)) = self.writing.replace((pid, Vec::new())) {
                        return Err(at(format!("pid {w}'s write is still in progress")));
                    }
                }
            }
            k if k == SYSCALL_EXIT_KIND => {
                let (pid, nr) = (u64_of(record, "pid")?, u64_of(record, "nr")?);
                if nr == u64::from(SYS_WRITE) {
                    match self.writing.take() {
                        Some((w, bytes)) if w == pid => self.hold(pid, Item::Out(bytes))?,
                        _ => return Err(at(format!("pid {pid} ends a write it did not start"))),
                    }
                }
                if INVARIANT_SYSCALLS.iter().any(|&s| u64::from(s) == nr) {
                    let value = u64_of(record, "ret")?;
                    self.hold(pid, Item::Ret { nr, value })?;
                }
            }
            k if k == EXIT_KIND => {
                let status = match get(record, "status")? {
                    Value::I64(s) => *s,
                    other => return Err(at(format!("status is {other:?}"))),
                };
                self.end(u64_of(record, "pid")?, End::Exit(status), out)?;
            }
            k if k == FAULT_KIND => {
                let cause = match get(record, "cause")? {
                    Value::Str(c) => c.clone(),
                    other => return Err(at(format!("cause is {other:?}"))),
                };
                let end = End::Fault {
                    cause,
                    epc: u64_of(record, "epc")?,
                    tval: u64_of(record, "tval")?,
                };
                self.end(u64_of(record, "pid")?, end, out)?;
            }
            k if k == SHUTDOWN_KIND => self.release(true, out),
            k if k == FINAL_KIND => {
                self.release(true, out);
                out.push(boundary(
                    OUTCOME,
                    "final",
                    vec![
                        ("word", u(u64_of(record, "word")?)),
                        ("value", u(u64_of(record, "value")?)),
                    ],
                ));
            }
            _ => {}
        }
        if let Some(&(_, kind, fields)) = SCHEDULE_RECORDS.iter().find(|r| r.0 == record.kind) {
            let mut values = Vec::with_capacity(fields.len());
            for f in fields {
                let v = if *f == "sepc" {
                    let epc = self
                        .epc
                        .take()
                        .ok_or_else(|| at("no interrupt before it".to_owned()))?;
                    u(epc)
                } else {
                    present(get(record, f)?.clone())
                };
                values.push((*f, v));
            }
            out.push(boundary(SCHEDULE, kind, values));
        }
        Ok(())
    }
}

impl Projection for M5Projection {
    fn support(&self) -> Support {
        let mut s = Support::default();
        s.streams
            .extend(["arch", "os", OUTCOME, SCHEDULE].map(str::to_owned));
        s.fields.extend(projection::support());
        for spec in [os(), outcome(), schedule()] {
            for k in &spec.kinds {
                s.fields.extend(
                    k.fields
                        .iter()
                        .map(|f| format!("{}/{}/{}", spec.stream, k.kind, f.name)),
                );
            }
        }
        s
    }

    fn begin(&mut self, profile: &ComparisonProfile) -> Result<(), String> {
        self.rv32.begin(profile)?;
        match profile.streams().find(|s| !s.window.is_empty()) {
            Some(s) => Err(format!("window {}", s.window)),
            None => Ok(()),
        }
    }

    fn project(&mut self, record: &TraceRecord, out: &mut Vec<Projected>) -> Result<(), String> {
        if record.kind == HALT_KIND {
            self.state.release(true, out);
            out.push(boundary(
                OUTCOME,
                "limit",
                vec![("instret", present(Value::U64(u64_of(record, "instret")?)))],
            ));
            self.limited = true;
            return Ok(());
        }
        if record.kind.starts_with("rv32.") {
            self.rv32.project(record, out)?;
        }
        if let Some(&(_, kind, fields)) = OS_RECORDS.iter().find(|r| r.0 == record.kind) {
            let names: Vec<&str> = record.fields.iter().map(|(n, _)| *n).collect();
            if names != fields {
                return Err(format!("{}: unexpected fields {names:?}", record.kind));
            }
            out.push(Projected {
                stream: "os".to_owned(),
                kind: kind.to_owned(),
                fields: record
                    .fields
                    .iter()
                    .map(|(n, v)| ((*n).to_owned(), present(v.clone())))
                    .collect(),
            });
        }
        self.state.take(record, out)
    }

    fn finished(&self) -> bool {
        self.rv32.finished() || self.limited
    }

    fn save(&self) -> Vec<u8> {
        self.state.encode()
    }

    fn load(&mut self, state: &[u8]) -> Result<(), String> {
        self.state = State::decode(state).map_err(|e| format!("outcome state: {e}"))?;
        Ok(())
    }
}

/// The default jitter scope (§7.2): the device, DMA, and interrupt links, by
/// declaration index. The CPU's, RAM's, and the kernel's memory links are left out.
pub const JITTER_LINKS: [u32; 9] = [2, 3, 4, 5, 6, 7, 8, 11, 12];

/// The frozen `m5-reference` configuration under `perturbation`.
pub fn platform_config(perturbation: &Perturbation) -> Config {
    defective_config(perturbation, None)
}

/// The frozen `m5-reference` configuration under `perturbation`, its kernel with
/// `defect` planted (§12.3).
pub fn defective_config(perturbation: &Perturbation, defect: Option<Defect>) -> Config {
    let mut c = Config::frozen();
    c.m3.latencies = perturbation.latencies.clone();
    c.m3.jitter = perturbation.jitter.clone();
    c.m3.ties = perturbation.ties;
    c.m3.script = perturbation.script.clone();
    if let Some(slice) = perturbation.slice {
        c.m5.slice = slice;
    }
    c.m5.defect = defect;
    if defect.is_some() {
        c.max_instructions = DEFECT_MAX_INSTRUCTIONS;
    }
    c
}

/// The instruction limit under a planted kernel defect: a kernel that loses a wake never
/// shuts down, and stops here, within the event budget, as M4's stop rule (§20). The
/// workload retires about 45,000 instructions.
pub const DEFECT_MAX_INSTRUCTIONS: NonZeroU64 = NonZeroU64::new(60_000).expect("nonzero");

/// The backend build that names `perturbation` and `defect`: the scenario's name, the
/// planted defect if any, and the perturbation's identity when it is not empty.
pub fn build_name(perturbation: &Perturbation, defect: Option<Defect>) -> String {
    let mut name = SCENARIO.to_owned();
    if let Some(d) = defect {
        name += &format!(" defect {d:?}");
    }
    if !perturbation.is_empty() {
        name += &format!(" perturbed {}", hex(&perturbation.identity()));
    }
    name
}

/// The scenario of `fixture`, unperturbed.
pub fn scenario(fixture: &Fixture) -> Scenario {
    Scenario {
        platform: SCENARIO.to_owned(),
        topology_hash: fixture.platform().topology_hash(),
        cpu_profile: "M5".to_owned(),
        program: fixture.disk_blake3,
        locator: format!("{}/{DISK_FILE}", m5::M5_DIR),
        seed: Config::frozen().m3.seed,
    }
}

/// The canonical run configuration: the M5 event watchdog.
pub fn config() -> BackendRunConfig {
    BackendRunConfig {
        budget: m5::EVENT_BUDGET,
    }
}

/// A SystemScope backend on `m5-reference` with `fixture`'s disk under `perturbation`,
/// able to apply media patches to the disk.
pub fn backend(fixture: &Fixture, perturbation: &Perturbation) -> RuntimeBackend {
    defective_backend(fixture, perturbation, None)
}

/// [`backend`] with `defect` planted in the kernel.
pub fn defective_backend(
    fixture: &Fixture,
    perturbation: &Perturbation,
    defect: Option<Defect>,
) -> RuntimeBackend {
    let fixture = fixture.clone();
    let config = defective_config(perturbation, defect);
    RuntimeBackend::new(
        BackendId {
            name: "systemscope".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: build_name(perturbation, defect),
        },
        Box::new(move |_, input| {
            let mut disk = fixture.disk.clone();
            if let InputVariant::Media(p) = input {
                p.apply(&mut disk)?;
            }
            m5ref::build(&fixture.firmware, &disk, &config)
                .map_err(|e| format!("m5-reference does not build: {e}"))
        }),
        Box::new(|| Box::new(M5Projection::default())),
    )
    .with_decoder(Box::new(super::evidence::decode_state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use systemscope_contracts::component::ComponentId;
    use systemscope_contracts::trace::{TraceAt, TraceOrigin};

    fn record(kind: &'static str, fields: Vec<(&'static str, Value)>) -> TraceRecord {
        TraceRecord {
            at: TraceAt::Init,
            origin: TraceOrigin::Component,
            component: ComponentId(7),
            kind,
            fields,
        }
    }

    fn u(v: u64) -> Value {
        Value::U64(v)
    }

    fn create(pid: u64) -> TraceRecord {
        record(
            CREATE_KIND,
            vec![
                ("pid", u(pid)),
                ("entry", u(0)),
                ("root", u(0)),
                ("error", Value::Str(String::new())),
            ],
        )
    }

    fn enter(pid: u64, nr: u32) -> TraceRecord {
        record(
            SYSCALL_ENTER_KIND,
            vec![
                ("pid", u(pid)),
                ("nr", u(nr.into())),
                ("a0", u(0)),
                ("a1", u(0)),
                ("a2", u(0)),
            ],
        )
    }

    fn exit(pid: u64, nr: u32, ret: u64) -> TraceRecord {
        record(
            SYSCALL_EXIT_KIND,
            vec![("pid", u(pid)), ("nr", u(nr.into())), ("ret", u(ret))],
        )
    }

    fn tx(byte: u8) -> TraceRecord {
        record(TX_KIND, vec![("byte", u(byte.into()))])
    }

    fn gone(pid: u64) -> TraceRecord {
        record(EXIT_KIND, vec![("pid", u(pid)), ("status", Value::I64(0))])
    }

    /// The `outcome` boundaries of `records`, as `kind pid`, checking that a projection
    /// restored from the state saved after every record emits the same.
    fn outcome_of(records: &[TraceRecord]) -> Vec<String> {
        let mut p = M5Projection::default();
        let mut out = Vec::new();
        for r in records {
            let saved = p.save();
            let mut again = M5Projection::default();
            again.load(&saved).unwrap();
            let (mut a, mut b) = (Vec::new(), Vec::new());
            p.project(r, &mut a).unwrap();
            again.project(r, &mut b).unwrap();
            assert_eq!(a, b, "{}", r.kind);
            assert_eq!(p.save(), again.save());
            out.extend(a);
        }
        out.iter()
            .filter(|b| b.stream == OUTCOME)
            .map(|b| {
                let pid = b
                    .fields
                    .iter()
                    .find(|(n, _)| n == "pid")
                    .map(|(_, v)| v.clone());
                format!("{} {pid:?}", b.kind)
            })
            .collect()
    }

    /// Two runs that interleave their processes differently, and do nothing else
    /// differently, give the same `outcome` stream.
    #[test]
    fn outcome_groups_by_pid_whatever_the_interleaving() {
        let one = [
            enter(1, SYS_WRITE),
            tx(b'a'),
            tx(b'b'),
            exit(1, SYS_WRITE, 2),
        ];
        let two = [
            enter(2, SYS_GETPID),
            exit(2, SYS_GETPID, 2),
            enter(2, 124),
            exit(2, 124, 0),
        ];
        let mut first: Vec<TraceRecord> = vec![create(1), create(2)];
        first.extend(one.clone());
        first.extend(two.clone());
        first.extend([gone(1), gone(2)]);
        let mut second: Vec<TraceRecord> = vec![create(1), create(2)];
        second.extend(two.clone());
        second.extend([enter(2, 124), exit(2, 124, 0), gone(2)]);
        second.extend(one.clone());
        second.push(gone(1));
        let a = outcome_of(&first);
        assert_eq!(a, outcome_of(&second));
        let p = |n: u64| format!("Some(Present(U64({n})))");
        assert_eq!(
            a,
            [
                format!("out {}", p(1)),
                format!("ret {}", p(1)),
                format!("end {}", p(1)),
                // `sched_yield` is not in the invariant set.
                format!("ret {}", p(2)),
                format!("end {}", p(2)),
            ]
        );
    }

    #[test]
    fn a_process_is_held_until_every_lower_pid_ends_or_the_shutdown() {
        let shutdown = record(
            SHUTDOWN_KIND,
            vec![("reason", u(0)), ("detail", Value::Str(String::new()))],
        );
        let fin = record(FINAL_KIND, vec![("word", u(0)), ("value", u(300))]);
        let mut p = M5Projection::default();
        let mut out = Vec::new();
        for r in [create(1), create(2), gone(2)] {
            p.project(&r, &mut out).unwrap();
        }
        assert!(out.iter().all(|b| b.stream != OUTCOME));
        p.project(&shutdown, &mut out).unwrap();
        p.project(&fin, &mut out).unwrap();
        let kinds: Vec<&str> = out
            .iter()
            .filter(|b| b.stream == OUTCOME)
            .map(|b| b.kind.as_str())
            .collect();
        assert_eq!(kinds, ["end", "final"]);
    }

    #[test]
    fn a_preemption_carries_the_latest_interrupts_epc() {
        let interrupt = record(
            CPU_INTERRUPT_KIND,
            vec![
                ("epc", u(0x1_0040)),
                ("cause", u(0x8000_0009)),
                ("handler", u(0)),
                ("from", Value::Str("U".to_owned())),
                ("to", Value::Str("S".to_owned())),
            ],
        );
        let preempt = record(PREEMPT_KIND, vec![("pid", u(1))]);
        let mut p = M5Projection::default();
        let mut out = Vec::new();
        assert!(
            p.project(&preempt, &mut out)
                .unwrap_err()
                .contains("no interrupt before it")
        );
        let mut p = M5Projection::default();
        let mut out = Vec::new();
        p.project(&interrupt, &mut out).unwrap();
        let mut resumed = M5Projection::default();
        resumed.load(&p.save()).unwrap();
        resumed.project(&preempt, &mut out).unwrap();
        let b = out.iter().find(|b| b.stream == SCHEDULE).unwrap();
        assert_eq!(
            b.fields,
            [
                ("pid".to_owned(), present(u(1))),
                ("sepc".to_owned(), present(u(0x1_0040)))
            ]
        );
    }

    #[test]
    fn misordered_kernel_records_are_refused() {
        let refused = |records: &[TraceRecord]| {
            let mut p = M5Projection::default();
            let mut out = Vec::new();
            records
                .iter()
                .try_for_each(|r| p.project(r, &mut out))
                .unwrap_err()
        };
        assert!(refused(&[tx(1)]).contains("outside a write"));
        assert!(refused(&[create(1), exit(1, SYS_WRITE, 0)]).contains("did not start"));
        assert!(refused(&[enter(3, SYS_GETPID), exit(3, SYS_GETPID, 3)]).contains("never created"));
        assert!(refused(&[create(1), gone(1), exit(1, SYS_GETPID, 1)]).contains("after its end"));
        assert!(refused(&[create(1), create(1)]).contains("created twice"));
        let mut p = M5Projection::default();
        assert!(p.load(&[]).is_err());
        assert!(p.load(&[0, 0, 0, 0, 0, 0, 1]).is_err());
    }
}
