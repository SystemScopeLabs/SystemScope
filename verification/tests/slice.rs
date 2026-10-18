//! The relevant-predecessor slice's links (`docs/m4-design.md` §15), on hand-built
//! windows shaped like the m3 platform's trace: CPU port traffic through a bus, a trap
//! into a kernel gate, and a DMA transfer that ends in an interrupt.

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::event::{EventKey, Phase};
use systemscope_contracts::time::Tick;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_verify::profile::Role;
use systemscope_verify::slice::{Link, Slice, slice};

const CPU: u32 = 0;
const BUS: u32 = 1;
const RAM: u32 = 2;
const IRQC: u32 = 4;
const BLK: u32 = 5;
const DISK: u32 = 6;
const KERNEL: u32 = 7;

fn paths() -> Vec<String> {
    [
        "soc.cpu0", "soc.bus", "soc.ram", "soc.uart", "soc.irqc", "soc.blk", "soc.disk", "kernel",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn at(event: u64) -> TraceAt {
    TraceAt::Event(EventKey {
        tick: Tick(event),
        phase: Phase::Commit,
        sequence: event,
    })
}

/// A message dispatch in event `event`.
fn d(event: u64, source: u32, target: u32, protocol: &str, msg: &str, txn: u64) -> TraceRecord {
    let mut fields = vec![
        ("source", Value::U64(u64::from(source))),
        ("port", Value::U64(0)),
        ("protocol", Value::Str(protocol.to_owned())),
        ("version", Value::U64(1)),
        ("msg", Value::Str(msg.to_owned())),
    ];
    if protocol != "irq" {
        fields.push(("txn", Value::U64(txn)));
    }
    TraceRecord {
        at: at(event),
        origin: TraceOrigin::Runtime,
        component: ComponentId(target),
        kind: "runtime.dispatch",
        fields,
    }
}

fn mem(event: u64, source: u32, target: u32, msg: &str, txn: u64) -> TraceRecord {
    d(event, source, target, "mem", msg, txn)
}

/// A component's record in event `event`.
fn c(event: u64, component: u32, kind: &'static str, txn: Option<u64>) -> TraceRecord {
    TraceRecord {
        at: at(event),
        origin: TraceOrigin::Component,
        component: ComponentId(component),
        kind,
        fields: txn.map(|t| ("txn", Value::U64(t))).into_iter().collect(),
    }
}

fn links(s: &Slice) -> Vec<(u64, Link)> {
    s.entries.iter().map(|e| (e.record, e.link)).collect()
}

/// Two instructions' worth of CPU traffic through the bus, the second a load.
fn instruction() -> Vec<TraceRecord> {
    vec![
        c(0, CPU, "rv32.commit", None),
        mem(1, CPU, BUS, "ReadReq", 1),
        c(1, BUS, "platform.bus.grant", None),
        mem(2, BUS, RAM, "ReadReq", 9),
        mem(3, RAM, BUS, "ReadResp", 9),
        mem(4, BUS, CPU, "ReadResp", 1),
        mem(5, CPU, BUS, "ReadReq", 2),
        mem(6, BUS, RAM, "ReadReq", 10),
        mem(7, RAM, BUS, "ReadResp", 10),
        mem(8, BUS, CPU, "ReadResp", 2),
        c(9, CPU, "rv32.commit", None),
    ]
}

#[test]
fn a_retirement_reaches_its_instructions_accesses() {
    let w = instruction();
    let s = slice(&w, 100, &paths(), Role::Actual);
    assert_eq!(s.role, Role::Actual);
    assert_eq!(s.first, 100);
    // The CPU's own requests and responses; the bus's downstream hops are not linked.
    assert_eq!(
        links(&s),
        [
            (101, Link::Access),
            (105, Link::Access),
            (106, Link::Access),
            (109, Link::Access),
            (110, Link::Divergent),
        ]
    );
    assert_eq!(s.entries[4].line.component, "soc.cpu0");
    assert_eq!(s.entries[0].line.component, "soc.bus");
    assert_eq!(s.entries[0].line.kind, "runtime.dispatch");
}

#[test]
fn a_response_reaches_its_request_and_a_request_its_instructions_earlier_accesses() {
    let w = instruction();
    // A response to the CPU: its request, one hop back.
    let s = slice(&w[..=5], 0, &paths(), Role::Expected);
    assert_eq!(links(&s), [(1, Link::Request), (5, Link::Divergent)]);
    // A downstream response: its downstream request.
    let s = slice(&w[..=4], 0, &paths(), Role::Expected);
    assert_eq!(links(&s), [(3, Link::Request), (4, Link::Divergent)]);
    // The load's request: the instruction's fetch before it.
    let s = slice(&w[..=6], 0, &paths(), Role::Expected);
    assert_eq!(
        links(&s),
        [(1, Link::Access), (5, Link::Access), (6, Link::Divergent)]
    );
    // The request is outside the window: no link is invented.
    let s = slice(&w[3..=5], 3, &paths(), Role::Expected);
    assert_eq!(links(&s), [(5, Link::Divergent)]);
    // Nothing links a record that matches no rule.
    let s = slice(&w[..=2], 0, &paths(), Role::Expected);
    assert_eq!(links(&s), [(2, Link::Divergent)]);
    assert_eq!(slice(&[], 4, &paths(), Role::Actual).entries, []);
}

#[test]
fn a_gate_response_reaches_the_trap_that_entered() {
    let w = vec![
        c(0, CPU, "rv32.exception", None),
        mem(1, CPU, BUS, "WriteReq", 3),
        mem(2, BUS, KERNEL, "WriteReq", 11),
        c(2, KERNEL, "os.gate.enter", Some(11)),
        c(2, KERNEL, "os.syscall.enter", None),
        mem(3, KERNEL, BUS, "ReadReq", 1),
        mem(4, BUS, KERNEL, "ReadResp", 1),
        c(4, KERNEL, "os.gate.release", Some(11)),
        mem(4, KERNEL, BUS, "WriteResp", 11),
    ];
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Trap),
            (2, Link::Request),
            (3, Link::Gate),
            (4, Link::Gate),
            (5, Link::Gate),
            (6, Link::Gate),
            (7, Link::Gate),
            (8, Link::Divergent),
        ]
    );
    // A syscall reaches its gate entry and the trap before it.
    let s = slice(&w[..=4], 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Trap),
            (2, Link::Gate),
            (3, Link::Trap),
            (4, Link::Divergent)
        ]
    );
    // Kernel work after a release is not that gate's.
    let mut after = w.clone();
    after.push(mem(5, KERNEL, BUS, "ReadReq", 2));
    let s = slice(&after, 0, &paths(), Role::Actual);
    assert_eq!(links(&s), [(9, Link::Divergent)]);
    // Nor is a release of another transaction.
    let mut other = w[..7].to_vec();
    other.push(c(4, KERNEL, "os.gate.release", Some(12)));
    let s = slice(&other, 0, &paths(), Role::Actual);
    assert_eq!(links(&s), [(7, Link::Divergent)]);
}

fn transfer() -> Vec<TraceRecord> {
    vec![
        mem(0, BUS, BLK, "WriteReq", 20),
        c(0, BLK, "platform.blk.command", None),
        mem(0, BLK, BUS, "WriteResp", 20),
        d(1, BLK, DISK, "block", "ReadBlock", 1),
        c(1, DISK, "platform.disk.read", None),
        mem(2, CPU, BUS, "ReadReq", 5),
        d(3, DISK, BLK, "block", "ReadResult", 1),
        mem(4, BLK, BUS, "WriteReq", 2),
        mem(5, BUS, BLK, "WriteResp", 2),
        c(5, BLK, "platform.blk.done", None),
        d(5, BLK, IRQC, "irq", "Level", 0),
        c(5, IRQC, "platform.irq.level", None),
        c(5, IRQC, "platform.irq.meip", None),
        d(5, IRQC, CPU, "irq", "Level", 0),
        c(6, CPU, "rv32.interrupt", None),
    ]
}

#[test]
fn an_interrupt_reaches_back_through_its_dma_transfer() {
    let w = transfer();
    let s = slice(&w, 0, &paths(), Role::Actual);
    let mut want: Vec<(u64, Link)> = [0, 1, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13]
        .map(|i| (i, Link::Dma))
        .to_vec();
    want.push((14, Link::Divergent));
    // The whole transfer, but neither the controller's MMIO response nor the CPU's
    // unrelated read.
    assert_eq!(links(&s), want);
    // A media result: its request, the medium's record, the command and its write.
    let s = slice(&w[..=6], 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Dma),
            (1, Link::Dma),
            (3, Link::Request),
            (4, Link::Dma),
            (6, Link::Divergent)
        ]
    );
    // A memory beat: the media result whose data it moves.
    let s = slice(&w[..=7], 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Dma),
            (1, Link::Dma),
            (3, Link::Request),
            (4, Link::Dma),
            (6, Link::Dma),
            (7, Link::Divergent)
        ]
    );
    // Without the command in the window, only the request is recorded.
    let s = slice(&w[2..=6], 2, &paths(), Role::Actual);
    assert_eq!(links(&s), [(3, Link::Request), (6, Link::Divergent)]);
}

#[test]
fn slices_round_trip() {
    let mut all = vec![
        slice(&instruction(), 3, &paths(), Role::Expected),
        slice(&transfer(), 0, &paths(), Role::Actual),
        slice(&[], 0, &paths(), Role::Actual),
    ];
    all.push(Slice {
        role: Role::Expected,
        first: 0,
        entries: all[1].entries.clone(),
    });
    for s in all {
        let mut e = Encoder::new();
        s.encode(&mut e);
        let bytes = e.into_bytes();
        let mut r = Decoder::new(&bytes);
        assert_eq!(Slice::decode(&mut r), Ok(s));
        assert_eq!(r.remaining(), 0);
    }
    for link in [
        Link::Divergent,
        Link::Request,
        Link::Access,
        Link::Trap,
        Link::Dma,
        Link::Gate,
    ] {
        assert!(!link.to_string().is_empty());
    }
    // Role, first, one entry: record 0 and link tag 6.
    let mut e = Encoder::new();
    e.u8(0);
    e.u64(0);
    e.len(1);
    e.u64(0);
    e.u8(6);
    let bytes = e.into_bytes();
    assert_eq!(
        Slice::decode(&mut Decoder::new(&bytes)),
        Err(DecodeError::InvalidTag {
            what: "link",
            tag: 6
        })
    );
}
