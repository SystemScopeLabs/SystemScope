//! Backend-neutral differential verification.
//!
//! See `docs/m4-design.md` for the specification.

pub mod backend;
pub mod case;
pub mod checkpoint;
pub mod compare;
pub mod error;
pub mod observation;
pub mod profile;
pub mod runtime;
