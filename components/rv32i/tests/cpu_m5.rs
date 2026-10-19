//! `Rv32iCpu` in the `M5` profile, driven directly (`docs/m5-design.md` §9): the `sirq`
//! port, `mideleg.SEI`, `mie.SEIE`, `mip.SEIP` with its M-writable bit, `sie` and `sip`
//! masked by delegation, the supervisor external interrupt's eligibility and delivery to
//! S or M, its priority below MEI, and snapshot schema 4. Expected values are spelled out
//! from the design.

mod common;

use std::num::NonZeroU64;

use common::asm::*;
use common::{MockCtx, Traced};
use systemscope_contracts::component::Component;
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::Phase;
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::IrqMsg;
use systemscope_contracts::protocol::mem_v1::{MemMsg, ReadOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;
use systemscope_rv32i::cpu::{
    COMMIT, COMMIT_KIND, FETCH, INTERRUPT_KIND, SIRQ_PORT, SNAPSHOT_SCHEMA_M3, SNAPSHOT_SCHEMA_M5,
};
use systemscope_rv32i::{Reg, Rv32iConfig, Rv32iCpu, Rv32iProfile, Sei};

const ENTRY: u32 = 0x8000_0000;
const CLOCK: ClockDomainId = ClockDomainId(0);
const LIMIT: u64 = 1000;

const SSTATUS: u16 = 0x100;
const SIE: u16 = 0x104;
const STVEC: u16 = 0x105;
const SIP: u16 = 0x144;
const MSTATUS: u16 = 0x300;
const MIDELEG: u16 = 0x303;
const MIE: u16 = 0x304;
const MTVEC: u16 = 0x305;
const MEPC: u16 = 0x341;
const MIP: u16 = 0x344;

const MRET: u32 = 0x3020_0073;

const U: u8 = 0;
const S: u8 = 1;
const M: u8 = 3;

const SIE_BIT: u32 = 1 << 1;
const MIE_BIT: u32 = 1 << 3;
const SPIE_BIT: u32 = 1 << 5;
const MPIE_BIT: u32 = 1 << 7;
const SPP_BIT: u32 = 1 << 8;
const MPP: u32 = 0b11 << 11;

/// Bit 9: `SEI`, `SEIE`, `SEIP`.
const SEI: u32 = 1 << 9;
/// Bit 11: `MEIE`, `MEIP`.
const MEI: u32 = 1 << 11;
const SEI_CAUSE: u32 = 0x8000_0009;
const MEI_CAUSE: u32 = 0x8000_000b;

const MTVEC_BASE: u32 = 0x8000_3000;
const STVEC_BASE: u32 = 0x8000_4000;
const CODE: u32 = 0x8000_1000;

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}
fn csrrw(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(1, rd, rs1, csr)
}
fn csrrs(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(2, rd, rs1, csr)
}
fn csrrc(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(3, rd, rs1, csr)
}
fn csrrsi(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(6, rd, uimm, csr)
}

fn config(profile: Rv32iProfile) -> Rv32iConfig {
    Rv32iConfig {
        clock: CLOCK,
        entry: ENTRY,
        max_instructions: NonZeroU64::new(LIMIT).unwrap(),
        profile,
    }
}

fn start() -> (Rv32iCpu, MockCtx) {
    let mut cpu = Rv32iCpu::new(config(Rv32iProfile::M5)).unwrap();
    let mut ctx = MockCtx::new();
    cpu.init(&mut ctx).unwrap();
    assert_eq!(ctx.take_wake().token, FETCH);
    (cpu, ctx)
}

/// Runs `insn`, which must not touch memory, through its commit; returns its records.
fn run_all(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Vec<Traced> {
    ctx.wake(cpu, FETCH, Phase::Request).unwrap();
    let MemMsg::ReadReq { txn, addr, len: 4 } = ctx.take_sent() else {
        panic!("not a fetch");
    };
    assert_eq!(addr, u64::from(cpu.pc()));
    let data = insn.to_le_bytes().to_vec();
    ctx.respond(
        cpu,
        MemMsg::ReadResp {
            txn,
            outcome: ReadOutcome::Data { data },
        },
    )
    .unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    ctx.wake(cpu, COMMIT, Phase::Commit).unwrap();
    assert_eq!(ctx.take_wake().token, FETCH);
    std::mem::take(&mut ctx.traced)
}

/// Runs `insn`, which must retire with no interrupt after it.
fn retire(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) {
    let records = run_all(cpu, ctx, insn);
    assert_eq!(records.len(), 1, "{insn:#010x}: {records:?}");
    assert_eq!(records[0].0, COMMIT_KIND);
}

fn reg(cpu: &Rv32iCpu, i: u8) -> u32 {
    cpu.registers().read(Reg::new(i).unwrap())
}

fn li(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, rd: u32, value: u32) {
    let lo = ((value & 0xfff) as i32) << 20 >> 20;
    let hi = value.wrapping_sub(lo as u32) >> 12;
    retire(cpu, ctx, lui(rd, hi));
    retire(cpu, ctx, addi(rd, rd, lo));
}

fn csrw(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, csr: u16, value: u32) {
    li(cpu, ctx, 31, value);
    retire(cpu, ctx, csrrw(0, csr, 31));
}

/// Reads `csr` with `csrrs x30, csr, x0`.
fn csrr(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, csr: u16) -> u32 {
    retire(cpu, ctx, csrrs(30, csr, 0));
    reg(cpu, 30)
}

fn view_u(view: &StateView, name: &str) -> u32 {
    match view.get(name) {
        Some(Value::U64(v)) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn read(cpu: &Rv32iCpu, name: &str) -> u32 {
    view_u(&cpu.inspect(), name)
}

fn mode(cpu: &Rv32iCpu) -> u8 {
    read(cpu, "priv") as u8
}

fn u(v: u32) -> Value {
    Value::U64(u64::from(v))
}

fn s(v: &str) -> Value {
    Value::Str(v.to_owned())
}

fn sirq(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, asserted: bool) -> Result<(), SimError> {
    let msg = Message::Irq(IrqMsg::Level { asserted });
    ctx.deliver(cpu, SIRQ_PORT, msg, Phase::Complete)
}

/// From M, enters `target` at [`CODE`] with `MRET`, `mstatus` = `extra` | MPP.
fn enter(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, target: u8, extra: u32) {
    assert_eq!(mode(cpu), M);
    csrw(cpu, ctx, MSTATUS, extra | u32::from(target) << 11);
    csrw(cpu, ctx, MEPC, CODE);
    retire(cpu, ctx, MRET);
    assert_eq!((mode(cpu), cpu.pc()), (target, CODE));
}

/// Both handlers set, SEI enabled, delegated or not.
fn setup(delegated: bool) -> (Rv32iCpu, MockCtx) {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MTVEC, MTVEC_BASE);
    csrw(&mut cpu, &mut ctx, STVEC, STVEC_BASE);
    csrw(&mut cpu, &mut ctx, MIDELEG, if delegated { SEI } else { 0 });
    csrw(&mut cpu, &mut ctx, MIE, SEI);
    (cpu, ctx)
}

/// Runs an `addi` with the line high: returns the records, the first the commit.
fn boundary(cpu: &mut Rv32iCpu, ctx: &mut MockCtx) -> Vec<Traced> {
    let records = run_all(cpu, ctx, addi(1, 1, 1));
    assert_eq!(records[0].0, COMMIT_KIND);
    records
}

fn interrupt(epc: u32, cause: u32, handler: u32, from: &str, to: &str) -> Traced {
    (
        INTERRUPT_KIND,
        vec![
            ("epc", u(epc)),
            ("cause", u(cause)),
            ("handler", u(handler)),
            ("from", s(from)),
            ("to", s(to)),
        ],
    )
}

fn name(privilege: u8) -> &'static str {
    ["U", "S", "", "M"][usize::from(privilege)]
}

// ---------------------------------------------------------------------------------------
// Ports and CSRs.

#[test]
fn m5_has_the_sirq_port_and_schema_4() {
    let names = |p| {
        let cpu = Rv32iCpu::new(config(p)).unwrap();
        cpu.ports().iter().map(|s| s.name).collect::<Vec<_>>()
    };
    assert_eq!(names(Rv32iProfile::M5), ["mem", "irq", "sirq"]);
    assert_eq!(names(Rv32iProfile::M3), ["mem", "irq"]);
    let (cpu, _) = start();
    assert_eq!(cpu.snapshot_schema_version(), SNAPSHOT_SCHEMA_M5);
    assert_eq!(SNAPSHOT_SCHEMA_M5, 4);
    assert_eq!(*cpu.sei(), Sei::new());
}

#[test]
fn mideleg_mie_and_mip_keep_their_sei_bits() {
    let (mut cpu, mut ctx) = start();
    for csr in [MIDELEG, MIE, MIP, SIE, SIP] {
        assert_eq!(csrr(&mut cpu, &mut ctx, csr), 0, "{csr:#x} resets to 0");
    }
    csrw(&mut cpu, &mut ctx, MIDELEG, u32::MAX);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIDELEG), SEI);
    csrw(&mut cpu, &mut ctx, MIE, u32::MAX);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIE), SEI | MEI);
    // MEIP is the irq line, read-only; SEIP is M-writable.
    csrw(&mut cpu, &mut ctx, MIP, u32::MAX);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIP), SEI);
    assert!(cpu.sei().soft && !cpu.sei().line);
    csrw(&mut cpu, &mut ctx, MIP, 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIP), 0);
    assert_eq!(read(&cpu, "mideleg"), SEI);
    assert_eq!(read(&cpu, "mie"), SEI | MEI);
}

#[test]
fn sie_and_sip_follow_the_delegation() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MIE, SEI);
    sirq(&mut cpu, &mut ctx, true).unwrap();
    // Not delegated: both read 0, and a write to sie changes nothing.
    assert_eq!(csrr(&mut cpu, &mut ctx, SIE), 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, SIP), 0);
    csrw(&mut cpu, &mut ctx, SIE, 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIE), SEI);
    // Delegated: they show SEIE and SEIP; sie.SEIE writes mie.SEIE, sip is read-only.
    csrw(&mut cpu, &mut ctx, MIDELEG, SEI);
    assert_eq!(csrr(&mut cpu, &mut ctx, SIE), SEI);
    assert_eq!(csrr(&mut cpu, &mut ctx, SIP), SEI);
    csrw(&mut cpu, &mut ctx, SIE, u32::MAX);
    assert_eq!(csrr(&mut cpu, &mut ctx, SIE), SEI);
    csrw(&mut cpu, &mut ctx, SIE, 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIE), 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, SIE), 0);
    csrw(&mut cpu, &mut ctx, SIP, 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, SIP), SEI);
    sirq(&mut cpu, &mut ctx, false).unwrap();
    assert_eq!(csrr(&mut cpu, &mut ctx, SIP), 0);
    assert_eq!(csrr(&mut cpu, &mut ctx, MIP), 0);
}

#[test]
fn a_read_modify_write_of_mip_never_latches_the_line() {
    let (mut cpu, mut ctx) = start();
    sirq(&mut cpu, &mut ctx, true).unwrap();
    // csrrc with a mask that keeps SEIP: rd sees the line, the write uses the soft bit.
    li(&mut cpu, &mut ctx, 2, MEI);
    retire(&mut cpu, &mut ctx, csrrc(1, MIP, 2));
    assert_eq!(reg(&cpu, 1), SEI);
    assert!(!cpu.sei().soft);
    // csrrs with an unrelated bit, likewise.
    li(&mut cpu, &mut ctx, 2, 1);
    retire(&mut cpu, &mut ctx, csrrs(1, MIP, 2));
    assert_eq!(reg(&cpu, 1), SEI);
    assert!(!cpu.sei().soft);
    sirq(&mut cpu, &mut ctx, false).unwrap();
    assert_eq!(csrr(&mut cpu, &mut ctx, MIP), 0);
    // Setting SEIP through csrrs sets the soft bit; the line does not clear it.
    li(&mut cpu, &mut ctx, 2, SEI);
    retire(&mut cpu, &mut ctx, csrrs(0, MIP, 2));
    assert!(cpu.sei().soft);
    sirq(&mut cpu, &mut ctx, true).unwrap();
    sirq(&mut cpu, &mut ctx, false).unwrap();
    assert_eq!(csrr(&mut cpu, &mut ctx, MIP), SEI);
}

#[test]
fn a_read_modify_write_keeps_every_other_bit() {
    let (mut cpu, mut ctx) = start();
    // The soft bit survives a csrrs of an unrelated bit, line low.
    li(&mut cpu, &mut ctx, 2, SEI);
    retire(&mut cpu, &mut ctx, csrrs(0, MIP, 2));
    li(&mut cpu, &mut ctx, 2, 1);
    retire(&mut cpu, &mut ctx, csrrs(0, MIP, 2));
    assert!(cpu.sei().soft);
    // csrrs of mie.SEIE keeps MEIE, and the reverse.
    csrw(&mut cpu, &mut ctx, MIE, MEI);
    li(&mut cpu, &mut ctx, 2, SEI);
    retire(&mut cpu, &mut ctx, csrrs(0, MIE, 2));
    assert_eq!(csrr(&mut cpu, &mut ctx, MIE), MEI | SEI);
    li(&mut cpu, &mut ctx, 2, MEI);
    retire(&mut cpu, &mut ctx, csrrc(0, MIE, 2));
    assert_eq!(csrr(&mut cpu, &mut ctx, MIE), SEI);
}

#[test]
fn a_sirq_level_outside_complete_faults_and_changes_nothing() {
    let (mut cpu, mut ctx) = start();
    for phase in [Phase::Request, Phase::Commit] {
        let msg = Message::Irq(IrqMsg::Level { asserted: true });
        assert!(matches!(
            ctx.deliver(&mut cpu, SIRQ_PORT, msg, phase),
            Err(SimError::ComponentFault(_))
        ));
        assert!(!cpu.sei().line);
    }
    sirq(&mut cpu, &mut ctx, true).unwrap();
    assert!(cpu.sei().line);
    assert!(ctx.sent.is_empty() && ctx.woke.is_empty() && ctx.traced.is_empty());
}

#[test]
fn an_m3_cpu_has_no_sirq_port() {
    let mut cpu = Rv32iCpu::new(config(Rv32iProfile::M3)).unwrap();
    let mut ctx = MockCtx::new();
    cpu.init(&mut ctx).unwrap();
    let msg = Message::Irq(IrqMsg::Level { asserted: true });
    assert!(
        ctx.deliver(&mut cpu, SIRQ_PORT, msg, Phase::Complete)
            .is_err()
    );
}

// ---------------------------------------------------------------------------------------
// Eligibility and delivery.

#[test]
fn a_non_delegated_sei_is_taken_in_m_with_mie_and_below_m_always() {
    for (privilege, extra, taken) in [
        (M, 0, false),
        (M, MPIE_BIT, true),
        (S, 0, true),
        (U, 0, true),
    ] {
        let (mut cpu, mut ctx) = setup(false);
        enter(&mut cpu, &mut ctx, privilege, extra);
        sirq(&mut cpu, &mut ctx, true).unwrap();
        let mstatus = read(&cpu, "mstatus");
        let records = boundary(&mut cpu, &mut ctx);
        let next = CODE + 4;
        if !taken {
            assert_eq!(records.len(), 1, "{privilege}: {records:?}");
            assert_eq!((mode(&cpu), cpu.pc()), (privilege, next));
            continue;
        }
        let want = interrupt(next, SEI_CAUSE, MTVEC_BASE, name(privilege), "M");
        assert_eq!(records[1..], [want], "{privilege}");
        assert_eq!((mode(&cpu), cpu.pc()), (M, MTVEC_BASE));
        assert_eq!(
            [
                read(&cpu, "mepc"),
                read(&cpu, "mcause"),
                read(&cpu, "mtval")
            ],
            [next, SEI_CAUSE, 0]
        );
        let after = read(&cpu, "mstatus");
        assert_eq!(after & MIE_BIT, 0, "MIE <- 0");
        assert_eq!(after & MPIE_BIT != 0, mstatus & MIE_BIT != 0, "MPIE <- MIE");
        assert_eq!((after & MPP) >> 11, u32::from(privilege), "MPP <- from");
        assert_eq!(read(&cpu, "scause"), 0);
    }
}

#[test]
fn a_delegated_sei_is_taken_below_s_or_in_s_with_sie_never_in_m() {
    for (privilege, extra, taken) in [
        (M, MPIE_BIT, false),
        (S, 0, false),
        (S, SIE_BIT, true),
        (U, 0, true),
        (U, MPIE_BIT, true),
    ] {
        let (mut cpu, mut ctx) = setup(true);
        enter(&mut cpu, &mut ctx, privilege, extra);
        sirq(&mut cpu, &mut ctx, true).unwrap();
        let mstatus = read(&cpu, "mstatus");
        let machine = ["mepc", "mcause", "mtval"].map(|n| read(&cpu, n));
        let records = boundary(&mut cpu, &mut ctx);
        let next = CODE + 4;
        if !taken {
            assert_eq!(records.len(), 1, "{privilege} {extra:#x}: {records:?}");
            assert_eq!((mode(&cpu), cpu.pc()), (privilege, next));
            continue;
        }
        let want = interrupt(next, SEI_CAUSE, STVEC_BASE, name(privilege), "S");
        assert_eq!(records[1..], [want], "{privilege}");
        assert_eq!((mode(&cpu), cpu.pc()), (S, STVEC_BASE));
        assert_eq!(
            [
                read(&cpu, "sepc"),
                read(&cpu, "scause"),
                read(&cpu, "stval")
            ],
            [next, SEI_CAUSE, 0]
        );
        let after = read(&cpu, "mstatus");
        assert_eq!(after & SIE_BIT, 0, "SIE <- 0");
        assert_eq!(after & SPIE_BIT != 0, mstatus & SIE_BIT != 0, "SPIE <- SIE");
        assert_eq!(after & SPP_BIT != 0, privilege == S, "SPP <- from");
        let mask = SIE_BIT | SPIE_BIT | SPP_BIT;
        assert_eq!(after & !mask, mstatus & !mask, "only the S bits change");
        assert_eq!(["mepc", "mcause", "mtval"].map(|n| read(&cpu, n)), machine);
    }
}

#[test]
fn nothing_is_taken_unless_pending_and_enabled() {
    for (enabled, pending) in [(false, true), (true, false)] {
        let (mut cpu, mut ctx) = setup(false);
        if !enabled {
            csrw(&mut cpu, &mut ctx, MIE, 0);
        }
        enter(&mut cpu, &mut ctx, U, 0);
        sirq(&mut cpu, &mut ctx, pending).unwrap();
        assert_eq!(boundary(&mut cpu, &mut ctx).len(), 1);
        assert_eq!(mode(&cpu), U);
    }
}

#[test]
fn the_soft_bit_alone_raises_the_interrupt() {
    let (mut cpu, mut ctx) = setup(true);
    li(&mut cpu, &mut ctx, 2, SEI);
    // Set in M, delegated: not taken in M, taken at the MRET's boundary, in U.
    retire(&mut cpu, &mut ctx, csrrs(0, MIP, 2));
    csrw(&mut cpu, &mut ctx, MSTATUS, 0);
    csrw(&mut cpu, &mut ctx, MEPC, CODE);
    let records = run_all(&mut cpu, &mut ctx, MRET);
    assert_eq!(records[0].0, COMMIT_KIND);
    assert_eq!(
        records[1..],
        [interrupt(CODE, SEI_CAUSE, STVEC_BASE, "U", "S")]
    );
}

#[test]
fn setting_sie_in_s_takes_the_pending_interrupt_at_its_own_boundary() {
    let (mut cpu, mut ctx) = setup(true);
    enter(&mut cpu, &mut ctx, S, 0);
    sirq(&mut cpu, &mut ctx, true).unwrap();
    assert_eq!(boundary(&mut cpu, &mut ctx).len(), 1);
    let pc = cpu.pc();
    let records = run_all(&mut cpu, &mut ctx, csrrsi(0, SSTATUS, SIE_BIT));
    assert_eq!(records[0].0, COMMIT_KIND);
    assert_eq!(
        records[1..],
        [interrupt(pc + 4, SEI_CAUSE, STVEC_BASE, "S", "S")]
    );
    assert_ne!(read(&cpu, "mstatus") & SPP_BIT, 0);
}

#[test]
fn mei_takes_priority_over_sei() {
    for delegated in [false, true] {
        let (mut cpu, mut ctx) = setup(delegated);
        csrw(&mut cpu, &mut ctx, MIE, SEI | MEI);
        enter(&mut cpu, &mut ctx, U, 0);
        sirq(&mut cpu, &mut ctx, true).unwrap();
        ctx.level(&mut cpu, true).unwrap();
        let records = boundary(&mut cpu, &mut ctx);
        assert_eq!(
            records[1..],
            [interrupt(CODE + 4, MEI_CAUSE, MTVEC_BASE, "U", "M")],
            "delegated: {delegated}"
        );
        assert_eq!(read(&cpu, "scause"), 0);
    }
}

#[test]
fn the_handler_ends_the_interrupt_by_clearing_seie_and_sret() {
    const SRET: u32 = 0x1020_0073;
    let (mut cpu, mut ctx) = setup(true);
    enter(&mut cpu, &mut ctx, U, 0);
    sirq(&mut cpu, &mut ctx, true).unwrap();
    assert_eq!(boundary(&mut cpu, &mut ctx).len(), 2);
    // The S handler: clear sie.SEIE, sepc is already the next PC, SRET back to U.
    li(&mut cpu, &mut ctx, 2, SEI);
    retire(&mut cpu, &mut ctx, csrrc(0, SIE, 2));
    retire(&mut cpu, &mut ctx, SRET);
    assert_eq!((mode(&cpu), cpu.pc()), (U, CODE + 4));
    assert_eq!(boundary(&mut cpu, &mut ctx).len(), 1);
}

// ---------------------------------------------------------------------------------------
// Snapshots.

fn snapshot_of(cpu: &Rv32iCpu) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    cpu.snapshot(&mut w);
    w.into_bytes()
}

fn restore_as(profile: Rv32iProfile, schema: u32, bytes: &[u8]) -> Result<Rv32iCpu, RestoreError> {
    let mut cpu = Rv32iCpu::new(config(profile)).unwrap();
    let mut r = SnapshotReader::new(bytes);
    cpu.restore(&mut r, schema)?;
    r.finish().map_err(RestoreError::Decode)?;
    Ok(cpu)
}

#[test]
fn schema_4_is_schema_3_and_the_four_sei_flags() {
    let (m5, _) = start();
    let m3 = Rv32iCpu::new(config(Rv32iProfile::M3)).unwrap();
    let (b5, b3) = (snapshot_of(&m5), snapshot_of(&m3));
    assert_eq!(b5.len(), b3.len() + 4);
    assert_eq!(&b5[..b3.len()], &b3[..]);
    assert_eq!(&b5[b3.len()..], &[0, 0, 0, 0]);

    let (mut cpu, mut ctx) = setup(true);
    csrw(&mut cpu, &mut ctx, MIP, SEI);
    sirq(&mut cpu, &mut ctx, true).unwrap();
    let bytes = snapshot_of(&cpu);
    assert_eq!(&bytes[bytes.len() - 4..], &[1, 1, 1, 1]);
    let restored = restore_as(Rv32iProfile::M5, SNAPSHOT_SCHEMA_M5, &bytes).unwrap();
    assert_eq!(*restored.sei(), *cpu.sei());
    assert_eq!(snapshot_of(&restored), bytes);
    assert_eq!(restored.inspect(), cpu.inspect());
}

#[test]
fn each_flag_restores_on_its_own() {
    let (cpu, _) = start();
    let base = snapshot_of(&cpu);
    let n = base.len();
    for k in 0..4 {
        let mut bytes = base.clone();
        bytes[n - 4 + k] = 1;
        let restored = restore_as(Rv32iProfile::M5, SNAPSHOT_SCHEMA_M5, &bytes).unwrap();
        let s = restored.sei();
        assert_eq!(
            [s.delegated, s.enabled, s.soft, s.line],
            std::array::from_fn(|i| i == k)
        );
    }
}

#[test]
fn schema_4_restores_only_into_m5() {
    let (m5, _) = start();
    let b5 = snapshot_of(&m5);
    let b3 = snapshot_of(&Rv32iCpu::new(config(Rv32iProfile::M3)).unwrap());
    for (profile, schema, bytes) in [
        (Rv32iProfile::M5, SNAPSHOT_SCHEMA_M3, &b5),
        (Rv32iProfile::M3, SNAPSHOT_SCHEMA_M5, &b5),
    ] {
        assert!(
            matches!(
                restore_as(profile, schema, bytes),
                Err(RestoreError::InvalidState(_))
            ),
            "{profile:?} schema {schema}"
        );
    }
    // Schema 3 bytes are too short for schema 4; schema 4 too long for schema 3.
    assert!(restore_as(Rv32iProfile::M5, SNAPSHOT_SCHEMA_M5, &b3).is_err());
    assert!(restore_as(Rv32iProfile::M3, SNAPSHOT_SCHEMA_M3, &b5).is_err());
}

#[test]
fn a_flag_that_is_not_0_or_1_is_rejected_and_changes_nothing() {
    let (mut cpu, mut ctx) = setup(true);
    let before = snapshot_of(&cpu);
    for k in 1..=4 {
        let mut bytes = before.clone();
        let n = bytes.len();
        bytes[n - k] = 2;
        let mut r = SnapshotReader::new(&bytes);
        assert!(matches!(
            cpu.restore(&mut r, SNAPSHOT_SCHEMA_M5),
            Err(RestoreError::InvalidState(_))
        ));
        assert_eq!(snapshot_of(&cpu), before);
    }
    retire(&mut cpu, &mut ctx, addi(1, 0, 1));
}
