//! The relevant-predecessor slice's links (`docs/m4-design.md` §15), on hand-built
//! windows shaped like the m3 platform's trace: CPU port traffic through a bus, a trap
//! into a kernel gate, and a DMA transfer that ends in an interrupt.

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::event::{EventKey, Phase};
use systemscope_contracts::time::Tick;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_verify::profile::Role;
use systemscope_verify::slice::{Link, Slice, Unresolved, slice};

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
        unresolved: vec![Unresolved {
            record: 3,
            addr: 0x104,
            len: 2,
        }],
    });
    all.push(slice(&renamed(), 0, &paths(), Role::Actual));
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
        Link::Rename,
        Link::Store,
    ] {
        assert!(!link.to_string().is_empty());
    }
    // Role, first, one entry: record 0 and link tag 8.
    let mut e = Encoder::new();
    e.u8(0);
    e.u64(0);
    e.len(1);
    e.u64(0);
    e.u8(8);
    let bytes = e.into_bytes();
    assert_eq!(
        Slice::decode(&mut Decoder::new(&bytes)),
        Err(DecodeError::InvalidTag {
            what: "link",
            tag: 8
        })
    );
}

/// `r` with `fields` appended.
fn with(mut r: TraceRecord, fields: &[(&'static str, Value)]) -> TraceRecord {
    r.fields.extend(fields.iter().cloned());
    r
}

fn on_port(mut r: TraceRecord, port: u64) -> TraceRecord {
    r.fields[1].1 = Value::U64(port);
    r
}

fn grant(event: u64, master: u64, txn: u64, downstream: u64) -> TraceRecord {
    grant_on(BUS, event, master, txn, downstream)
}

fn grant_on(bus: u32, event: u64, master: u64, txn: u64, downstream: u64) -> TraceRecord {
    with(
        c(event, bus, "platform.bus.grant", None),
        &[
            ("region", Value::U64(0)),
            ("master", Value::U64(master)),
            ("txn", Value::U64(txn)),
            ("downstream_txn", Value::U64(downstream)),
        ],
    )
}

fn data(b: &[u8]) -> (&'static str, Value) {
    ("data", Value::Bytes(b.to_vec()))
}

fn addr(a: u64) -> (&'static str, Value) {
    ("addr", Value::U64(a))
}

/// A CPU store through the bus, then a load of partly the same bytes.
fn renamed() -> Vec<TraceRecord> {
    vec![
        with(
            mem(0, CPU, BUS, "WriteReq", 1),
            &[addr(0x100), data(&[1, 2, 3, 4])],
        ),
        grant(1, 0, 1, 7),
        with(
            mem(2, BUS, RAM, "WriteReq", 7),
            &[addr(0), data(&[1, 2, 3, 4])],
        ),
        mem(3, RAM, BUS, "WriteResp", 7),
        mem(4, BUS, CPU, "WriteResp", 1),
        with(mem(5, CPU, BUS, "ReadReq", 2), &[addr(0x102)]),
        grant(6, 0, 2, 8),
        with(mem(7, BUS, RAM, "ReadReq", 8), &[addr(2)]),
        with(mem(8, RAM, BUS, "ReadResp", 8), &[data(&[3, 4, 9, 9])]),
        with(mem(9, BUS, CPU, "ReadResp", 2), &[data(&[3, 4, 9, 9])]),
    ]
}

#[test]
fn a_relayed_response_reaches_through_the_bus_rename() {
    let w = renamed();
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Store),
            (5, Link::Request),
            (6, Link::Rename),
            (7, Link::Request),
            (8, Link::Rename),
            (9, Link::Divergent),
        ]
    );
    // The store gives the first two loaded bytes; nothing in the window gives the rest.
    assert_eq!(
        s.unresolved,
        [Unresolved {
            record: 9,
            addr: 0x104,
            len: 2
        }]
    );
    // A downstream request: its grant, and the master request the grant forwards.
    let s = slice(&w[..=2], 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [(0, Link::Rename), (1, Link::Rename), (2, Link::Divergent)]
    );
    assert_eq!(s.unresolved, []);
}

#[test]
fn a_rename_and_a_store_need_their_own_master_and_bytes() {
    let mut w = renamed();
    // Another master's request with the same `txn`, granted later: not the CPU's.
    w.insert(
        7,
        on_port(with(mem(6, BLK, BUS, "ReadReq", 2), &[addr(0x200)]), 1),
    );
    w.insert(8, grant(6, 1, 2, 9));
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Store),
            (5, Link::Request),
            (6, Link::Rename),
            (9, Link::Request),
            (10, Link::Rename),
            (11, Link::Divergent),
        ]
    );
    // A later store of other bytes hides the earlier one: the replay gives what the
    // latest write wrote, and that is not what was loaded.
    let mut w = renamed();
    w.insert(
        5,
        with(mem(4, CPU, BUS, "WriteReq", 3), &[addr(0x103), data(&[7])]),
    );
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Store),
            (6, Link::Request),
            (7, Link::Rename),
            (8, Link::Request),
            (9, Link::Rename),
            (10, Link::Divergent),
        ]
    );
    assert_eq!(
        s.unresolved,
        [Unresolved {
            record: 10,
            addr: 0x103,
            len: 3
        }]
    );
}

/// `w` with `r` inserted at `at`.
fn put(mut w: Vec<TraceRecord>, at: usize, r: TraceRecord) -> Vec<TraceRecord> {
    w.insert(at, r);
    w
}

fn each(ids: &[u64], link: Link, last: u64) -> Vec<(u64, Link)> {
    let mut out: Vec<(u64, Link)> = ids.iter().map(|&i| (i, link)).collect();
    out.push((last, Link::Divergent));
    out
}

#[test]
fn accesses_are_the_cpus_own_dispatches_since_its_own_retirement() {
    // A runtime record of another kind is no dispatch, even with a dispatch's fields.
    let queued = TraceRecord {
        kind: "runtime.queue",
        ..mem(5, CPU, BUS, "ReadReq", 3)
    };
    let s = slice(&put(instruction(), 6, queued), 0, &paths(), Role::Actual);
    assert_eq!(links(&s), each(&[1, 5, 7, 10], Link::Access, 11));
    // Another CPU's retirement does not end this CPU's instruction.
    let other = c(5, 3, "rv32.commit", None);
    let s = slice(&put(instruction(), 6, other), 0, &paths(), Role::Actual);
    assert_eq!(links(&s), each(&[1, 5, 7, 10], Link::Access, 11));
    // A record no rule reaches links nothing, inside a held gate or after a transfer.
    let unrelated = c(5, RAM, "platform.ram.note", None);
    let held = gated()[..6].to_vec();
    let s = slice(&put(held, 6, unrelated.clone()), 0, &paths(), Role::Actual);
    assert_eq!(links(&s), [(6, Link::Divergent)]);
    let s = slice(&put(transfer(), 15, unrelated), 0, &paths(), Role::Actual);
    assert_eq!(links(&s), [(15, Link::Divergent)]);
}

fn gated() -> Vec<TraceRecord> {
    vec![
        c(0, CPU, "rv32.exception", None),
        mem(1, CPU, BUS, "WriteReq", 3),
        mem(2, BUS, KERNEL, "WriteReq", 11),
        c(2, KERNEL, "os.gate.enter", Some(11)),
        c(2, KERNEL, "os.syscall.enter", None),
        mem(3, KERNEL, BUS, "ReadReq", 1),
        mem(4, BUS, KERNEL, "ReadResp", 1),
        c(4, KERNEL, "os.gate.release", Some(11)),
        mem(4, KERNEL, BUS, "WriteResp", 11),
    ]
}

#[test]
fn a_gate_links_only_its_own_write_release_and_held_work() {
    let enter = c(2, KERNEL, "os.gate.enter", Some(11));
    // The write that entered is in the same event, to the kernel.
    let w = vec![mem(1, BUS, KERNEL, "WriteReq", 11), enter.clone()];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(1, Link::Divergent)]
    );
    let w = vec![
        mem(2, BUS, KERNEL, "WriteReq", 11),
        mem(2, BUS, RAM, "WriteReq", 12),
        enter.clone(),
    ];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(0, Link::Gate), (2, Link::Divergent)]
    );
    // The release's response reaches the release, not another kernel record with its
    // transaction id in that event.
    let w = put(gated(), 8, c(4, KERNEL, "os.gate.note", Some(11)));
    let s = slice(&w, 0, &paths(), Role::Actual);
    let mut want = vec![(0, Link::Trap), (2, Link::Request)];
    want.extend([3, 4, 5, 6, 7].map(|i| (i, Link::Gate)));
    want.push((9, Link::Divergent));
    assert_eq!(links(&s), want);
    // Only a response from the kernel is a release's.
    let mut w = gated()[..8].to_vec();
    w.push(mem(4, KERNEL, BUS, "ReadReq", 11));
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(8, Link::Divergent)]
    );
    // Only a release ends a held gate, not another record with its transaction id.
    let w = put(
        gated()[..6].to_vec(),
        4,
        c(2, KERNEL, "os.gate.note", Some(11)),
    );
    let s = slice(&w, 0, &paths(), Role::Actual);
    let mut want = vec![(0, Link::Trap)];
    want.extend([2, 3, 4, 5].map(|i| (i, Link::Gate)));
    want.push((6, Link::Divergent));
    assert_eq!(links(&s), want);
    // A release before the enter, of that transaction id or another, is an earlier
    // gate's.
    for txn in [11, 5] {
        let w = vec![
            c(1, KERNEL, "os.gate.release", Some(txn)),
            enter.clone(),
            mem(3, KERNEL, BUS, "ReadReq", 1),
        ];
        assert_eq!(
            links(&slice(&w, 0, &paths(), Role::Actual)),
            [(1, Link::Gate), (2, Link::Divergent)],
            "{txn}"
        );
    }
}

#[test]
fn a_transfer_links_only_its_own_command_parts_and_interrupt() {
    // A status poll after the media result is neither a part of the transfer nor the
    // result a beat moves.
    let w = put(transfer(), 7, mem(3, BUS, BLK, "ReadReq", 21));
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        each(&[0, 1, 3, 4, 6, 8, 9, 10, 11, 12, 13, 14], Link::Dma, 15)
    );
    let s = slice(&w[..=8], 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Dma),
            (1, Link::Dma),
            (3, Link::Request),
            (4, Link::Dma),
            (6, Link::Dma),
            (8, Link::Divergent)
        ]
    );
    // Interrupt records before the completion are not this interrupt's.
    let w = put(transfer(), 9, c(5, IRQC, "platform.irq.meip", None));
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        each(&[0, 1, 3, 4, 6, 7, 8, 10, 11, 12, 13, 14], Link::Dma, 15)
    );
    // The command's write is in the same event, to the controller.
    let w = vec![
        mem(0, BUS, BLK, "WriteReq", 20),
        c(1, BLK, "platform.blk.command", None),
    ];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(1, Link::Divergent)]
    );
    let command = c(0, BLK, "platform.blk.command", None);
    let w = vec![
        mem(0, BUS, BLK, "WriteReq", 20),
        mem(0, BUS, RAM, "WriteReq", 21),
        command.clone(),
    ];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(0, Link::Dma), (2, Link::Divergent)]
    );
    // The controller's other records are neither its command nor its completion.
    let w = put(transfer(), 2, c(1, BLK, "platform.blk.status", None));
    let s = slice(&w[..=7], 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Dma),
            (1, Link::Dma),
            (4, Link::Request),
            (5, Link::Dma),
            (7, Link::Divergent)
        ]
    );
    let s = slice(&w[..=2], 0, &paths(), Role::Actual);
    assert_eq!(links(&s), [(2, Link::Divergent)]);
    // A medium's record before the command is an earlier transfer's.
    let w = vec![
        c(0, DISK, "platform.disk.read", None),
        command,
        c(1, BLK, "platform.blk.done", None),
    ];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(1, Link::Dma), (2, Link::Divergent)]
    );
}

#[test]
fn a_rename_and_a_store_stay_on_their_own_bus_and_bytes() {
    // Another bus's grant forwards no request to this bus.
    let w = vec![mem(0, CPU, BUS, "ReadReq", 1), grant_on(3, 1, 0, 1, 5)];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(1, Link::Divergent)]
    );
    // The downstream response follows its grant.
    let w = vec![
        mem(0, CPU, BUS, "ReadReq", 2),
        mem(1, RAM, BUS, "ReadResp", 8),
        grant(2, 0, 2, 8),
        mem(3, BUS, CPU, "ReadResp", 2),
    ];
    assert_eq!(
        links(&slice(&w, 0, &paths(), Role::Actual)),
        [(0, Link::Request), (2, Link::Rename), (3, Link::Divergent)]
    );
    // A master's later request with the downstream id is not the downstream response.
    let w = put(renamed(), 9, mem(8, CPU, BUS, "ReadReq", 8));
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Store),
            (5, Link::Request),
            (6, Link::Rename),
            (7, Link::Request),
            (8, Link::Rename),
            (10, Link::Divergent),
        ]
    );
    // A write that does not go through the bus gives the load nothing.
    let w = put(
        renamed(),
        5,
        with(
            mem(4, BLK, RAM, "WriteReq", 3),
            &[addr(0x104), data(&[9, 9])],
        ),
    );
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [
            (0, Link::Store),
            (6, Link::Request),
            (7, Link::Rename),
            (8, Link::Request),
            (9, Link::Rename),
            (10, Link::Divergent),
        ]
    );
    assert_eq!(
        s.unresolved,
        [Unresolved {
            record: 10,
            addr: 0x104,
            len: 2
        }]
    );
    // Unresolved bytes on both sides of a stored one are two ranges.
    let w = vec![
        grant(0, 0, 9, 9),
        with(mem(0, CPU, BUS, "WriteReq", 1), &[addr(0x101), data(&[5])]),
        with(mem(1, CPU, BUS, "ReadReq", 2), &[addr(0x100)]),
        with(mem(2, BUS, CPU, "ReadResp", 2), &[data(&[0, 5, 0, 0])]),
    ];
    let s = slice(&w, 0, &paths(), Role::Actual);
    assert_eq!(
        links(&s),
        [(1, Link::Store), (2, Link::Request), (3, Link::Divergent)]
    );
    assert_eq!(
        s.unresolved,
        [
            Unresolved {
                record: 3,
                addr: 0x100,
                len: 1
            },
            Unresolved {
                record: 3,
                addr: 0x102,
                len: 2
            }
        ]
    );
}
