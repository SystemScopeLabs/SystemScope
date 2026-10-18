//! The relevant-predecessor slice (`docs/m4-design.md` §15): the recorded predecessors of
//! the first differing canonical trace record, found by following only the links the
//! canonical trace records, backwards through the dense replay window before it.
//!
//! A slice is evidence of what came before, never a cause (D12). A relationship the
//! trace does not record does not appear in it.

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};

use crate::profile::Role;
use crate::state::{decode_role, role_tag};
use crate::trace::TraceLine;

/// The records of the dense replay window a slice is built from, before the first
/// differing record: at most this many.
pub const SLICE_RECORDS: usize = 4096;

/// Why a record is in a slice: the link that reached it (§15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// The first differing record itself, where the slice starts.
    Divergent,
    /// The request a response on the same link answers: the `runtime.dispatch` pair with
    /// one protocol and transaction id and source and target swapped.
    Request,
    /// The CPU's port traffic since its previous retirement, the memory accesses of one
    /// instruction: its fetch, its load or store, and its page walk's PTE reads, which
    /// only their ordering identifies (the CPU has one outstanding access).
    Access,
    /// A trap's way into a system call: `rv32.exception` or `rv32.trap`, then
    /// `os.gate.enter`, then `os.syscall.enter`.
    Trap,
    /// A DMA transfer: `platform.blk.command`, the controller's media requests and memory
    /// beats, `platform.disk.*`, `platform.blk.done`, and the interrupt it raises.
    Dma,
    /// A kernel gate's held `ENTER`: the write that entered it, `os.gate.enter`, the
    /// kernel's own memory accesses, and `os.gate.release`.
    Gate,
}

impl Link {
    fn tag(self) -> u8 {
        match self {
            Link::Divergent => 0,
            Link::Request => 1,
            Link::Access => 2,
            Link::Trap => 3,
            Link::Dma => 4,
            Link::Gate => 5,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Link::Divergent => "divergent",
            Link::Request => "request",
            Link::Access => "access",
            Link::Trap => "trap",
            Link::Dma => "dma",
            Link::Gate => "gate",
        }
    }
}

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One record of a slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceEntry {
    /// The record's index after the trace scan's origin.
    pub record: u64,
    /// The link that included it.
    pub link: Link,
    /// The record.
    pub line: TraceLine,
}

/// One side's recorded predecessors of its first differing trace record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice {
    /// The side.
    pub role: Role,
    /// The index after the origin of the window's first record: a predecessor before it
    /// is outside the window and not in the slice.
    pub first: u64,
    /// The included records in trace order, the divergent one last.
    pub entries: Vec<SliceEntry>,
}

impl Slice {
    /// Appends the canonical encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.u8(role_tag(self.role));
        e.u64(self.first);
        e.len(self.entries.len());
        for s in &self.entries {
            e.u64(s.record);
            e.u8(s.link.tag());
            s.line.encode(e);
        }
    }

    /// Reads one slice.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Slice, DecodeError> {
        let role = decode_role(d)?;
        let first = d.u64()?;
        let n = d.len()?;
        let mut entries = Vec::new();
        for _ in 0..n {
            let record = d.u64()?;
            let link = match d.u8()? {
                0 => Link::Divergent,
                1 => Link::Request,
                2 => Link::Access,
                3 => Link::Trap,
                4 => Link::Dma,
                5 => Link::Gate,
                tag => return Err(DecodeError::InvalidTag { what: "link", tag }),
            };
            entries.push(SliceEntry {
                record,
                link,
                line: TraceLine::decode(d)?,
            });
        }
        Ok(Slice {
            role,
            first,
            entries,
        })
    }
}

const REQUESTS: [&str; 4] = ["ReadReq", "WriteReq", "ReadBlock", "WriteBlock"];
const RESPONSES: [&str; 4] = ["ReadResp", "WriteResp", "ReadResult", "WriteResult"];
const RETIREMENTS: [&str; 5] = [
    "rv32.commit",
    "rv32.trap",
    "rv32.exception",
    "rv32.interrupt",
    "rv32.halt",
];
const IRQ: [&str; 3] = ["platform.irq.level", "platform.irq.meip", "rv32.interrupt"];

fn field<'r>(r: &'r TraceRecord, name: &str) -> Option<&'r Value> {
    r.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
}

fn int(r: &TraceRecord, name: &str) -> Option<u64> {
    match field(r, name) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

fn text<'r>(r: &'r TraceRecord, name: &str) -> Option<&'r str> {
    match field(r, name) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// A message dispatch: its source, its target, and its message name.
struct Dispatch<'r> {
    source: u64,
    target: u64,
    msg: &'r str,
}

fn dispatch(r: &TraceRecord) -> Option<Dispatch<'_>> {
    if r.origin != TraceOrigin::Runtime || r.kind != "runtime.dispatch" {
        return None;
    }
    Some(Dispatch {
        source: int(r, "source")?,
        target: u64::from(r.component.0),
        msg: text(r, "msg")?,
    })
}

fn component(r: &TraceRecord) -> Option<u64> {
    (r.origin == TraceOrigin::Component).then_some(u64::from(r.component.0))
}

/// The components that emit `kind`, in the window.
fn emitters(records: &[TraceRecord], kind: &str) -> Vec<u64> {
    let mut out: Vec<u64> = records
        .iter()
        .filter(|r| r.kind == kind)
        .filter_map(component)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Builds `role`'s slice from the dense window `records`, whose last record is the first
/// differing one and whose first has index `first` after the origin. `paths` names the
/// components.
pub fn slice(records: &[TraceRecord], first: u64, paths: &[String], role: Role) -> Slice {
    let mut links: Vec<Option<Link>> = vec![None; records.len()];
    let Some(anchor) = records.len().checked_sub(1) else {
        return Slice {
            role,
            first,
            entries: Vec::new(),
        };
    };
    links[anchor] = Some(Link::Divergent);
    let kernels = emitters(records, "os.gate.enter");
    let devices = emitters(records, "platform.blk.command");
    let mut media = emitters(records, "platform.disk.read");
    media.extend(emitters(records, "platform.disk.write"));
    // Every link reaches back only, so one pass from the divergent record down sees
    // every included record before its predecessors.
    for i in (0..=anchor).rev() {
        if links[i].is_none() {
            continue;
        }
        let r = &records[i];
        let mut add = |j: usize, link: Link| {
            if links[j].is_none() {
                links[j] = Some(link);
            }
        };
        let before = &records[..i];
        if let Some(j) = request(before, r) {
            add(j, Link::Request);
        }
        for j in accesses(before, r) {
            add(j, Link::Access);
        }
        for j in trap(before, r) {
            add(j, Link::Trap);
        }
        for j in gate(before, r, &kernels) {
            add(j, Link::Gate);
        }
        for j in dma(before, r, &devices, &media) {
            add(j, Link::Dma);
        }
    }
    let entries = links
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            l.map(|link| SliceEntry {
                record: first + i as u64,
                link,
                line: TraceLine::of(&records[i], paths),
            })
        })
        .collect();
    Slice {
        role,
        first,
        entries,
    }
}

/// The request a response answers: the latest dispatch of the same protocol and
/// transaction id from the response's target to its source.
fn request(before: &[TraceRecord], r: &TraceRecord) -> Option<usize> {
    let d = dispatch(r)?;
    if !RESPONSES.contains(&d.msg) {
        return None;
    }
    let (protocol, txn) = (text(r, "protocol")?, int(r, "txn")?);
    before.iter().rposition(|q| {
        dispatch(q).is_some_and(|x| {
            REQUESTS.contains(&x.msg)
                && x.source == d.target
                && x.target == d.source
                && text(q, "protocol") == Some(protocol)
                && int(q, "txn") == Some(txn)
        })
    })
}

/// A CPU's port traffic since its previous retirement: requests it sent and responses
/// it received. Reached from a retirement, or from one of those accesses, whose earlier
/// accesses in the same instruction precede it.
fn accesses(before: &[TraceRecord], r: &TraceRecord) -> Vec<usize> {
    let cpu = if RETIREMENTS.contains(&r.kind) && r.kind != "rv32.interrupt" {
        component(r)
    } else {
        // A request a CPU sent, if the window shows that component retiring at all.
        dispatch(r)
            .filter(|d| REQUESTS.contains(&d.msg))
            .map(|d| d.source)
            .filter(|c| {
                before
                    .iter()
                    .any(|q| RETIREMENTS.contains(&q.kind) && component(q) == Some(*c))
            })
    };
    let Some(cpu) = cpu else {
        return Vec::new();
    };
    let start = before
        .iter()
        .rposition(|q| RETIREMENTS.contains(&q.kind) && component(q) == Some(cpu))
        .map_or(0, |k| k + 1);
    (start..before.len())
        .filter(|&j| {
            dispatch(&before[j]).is_some_and(|d| {
                (d.source == cpu && REQUESTS.contains(&d.msg))
                    || (d.target == cpu && RESPONSES.contains(&d.msg))
            })
        })
        .collect()
}

/// `os.syscall.enter` follows `os.gate.enter`, which follows the trap that left user
/// mode.
fn trap(before: &[TraceRecord], r: &TraceRecord) -> Vec<usize> {
    let want: &[&str] = match r.kind {
        "os.syscall.enter" => &["os.gate.enter"],
        "os.gate.enter" => &["rv32.exception", "rv32.trap"],
        _ => return Vec::new(),
    };
    before
        .iter()
        .rposition(|q| want.contains(&q.kind))
        .into_iter()
        .collect()
}

/// Whether `r` is kernel `k`'s own: its records and the dispatches it sends or receives.
fn kernel_owns(r: &TraceRecord, k: u64) -> bool {
    component(r) == Some(k) || dispatch(r).is_some_and(|d| d.source == k || d.target == k)
}

/// A held `ENTER`: from its release or anything the kernel does while it is held, back
/// to `os.gate.enter` and the write that entered, with the kernel's traffic between.
fn gate(before: &[TraceRecord], r: &TraceRecord, kernels: &[u64]) -> Vec<usize> {
    let mut out = Vec::new();
    for &k in kernels {
        if r.kind == "os.gate.enter" && component(r) == Some(k) {
            // The write that entered: the dispatch to the kernel in the same event.
            if let Some(j) = before.iter().rposition(|q| {
                q.at == r.at && dispatch(q).is_some_and(|d| d.target == k && d.msg == "WriteReq")
            }) {
                out.push(j);
            }
            continue;
        }
        if !kernel_owns(r, k) {
            continue;
        }
        // The held `ENTER`'s response: the release in the same event that sent it.
        if dispatch(r).is_some_and(|d| d.source == k && d.msg == "WriteResp")
            && let Some(j) = before.iter().rposition(|q| {
                q.at == r.at
                    && q.kind == "os.gate.release"
                    && component(q) == Some(k)
                    && int(q, "txn") == int(r, "txn")
            })
        {
            out.push(j);
            continue;
        }
        let Some(enter) = before
            .iter()
            .rposition(|q| q.kind == "os.gate.enter" && component(q) == Some(k))
        else {
            continue;
        };
        let held = int(&before[enter], "txn");
        let released = |q: &TraceRecord| {
            q.kind == "os.gate.release" && component(q) == Some(k) && int(q, "txn") == held
        };
        // Past a release, the kernel's work is not that `ENTER`'s, unless `r` is the
        // release itself.
        if before[enter + 1..].iter().any(released) {
            continue;
        }
        if r.kind == "os.gate.release" && int(r, "txn") != held {
            continue;
        }
        out.push(enter);
        out.extend((enter + 1..before.len()).filter(|&j| kernel_owns(&before[j], k)));
    }
    out
}

fn is_irq(r: &TraceRecord) -> bool {
    IRQ.contains(&r.kind) || text(r, "protocol").is_some_and(|p| p.starts_with("irq"))
}

/// Whether `r` is part of controller `c`'s transfers: its media requests and memory
/// beats, the responses and media results it receives, and the media's records. Its
/// MMIO traffic, such as status polls, is not.
fn transfer_part(r: &TraceRecord, c: u64, media: &[u64]) -> bool {
    if let Some(d) = dispatch(r) {
        return (d.source == c && REQUESTS.contains(&d.msg))
            || (d.target == c && (RESPONSES.contains(&d.msg)) && d.source != c);
    }
    component(r).is_some_and(|x| media.contains(&x))
}

/// A DMA transfer, `docs/m4-design.md` §15: an interrupt follows `platform.blk.done`
/// and the interrupt records since; the completion follows the whole transfer since
/// `platform.blk.command`; any part of a transfer follows its command, a memory beat
/// also the latest media result it carries, and a media result the medium's latest
/// record; the command follows the MMIO write that issued it in the same event.
fn dma(before: &[TraceRecord], r: &TraceRecord, devices: &[u64], media: &[u64]) -> Vec<usize> {
    let mut out = Vec::new();
    if is_irq(r)
        && let Some(done) = before.iter().rposition(|q| q.kind == "platform.blk.done")
    {
        out.push(done);
        out.extend((done + 1..before.len()).filter(|&j| is_irq(&before[j])));
    }
    for &c in devices {
        let mine = component(r) == Some(c);
        if r.kind == "platform.blk.command" && mine {
            if let Some(j) = before.iter().rposition(|q| {
                q.at == r.at && dispatch(q).is_some_and(|d| d.target == c && d.msg == "WriteReq")
            }) {
                out.push(j);
            }
            continue;
        }
        let done = r.kind == "platform.blk.done" && mine;
        if !done && !transfer_part(r, c, media) {
            continue;
        }
        let Some(command) = before
            .iter()
            .rposition(|q| q.kind == "platform.blk.command" && component(q) == Some(c))
        else {
            continue;
        };
        out.push(command);
        let since = command + 1..before.len();
        if done {
            out.extend(since.filter(|&j| transfer_part(&before[j], c, media)));
            continue;
        }
        let Some(d) = dispatch(r) else {
            continue;
        };
        if d.source == c && !media.contains(&d.target) {
            // A memory beat: the media result whose data it moves.
            out.extend(since.rev().find(|&j| {
                dispatch(&before[j]).is_some_and(|x| x.target == c && media.contains(&x.source))
            }));
        } else if media.contains(&d.source) {
            // A media result: the medium's record of that access.
            out.extend(
                since
                    .rev()
                    .find(|&j| component(&before[j]) == Some(d.source)),
            );
        }
    }
    out
}
