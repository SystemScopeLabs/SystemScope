//! Deterministic discrete-event simulation runtime.
//!
//! See `docs/m0-design.md` for the specification.

pub mod choice;
pub mod export;
pub mod jitter;
pub mod rng;
pub mod runtime;
pub mod scheduler;
pub mod script;
pub mod snapshot;
pub mod ties;
pub mod topology;
pub mod trace;
