//! M4 verification on `m3-reference` (`docs/m4-design.md`): the M4 backend and
//! comparison profile of the M3 scenario ([`verify`]), its reproducer bundles
//! ([`repro`]), the expected streams of the unmodified run ([`expected`]), the register
//! faults ([`faults`]), and the source-mutant targets and summary ([`mutants`]). The
//! workload and its fixture are M3's ([`crate::m3`]).

pub mod expected;
pub mod faults;
pub mod mutants;
pub mod repro;
pub mod verify;
