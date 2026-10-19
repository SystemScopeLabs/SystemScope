//! The directed M5.2 programs for the Spike differential: the `M5` profile's
//! supervisor external interrupt (`docs/m5-design.md` §9, §21), its CSRs, and its
//! delivery to S or M.
//!
//! Like [`privgen`](crate::privgen), each program is machine code and a minimal ELF
//! written directly. Spike models no interrupt controller, so every program makes SEI
//! pending with the M-writable `mip.SEIP` bit, which both sides take at the same
//! instruction. Every program has the same frame:
//!
//! - it starts in M, points `mtvec` at the pass sequence, and runs its body;
//! - the S handler reads `scause`, `sepc`, `stval`, and `sstatus` into `t3`–`t6`, clears
//!   `sie.SEIE` (it cannot clear the pending bit from S), and returns with `SRET` to the
//!   interrupted instruction;
//! - the M interrupt handler, which a body that takes SEI in M points `mtvec` at, reads
//!   `mcause`, `mepc`, and `mstatus` into `a0`–`a2`, points `mtvec` back at the pass
//!   sequence, and ends the program with an `ECALL`;
//! - the body ends with an exception taken in M, as in `privgen`.
//!
//! The bodies stay clear of the recorded divergences: `mip` is never read, because the
//! pinned Spike's machine timer is always pending, and is written only with `csrrs` or
//! `csrrc` of `SEIP` alone; `mideleg` is written with `SEIP` or 0, since Spike also
//! delegates `SSIP` and `STIP`; and `sip` is never written.

use systemscope_rv32i::csr::{MCAUSE, MEPC, MIE, MIP, MSTATUS, MTVEC};
use systemscope_rv32i::privilege::{MIDELEG, SCAUSE, SEPC, SIE, SIP, SRET, SSTATUS, STVAL, STVEC};

use crate::privgen::{Asm, ECALL};
use crate::progen::{ExpectedTrap, Program, WINDOW_SIZE, elf, pass_sequence};

const T0: u8 = 5;
const T2: u8 = 7;
const A0: u8 = 10;
const A1: u8 = 11;
const A2: u8 = 12;
const A3: u8 = 13;
const A4: u8 = 14;
const A5: u8 = 15;
const T3: u8 = 28;
const T4: u8 = 29;
const T5: u8 = 30;
const T6: u8 = 31;

const CSRRS: u32 = 2;
const CSRRC: u32 = 3;
const CSRRSI: u32 = 6;

/// Bit 9: `SEI`, `SEIE`, `SEIP`.
pub const SEI: u32 = 1 << 9;

/// `mstatus.MPP` for each mode.
const MPP_U: u32 = 0;
const MPP_S: u32 = 1 << 11;

const FROM_M: &str = "EnvironmentCallFromM";
const FROM_S: &str = "EnvironmentCallFromS";
const FROM_U: &str = "EnvironmentCallFromU";

/// Where the handlers are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Labels {
    s_handler: u32,
    m_interrupt: u32,
    m_handler: u32,
}

impl Labels {
    /// The `ECALL` that ends the M interrupt handler.
    fn m_interrupt_end(&self) -> ExpectedTrap {
        ExpectedTrap {
            pc: self.m_interrupt + 4 * (3 + 2 + 1),
            insn: ECALL,
            cause: FROM_M,
            tval: 0,
        }
    }
}

/// `li t0, SEI`, then `csrrs x0, mip, t0` (`set`) or `csrrc x0, mip, t0`.
fn seip(a: &mut Asm, set: bool) -> &mut Asm {
    let funct3 = if set { CSRRS } else { CSRRC };
    a.li(T0, SEI).csr(funct3, 0, T0, MIP)
}

/// `stvec` = the S handler, `mideleg` = `SEI` if `delegated`, `mie` = `SEIE`.
fn enable<'a>(a: &'a mut Asm, labels: &Labels, delegated: bool) -> &'a mut Asm {
    a.li(T0, labels.s_handler)
        .write(STVEC, T0)
        .li(T0, if delegated { SEI } else { 0 })
        .write(MIDELEG, T0)
        .li(T0, SEI)
        .write(MIE, T0)
}

/// `mtvec` = the M interrupt handler.
fn to_m_interrupt<'a>(a: &'a mut Asm, labels: &Labels) -> &'a mut Asm {
    a.li(T0, labels.m_interrupt).write(MTVEC, T0)
}

/// The program `m5-<name>`: the frame around `body`, which returns the exception taken in
/// M that ends it.
fn build(name: &str, body: impl Fn(&mut Asm, &Labels) -> ExpectedTrap) -> (Program, ExpectedTrap) {
    let assemble = |labels: &Labels| {
        let mut a = Asm::default();
        a.li(T0, labels.m_handler).write(MTVEC, T0);
        let expected = body(&mut a, labels);
        let s_handler = a.pc();
        a.read(T3, SCAUSE)
            .read(T4, SEPC)
            .read(T5, STVAL)
            .read(T6, SSTATUS)
            .li(T2, SEI)
            .csr(CSRRC, 0, T2, SIE)
            .push(SRET);
        let m_interrupt = a.pc();
        a.read(A0, MCAUSE)
            .read(A1, MEPC)
            .read(A2, MSTATUS)
            .li(T2, labels.m_handler)
            .write(MTVEC, T2)
            .push(ECALL);
        let m_handler = a.pc();
        a.0.extend(pass_sequence(m_handler));
        let labels = Labels {
            s_handler,
            m_interrupt,
            m_handler,
        };
        (a.0, expected, labels)
    };
    let (_, _, labels) = assemble(&Labels {
        s_handler: 0,
        m_interrupt: 0,
        m_handler: 0,
    });
    let (code, expected, again) = assemble(&labels);
    assert_eq!(again, labels, "the layout does not depend on the labels");
    let program = Program {
        name: format!("m5-{name}"),
        elf: elf(&code, &[0; WINDOW_SIZE as usize]),
        code,
    };
    (program, expected)
}

/// The CSRs in M: `mideleg` keeps `SEI`, `mie` keeps `SEIE` and `MEIE`, `sie` and `sip`
/// show SEI only while it is delegated, `sie.SEIE` writes `mie.SEIE`, and the M-writable
/// `mip.SEIP` shows in `sip`. A delegated SEI is never taken in M.
fn csrs() -> (Program, ExpectedTrap) {
    build("csrs", |a, labels| {
        a.read(A0, MIDELEG)
            .read(A1, MIE)
            .read(A2, SIE)
            .read(A3, SIP);
        a.li(T0, labels.s_handler).write(STVEC, T0);
        a.store(MIDELEG, SEI).read(A1, SIE).read(A2, SIP);
        a.store(MIE, SEI | 1 << 11).store(MIE, SEI).read(A1, SIE);
        a.store(SIE, 0).read(A2, MIE).store(SIE, SEI).read(A2, MIE);
        a.csr(CSRRSI, 0, 8, MSTATUS);
        seip(a, true).read(A3, SIP).read(A4, SIE).addi(A5, 0, 1);
        seip(a, false).read(A3, SIP);
        a.store(MIDELEG, 0)
            .read(A1, SIE)
            .read(A2, SIP)
            .read(A3, MIE);
        a.ending(ECALL, FROM_M)
    })
}

/// A non-delegated SEI in M waits for `MIE`, and is taken at the boundary of the write
/// that sets it.
fn m_mie() -> (Program, ExpectedTrap) {
    build("m-mie", |a, labels| {
        enable(a, labels, false);
        to_m_interrupt(a, labels);
        seip(a, true).addi(A3, 0, 1);
        a.csr(CSRRSI, 0, 8, MSTATUS).addi(A4, 0, 2);
        labels.m_interrupt_end()
    })
}

/// A non-delegated SEI is taken below M whatever `MIE` says: at the `MRET` into `mpp`.
fn below_m(name: &str, mpp: u32) -> (Program, ExpectedTrap) {
    build(name, |a, labels| {
        enable(a, labels, false);
        to_m_interrupt(a, labels);
        seip(a, true).addi(A3, 0, 1);
        a.mret_with(mpp).addi(A4, 0, 2);
        labels.m_interrupt_end()
    })
}

/// A delegated SEI is not taken in M, even with `MIE`.
fn deleg_m() -> (Program, ExpectedTrap) {
    build("deleg-m", |a, labels| {
        enable(a, labels, true);
        a.csr(CSRRSI, 0, 8, MSTATUS);
        seip(a, true).addi(A3, 0, 1).read(A4, SIP);
        seip(a, false);
        a.ending(ECALL, FROM_M)
    })
}

/// A delegated SEI in S waits for `SIE` and is taken at the boundary of the `csrsi` that
/// sets it; the handler returns to S with SEI disabled.
fn deleg_s() -> (Program, ExpectedTrap) {
    build("deleg-s", |a, labels| {
        enable(a, labels, true);
        seip(a, true);
        a.mret_with(MPP_S).addi(A3, 0, 1).read(A4, SSTATUS);
        a.csr(CSRRSI, 0, 2, SSTATUS)
            .addi(A5, 0, 2)
            .read(A4, SSTATUS);
        a.ending(ECALL, FROM_S)
    })
}

/// A delegated SEI pending in S with `SIE` = 0 is taken at the first U boundary, after
/// `SRET`; the handler returns to U.
fn deleg_u() -> (Program, ExpectedTrap) {
    build("deleg-u", |a, labels| {
        enable(a, labels, true);
        seip(a, true);
        a.mret_with(MPP_S).addi(A3, 0, 1);
        a.sret_to_next().addi(A4, 0, 2);
        a.ending(ECALL, FROM_U)
    })
}

/// Every directed M5.2 program, with the exception taken in M that ends it.
pub fn programs() -> Vec<(Program, ExpectedTrap)> {
    vec![
        csrs(),
        m_mie(),
        below_m("s-nondeleg", MPP_S),
        below_m("u-nondeleg", MPP_U),
        deleg_m(),
        deleg_s(),
        deleg_u(),
    ]
}

#[cfg(test)]
mod tests {
    use systemscope_contracts::trace::Value;
    use systemscope_rv32i::csr::decode_privileged;
    use systemscope_rv32i::{PrivInstr, Rv32iProfile};

    use super::*;
    use crate::manifest::load;
    use crate::runner::{self, CPU, End, Finished, Start};

    fn run(program: &Program) -> Finished {
        let image = load(&program.name, &program.elf).unwrap();
        runner::execute(
            runner::platform_with_profile(&image, false, runner::SEED, Rv32iProfile::M5),
            Start::Init { traced: true },
            Vec::new(),
        )
    }

    fn reg(finished: &Finished, name: &str) -> u32 {
        match finished.views[CPU.0 as usize].get(name) {
            Some(Value::U64(v)) => *v as u32,
            other => panic!("{name}: {other:?}"),
        }
    }

    fn find(name: &str) -> Program {
        programs()
            .into_iter()
            .find(|(p, _)| p.name == name)
            .unwrap()
            .0
    }

    /// The `(cause, to)` of every `rv32.interrupt` record of a run.
    fn interrupts(finished: &Finished) -> Vec<(u64, String)> {
        let trace = finished.trace.as_ref().unwrap();
        trace
            .records
            .iter()
            .filter(|r| r.kind == "rv32.interrupt")
            .map(|r| {
                let get = |n: &str| r.fields.iter().find(|f| f.0 == n).unwrap().1.clone();
                match (get("cause"), get("to")) {
                    (Value::U64(c), Value::Str(t)) => (c, t),
                    other => panic!("{other:?}"),
                }
            })
            .collect()
    }

    #[test]
    fn every_program_ends_with_its_exception_taken_in_m() {
        let programs = programs();
        assert_eq!(programs.len(), 7);
        for (p, expected) in &programs {
            let finished = run(p);
            assert_eq!(
                finished.outcome.end,
                End::Trap {
                    cause: expected.cause.to_owned(),
                    pc: expected.pc,
                    tval: expected.tval,
                },
                "{}",
                p.name
            );
        }
    }

    #[test]
    fn each_program_takes_the_interrupts_its_design_says() {
        let s = |n| (0x8000_0009, n);
        for (name, want) in [
            ("m5-csrs", vec![]),
            ("m5-m-mie", vec![s("M".to_owned())]),
            ("m5-s-nondeleg", vec![s("M".to_owned())]),
            ("m5-u-nondeleg", vec![s("M".to_owned())]),
            ("m5-deleg-m", vec![]),
            ("m5-deleg-s", vec![s("S".to_owned())]),
            ("m5-deleg-u", vec![s("S".to_owned())]),
        ] {
            assert_eq!(interrupts(&run(&find(name))), want, "{name}");
        }
    }

    #[test]
    fn the_handlers_see_the_interrupt() {
        // In M: mcause, and MPP the interrupted mode.
        for (name, mpp) in [("m5-m-mie", 3), ("m5-s-nondeleg", 1), ("m5-u-nondeleg", 0)] {
            let f = run(&find(name));
            assert_eq!(reg(&f, "x10"), 0x8000_0009, "{name}");
            assert_eq!((reg(&f, "x12") >> 11) & 3, mpp, "{name}");
            // The interrupted code's next instruction never ran.
            assert_eq!(reg(&f, "x14"), 0, "{name}");
        }
        // In S: scause, stval 0, SPP the interrupted mode; the code after it ran.
        for (name, spp) in [("m5-deleg-s", 1 << 8), ("m5-deleg-u", 0)] {
            let f = run(&find(name));
            assert_eq!((reg(&f, "x28"), reg(&f, "x30")), (0x8000_0009, 0), "{name}");
            assert_eq!(reg(&f, "x31") & 1 << 8, spp, "{name}");
        }
        let f = run(&find("m5-deleg-s"));
        assert_eq!((reg(&f, "x13"), reg(&f, "x15")), (1, 2));
        let f = run(&find("m5-deleg-u"));
        assert_eq!((reg(&f, "x13"), reg(&f, "x14")), (1, 2));
        let f = run(&find("m5-csrs"));
        assert_eq!(reg(&f, "x15"), 1);
    }

    /// The bodies avoid the recorded divergences: no read of `mip`, `mip` written only by
    /// `csrrs` or `csrrc` without a destination, and no write of `sip`.
    #[test]
    fn no_program_reaches_a_recorded_divergence() {
        for (p, _) in programs() {
            for &word in &p.code {
                let Some(instr @ PrivInstr::Csr { csr, .. }) = decode_privileged(word) else {
                    continue;
                };
                let rd = (word >> 7) & 0x1f;
                let funct3 = (word >> 12) & 7;
                if csr == MIP {
                    assert!(rd == 0 && matches!(funct3, CSRRS | CSRRC), "{}", p.name);
                }
                if instr.writes() {
                    assert_ne!(csr, SIP, "{}", p.name);
                }
            }
        }
    }
}
