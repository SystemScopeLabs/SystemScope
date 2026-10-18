//! Backend-neutral differential verification.
//!
//! See `docs/m4-design.md` for the specification.

pub mod backend;
pub mod case;
pub mod checkpoint;
pub mod compare;
pub mod error;
pub mod latent;
pub mod locate;
pub mod observation;
pub mod profile;
pub mod report;
pub mod result;
pub mod runtime;
pub mod state;
pub mod trace;
