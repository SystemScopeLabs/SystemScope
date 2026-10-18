//! The `arch` projection of `rv32.*` records (`docs/m4-design.md` §8.2): field mapping,
//! `Absent` values, the halting trap, and the records it refuses.

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_rv32::projection::{
    EXCEPTION_FIELDS, HALT_TRAP, RETIRE_FIELDS, Rv32Projection, WRITE_TOHOST, arch_m3, support,
};
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::ComparisonProfile;
use systemscope_verify::runtime::Projected;

fn record(kind: &'static str, fields: Vec<(&'static str, Value)>) -> TraceRecord {
    TraceRecord {
        at: TraceAt::Init,
        origin: TraceOrigin::Component,
        component: ComponentId(0),
        kind,
        fields,
    }
}

fn u(v: u64) -> Value {
    Value::U64(v)
}

fn s(v: &str) -> Value {
    Value::Str(v.to_owned())
}

fn project(p: &mut Rv32Projection, r: TraceRecord) -> Result<Vec<Projected>, String> {
    let mut out = Vec::new();
    p.project(&r, &mut out)?;
    Ok(out)
}

fn one(r: TraceRecord) -> Projected {
    let mut out = project(&mut Rv32Projection::default(), r).unwrap();
    assert_eq!(out.len(), 1);
    out.remove(0)
}

fn fields(p: &Projected) -> Vec<(&str, ObservedValue)> {
    p.fields
        .iter()
        .map(|(n, v)| (n.as_str(), v.clone()))
        .collect()
}

fn present(v: Value) -> ObservedValue {
    ObservedValue::Present(v)
}

const ABSENT: ObservedValue = ObservedValue::Absent;

fn commit(rd: u64, tail: Vec<(&'static str, Value)>) -> TraceRecord {
    let mut f = vec![
        ("pc", u(0x100)),
        ("insn", u(0x13)),
        ("rd", u(rd)),
        ("rd_value", u(7)),
        ("next_pc", u(0x104)),
        ("priv", u(0)),
    ];
    f.extend(tail);
    record("rv32.commit", f)
}

#[test]
fn a_commit_is_a_retire_in_field_order() {
    let p = one(commit(5, Vec::new()));
    assert_eq!((p.stream.as_str(), p.kind.as_str()), ("arch", "retire"));
    let names: Vec<_> = p.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, RETIRE_FIELDS);
    assert_eq!(
        fields(&p),
        [
            ("pc", present(u(0x100))),
            ("insn", present(u(0x13))),
            ("priv", present(u(0))),
            ("rd", present(u(5))),
            ("rd_value", present(u(7))),
            ("mem", present(s("none"))),
            ("addr", ABSENT),
            ("width", ABSENT),
            ("value", ABSENT),
            ("paddr", ABSENT),
            ("csr", ABSENT),
            ("csr_value", ABSENT),
        ]
    );
}

#[test]
fn x0_writes_loads_stores_and_csrs_map_as_specified() {
    let p = one(commit(0, Vec::new()));
    assert_eq!(fields(&p)[3..5], [("rd", ABSENT), ("rd_value", ABSENT)]);
    let load = one(commit(5, vec![("addr", u(0x40)), ("paddr", u(0x8040))]));
    assert_eq!(
        fields(&load)[5..10],
        [
            ("mem", present(s("load"))),
            ("addr", present(u(0x40))),
            ("width", ABSENT),
            ("value", ABSENT),
            ("paddr", present(u(0x8040))),
        ]
    );
    let store = one(commit(
        0,
        vec![
            ("addr", u(0x40)),
            ("paddr", u(0x40)),
            ("width", u(4)),
            ("value", u(9)),
        ],
    ));
    assert_eq!(
        fields(&store)[5..9],
        [
            ("mem", present(s("store"))),
            ("addr", present(u(0x40))),
            ("width", present(u(4))),
            ("value", present(u(9))),
        ]
    );
    // A suppressed CSR write names its CSR only: not an architectural write.
    let suppressed = one(commit(5, vec![("csr", u(0x300))]));
    assert_eq!(
        fields(&suppressed)[10..],
        [("csr", ABSENT), ("csr_value", ABSENT)]
    );
    let written = one(commit(5, vec![("csr", u(0x300)), ("csr_value", u(8))]));
    assert_eq!(
        fields(&written)[10..],
        [("csr", present(u(0x300))), ("csr_value", present(u(8)))]
    );
    // M1 and M2 commits carry no priv.
    let m1 = one(record(
        "rv32.commit",
        vec![
            ("pc", u(0)),
            ("insn", u(0)),
            ("rd", u(0)),
            ("rd_value", u(0)),
            ("next_pc", u(4)),
        ],
    ));
    assert_eq!(fields(&m1)[2], ("priv", ABSENT));
}

fn exception() -> TraceRecord {
    record(
        "rv32.exception",
        vec![
            ("pc", u(0x200)),
            ("insn", u(0x73)),
            ("cause", s("EnvironmentCallFromU")),
            ("tval", u(0)),
            ("from", s("U")),
            ("to", s("S")),
        ],
    )
}

fn trap() -> TraceRecord {
    record(
        "rv32.trap",
        vec![
            ("pc", u(0x300)),
            ("insn", u(0x73)),
            ("cause", s("EnvironmentCallFromS")),
            ("tval", u(0)),
        ],
    )
}

#[test]
fn exceptions_and_the_halting_trap_are_exceptions() {
    let e = one(exception());
    assert_eq!(e.kind, "exception");
    let names: Vec<_> = e.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, EXCEPTION_FIELDS);
    assert_eq!(fields(&e)[4], ("to", present(s("S"))));
    let mut p = Rv32Projection::default();
    assert!(!p.finished());
    let t = project(&mut p, trap()).unwrap();
    assert!(p.finished());
    assert_eq!(t[0].kind, "exception");
    assert_eq!(
        fields(&t[0]),
        [
            ("pc", present(u(0x300))),
            ("insn", present(u(0x73))),
            ("cause", present(s("EnvironmentCallFromS"))),
            ("tval", present(u(0))),
            ("to", present(s("M"))),
        ]
    );
    // Other components' records pass through even after the trap.
    let other = record("os.shutdown", Vec::new());
    assert_eq!(project(&mut p, other), Ok(Vec::new()));
    assert_eq!(
        project(&mut p, commit(1, Vec::new())),
        Err("rv32.commit: after the halting trap".to_owned())
    );
}

#[test]
fn an_interrupt_is_taken_to_m() {
    let i = one(record(
        "rv32.interrupt",
        vec![
            ("mepc", u(0x10)),
            ("mcause", u(0x8000_000B)),
            ("handler", u(0x20)),
        ],
    ));
    assert_eq!(i.kind, "interrupt");
    assert_eq!(
        fields(&i),
        [
            ("pc", present(u(0x10))),
            ("cause", present(u(0x8000_000B))),
            ("to", present(s("M"))),
        ]
    );
}

#[test]
fn records_out_of_shape_are_refused() {
    let err = |r: TraceRecord| project(&mut Rv32Projection::default(), r).unwrap_err();
    assert_eq!(
        err(commit(1, vec![("bogus", u(0))])),
        "rv32.commit: unexpected field bogus"
    );
    assert_eq!(
        err(commit(1, vec![("width", u(4))])),
        "rv32.commit: a width without an address"
    );
    assert_eq!(
        err(commit(1, vec![("csr_value", u(4))])),
        "rv32.commit: no csr"
    );
    assert_eq!(
        err(record("rv32.commit", vec![("rd", s("x"))])),
        "rv32.commit: rd is not a U64"
    );
    assert_eq!(
        err(record("rv32.exception", vec![("pc", u(0))])),
        "rv32.exception: unexpected fields [\"pc\"]"
    );
    assert_eq!(
        err(record("rv32.trap", vec![("pc", u(0))])),
        "rv32.trap: unexpected fields [\"pc\"]"
    );
    assert_eq!(
        err(record("rv32.halt", vec![("instret", u(1))])),
        "rv32.halt: the instruction limit"
    );
    assert_eq!(
        err(record("rv32.other", Vec::new())),
        "rv32.other: unknown CPU record kind"
    );
}

#[test]
fn the_projection_reports_every_field_and_checks_the_profile() {
    let fields: Vec<String> = support().collect();
    assert_eq!(fields.len(), 12 + 5 + 3);
    assert!(fields.contains(&"arch/retire/csr_value".to_owned()));
    assert!(fields.contains(&"arch/interrupt/to".to_owned()));
    let profile = |stop_rule: &str, window: &str| {
        let mut primary = arch_m3();
        primary.window = window.to_owned();
        ComparisonProfile {
            name: "p".to_owned(),
            version: 1,
            primary,
            auxiliary: Vec::new(),
            stop_rule: stop_rule.to_owned(),
            report_window: 3,
        }
    };
    let mut p = Rv32Projection::default();
    assert_eq!(p.begin(&profile(HALT_TRAP, "")), Ok(()));
    assert_eq!(
        p.begin(&profile("shutdown", "")),
        Err("stop rule shutdown".to_owned())
    );
    assert_eq!(
        p.begin(&profile(HALT_TRAP, "elf-entry")),
        Err("window elf-entry".to_owned())
    );
    // The M3 stream compares every field, Absent only where it may not apply.
    let retire = arch_m3().kind("retire").unwrap().clone();
    let absent: Vec<&str> = retire
        .fields
        .iter()
        .filter(|f| f.may_be_absent)
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        absent,
        [
            "priv",
            "rd",
            "rd_value",
            "addr",
            "width",
            "value",
            "paddr",
            "csr",
            "csr_value"
        ]
    );
}

/// A store of `width` bytes to `addr`.
fn store(addr: u64, width: u64) -> TraceRecord {
    commit(
        0,
        vec![
            ("addr", u(addr)),
            ("paddr", u(addr)),
            ("width", u(width)),
            ("value", u(1)),
        ],
    )
}

#[test]
fn write_tohost_ends_the_stream_after_its_second_store() {
    let profile = |stop_rule: &str| ComparisonProfile {
        name: "p".to_owned(),
        version: 1,
        primary: arch_m3(),
        auxiliary: Vec::new(),
        stop_rule: stop_rule.to_owned(),
        report_window: 3,
    };
    const TOHOST: u64 = 0x8000_1000;
    // Without a tohost, only the halting trap ends it.
    let mut p = Rv32Projection::default();
    assert_eq!(p.begin(&profile(WRITE_TOHOST)), Ok(()));
    for r in [store(TOHOST, 4), store(TOHOST + 4, 4)] {
        project(&mut p, r).unwrap();
    }
    assert!(!p.finished());
    let mut p = Rv32Projection::new(Some(TOHOST as u32));
    assert_eq!(p.begin(&profile(WRITE_TOHOST)), Ok(()));
    // A store to tohost + 4 before tohost, a narrow store, and a load do not end it.
    for r in [
        store(TOHOST + 4, 4),
        store(TOHOST, 2),
        commit(5, vec![("addr", u(TOHOST)), ("paddr", u(TOHOST))]),
        store(TOHOST + 4, 4),
    ] {
        assert_eq!(project(&mut p, r).unwrap().len(), 1);
        assert!(!p.finished());
    }
    assert_eq!(project(&mut p, store(TOHOST, 4)).unwrap().len(), 1);
    assert!(!p.finished());
    assert_eq!(project(&mut p, store(TOHOST + 8, 4)).unwrap().len(), 1);
    assert!(!p.finished());
    // The second store of write_tohost is the last boundary; what follows is not read.
    assert_eq!(project(&mut p, store(TOHOST + 4, 4)).unwrap().len(), 1);
    assert!(p.finished());
    assert_eq!(project(&mut p, trap()), Ok(Vec::new()));
    assert_eq!(
        project(&mut p, record("rv32.other", Vec::new())),
        Ok(Vec::new())
    );
    // Under halt-trap, the same stores never end it.
    let mut p = Rv32Projection::new(Some(TOHOST as u32));
    assert_eq!(p.begin(&profile(HALT_TRAP)), Ok(()));
    for r in [store(TOHOST, 4), store(TOHOST + 4, 4)] {
        project(&mut p, r).unwrap();
    }
    assert!(!p.finished());
    // A halting trap before write_tohost ends it too.
    let mut p = Rv32Projection::new(Some(TOHOST as u32));
    p.begin(&profile(WRITE_TOHOST)).unwrap();
    assert_eq!(project(&mut p, trap()).unwrap().len(), 1);
    assert!(p.finished());
}
