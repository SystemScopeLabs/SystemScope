//! Engine failures: whether M4 could look, kept apart from what it found
//! (`docs/m4-design.md` §20).

use std::fmt;

/// Why the engine produced no complete result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// A side did not reach the stop rule: a crash, a fault, or its safety budget.
    BackendFailed(String),
    /// A capability-gated call was made on a backend without the capability.
    Unsupported(String),
    /// A checkpoint's recorded boundary counts or prefix digests do not match a replay.
    BoundaryDesync(String),
    /// An invariant of the engine itself failed.
    Internal(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::BackendFailed(why) => write!(f, "backend failed: {why}"),
            EngineError::Unsupported(what) => write!(f, "unsupported: {what}"),
            EngineError::BoundaryDesync(why) => write!(f, "boundary desync: {why}"),
            EngineError::Internal(why) => write!(f, "internal error: {why}"),
        }
    }
}

impl std::error::Error for EngineError {}
