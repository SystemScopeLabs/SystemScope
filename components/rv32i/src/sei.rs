//! The `M5` profile's supervisor external interrupt (`docs/m5-design.md` §9).
//!
//! Everything here is pure, and only the `M5` profile ([`Rv32iProfile`](crate::cpu::Rv32iProfile))
//! uses it. [`Sei`] holds what `M5` adds to `M3`: `mideleg.SEI`, `mie.SEIE`, the
//! M-writable `mip.SEIP` bit, and the `sirq` line. Its operations overlay the `M3` rules
//! ([`M3State`]) on the five CSRs it touches and leave every other CSR to them.
//!
//! - `mideleg`: bit 9 writable; every other bit reads 0.
//! - `mie`: `MEIE` and `SEIE`.
//! - `mip`: `MEIP` is the `irq` line, read-only; `SEIP` reads the M-writable bit ORed with
//!   the `sirq` line, and a write sets only the M-writable bit. A read-modify-write
//!   (`csrrs`, `csrrc`) computes from the M-writable bit, not the line, as the privileged
//!   specification says, so the line is never latched into it.
//! - `sie`, `sip`: `SEIE` and `SEIP` while SEI is delegated, read 0 otherwise. `sip.SEIP`
//!   is read-only; `sie.SEIE` writes `mie.SEIE` while delegated.

use crate::csr::{CsrFile, MIE, MIP};
use crate::privilege::{M3State, MIDELEG, Privilege, SIE, SIP, Spp};

/// Bit 9: `SEI` in `mideleg`, `SEIE` in `mie` and `sie`, `SEIP` in `mip` and `sip`.
pub const SEI_BIT: u32 = 1 << 9;

/// The supervisor external interrupt's `mcause` or `scause`.
pub const SEI_CAUSE: u32 = 0x8000_0009;

/// What the `M5` profile adds to `M3`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Sei {
    /// `mideleg.SEI`.
    pub delegated: bool,
    /// `mie.SEIE`.
    pub enabled: bool,
    /// The M-writable `mip.SEIP` bit.
    pub soft: bool,
    /// The `sirq` input level.
    pub line: bool,
}

impl Sei {
    /// The reset state: everything 0, the line low.
    pub const fn new() -> Sei {
        Sei {
            delegated: false,
            enabled: false,
            soft: false,
            line: false,
        }
    }

    /// `SEIP` as read: the M-writable bit ORed with the line.
    pub const fn pending(&self) -> bool {
        self.soft || self.line
    }

    /// Reads `csr` in the `M5` profile; `None` if it is not on the whitelist.
    pub fn read(&self, m3: &M3State, m: &CsrFile, csr: u16) -> Option<u32> {
        let bit = |b: bool| if b { SEI_BIT } else { 0 };
        Some(match csr {
            MIDELEG => bit(self.delegated),
            MIE => m3.read(m, MIE)? | bit(self.enabled),
            MIP => m3.read(m, MIP)? | bit(self.pending()),
            SIE => bit(self.delegated && self.enabled),
            SIP => bit(self.delegated && self.pending()),
            _ => m3.read(m, csr)?,
        })
    }

    /// The old value a read-modify-write of `csr` computes from: [`Sei::read`], except
    /// that `mip.SEIP` is the M-writable bit alone.
    pub fn read_for_write(&self, m3: &M3State, m: &CsrFile, csr: u16) -> Option<u32> {
        match csr {
            MIP => Some(m3.read(m, MIP)? | if self.soft { SEI_BIT } else { 0 }),
            _ => self.read(m3, m, csr),
        }
    }

    /// Writes `value` to `csr` in the `M5` profile. Returns `None`, changing nothing, if
    /// `csr` is not on the whitelist.
    pub fn write(&mut self, m3: &mut M3State, m: &mut CsrFile, csr: u16, value: u32) -> Option<()> {
        let set = value & SEI_BIT != 0;
        match csr {
            MIDELEG => self.delegated = set,
            MIE => {
                m3.write(m, MIE, value)?;
                self.enabled = set;
            }
            MIP => self.soft = set,
            SIE => {
                if self.delegated {
                    self.enabled = set;
                }
            }
            SIP => {}
            _ => m3.write(m, csr, value)?,
        }
        Some(())
    }

    /// Whether SEI is taken at this boundary, MEI aside (§9): pending and enabled, and,
    /// if delegated, the hart below S or in S with `SIE`; if not, below M or in M with
    /// `MIE`. A delegated interrupt is never taken in M.
    pub fn eligible(&self, m3: &M3State, m: &CsrFile) -> bool {
        if !(self.enabled && self.pending()) {
            return false;
        }
        if self.delegated {
            match m3.privilege {
                Privilege::User => true,
                Privilege::Supervisor => m3.sie,
                Privilege::Machine => false,
            }
        } else {
            m3.privilege < Privilege::Machine || m.mie
        }
    }

    /// Takes SEI at a boundary whose next PC is `next_pc`, in S if delegated and in M
    /// otherwise. Returns the new `pc`.
    ///
    /// In S: `sepc` ← `next_pc`, `scause` ← [`SEI_CAUSE`], `stval` ← 0, `SPP` ← `priv`,
    /// `SPIE` ← `SIE`, `SIE` ← 0, `priv` ← S, `pc` ← `stvec` BASE. In M: `MPP` ← `priv`,
    /// `priv` ← M, `mepc` ← `next_pc`, `mcause` ← [`SEI_CAUSE`], `mtval` ← 0, `MPIE` ←
    /// `MIE`, `MIE` ← 0, `pc` ← `mtvec` BASE.
    pub fn take(&self, m3: &mut M3State, m: &mut CsrFile, next_pc: u32) -> u32 {
        if self.delegated {
            m3.spp = match m3.privilege {
                Privilege::User => Spp::User,
                _ => Spp::Supervisor,
            };
            m3.sepc = next_pc;
            m3.scause = SEI_CAUSE;
            m3.stval = 0;
            m3.spie = m3.sie;
            m3.sie = false;
            m3.privilege = Privilege::Supervisor;
            m3.stvec & !0b11
        } else {
            m3.mpp = m3.privilege;
            m3.privilege = Privilege::Machine;
            m.mepc = next_pc;
            m.mcause = SEI_CAUSE;
            m.mtval = 0;
            m.mpie = m.mie;
            m.mie = false;
            m.mtvec
        }
    }
}
