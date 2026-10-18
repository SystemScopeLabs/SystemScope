//! The differential backend interface (`docs/m4-design.md` §7, §9).

use std::collections::BTreeSet;

use crate::case::{BackendId, BackendRunConfig, InputVariant, Scenario};
use crate::compare::{Recorded, Verdict, compare};
use crate::error::EngineError;
use crate::observation::Observation;
use crate::profile::{ComparisonProfile, Plan, Support, plan};

/// What a backend can do (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// L2 `arch` boundaries.
    ArchStream,
    /// L2 `effect` boundaries.
    EffectStream,
    /// L2 `os` boundaries.
    OsStream,
    /// Advance to the next boundary on demand.
    StepBoundary,
    /// The same scenario and input variant give the same streams.
    DeterministicReplay,
}

/// A backend's declared capabilities.
pub type Capabilities = BTreeSet<Capability>;

/// One side of a comparison.
pub trait Backend {
    /// The backend's identity.
    fn describe(&self) -> BackendId;
    /// What it can do.
    fn capabilities(&self) -> Capabilities;
    /// The streams and fields it can report.
    fn support(&self) -> Support;
    /// Builds this side at its initial state, for `profile`, which is already narrowed to
    /// what both sides support.
    fn start(
        &mut self,
        scenario: &Scenario,
        input: &InputVariant,
        config: &BackendRunConfig,
        profile: &ComparisonProfile,
    ) -> Result<(), EngineError>;
    /// The next boundary of `stream`, or `None` once the stop rule is reached. Requires
    /// [`Capability::StepBoundary`].
    fn next(&mut self, stream: &str) -> Result<Option<Observation>, EngineError>;
    /// The whole stream up to the stop rule.
    fn record(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        if !self.capabilities().contains(&Capability::StepBoundary) {
            return Err(EngineError::Unsupported("record"));
        }
        let mut out = Vec::new();
        while let Some(o) = self.next(stream)? {
            out.push(o);
        }
        Ok(out)
    }
}

/// One side of a case: its backend, input variant, and run configuration.
pub struct Side<'a> {
    /// The backend.
    pub backend: &'a mut dyn Backend,
    /// Its input variant.
    pub input: InputVariant,
    /// Its run configuration.
    pub config: BackendRunConfig,
}

/// Plans `profile` for both sides, runs each to the stop rule, records every compared
/// stream, and compares them: the naive end-to-end verification.
pub fn verify(
    profile: &ComparisonProfile,
    scenario: &Scenario,
    expected: Side<'_>,
    actual: Side<'_>,
) -> Result<Verdict, EngineError> {
    let plan = match plan(
        profile,
        &expected.backend.support(),
        &actual.backend.support(),
    ) {
        Ok(p) => p,
        Err(why) => return Ok(Verdict::Incomparable(why)),
    };
    let e = run(&plan, scenario, expected)?;
    let a = run(&plan, scenario, actual)?;
    compare(&plan, &e, &a)
}

/// Starts one side and records every stream of the plan.
pub fn run(plan: &Plan, scenario: &Scenario, side: Side<'_>) -> Result<Recorded, EngineError> {
    side.backend
        .start(scenario, &side.input, &side.config, &plan.profile)?;
    plan.profile
        .streams()
        .map(|s| side.backend.record(&s.stream))
        .collect()
}
