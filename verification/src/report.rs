//! A same-engine case's whole verification result: the comparison, located by checkpoint
//! search, with its state and trace evidence (`docs/m4-design.md` §6.7, §12–§14).

use crate::backend::{Backend, Side, run};
use crate::case::{BackendRunConfig, InputVariant, Scenario};
use crate::checkpoint::Checkpoint;
use crate::compare::{StreamResult, Verdict};
use crate::error::EngineError;
use crate::latent::{SameSide, first_state_divergence};
use crate::locate::{Located, record_index, resume, search};
use crate::profile::{Plan, Role};
use crate::result::{StateEvidence, TraceEvidence, VerificationResult};
use crate::runtime::RuntimeBackend;
use crate::state::diff_state;
use crate::trace::{auxiliary_windows, trace_and_slices};

/// Verifies `inputs[1]` against `inputs[0]` on backends `make` builds, with a checkpoint
/// every `every` primary boundaries. A primary divergence at ordinal `d` gets every
/// evidence a same-engine pair can give: each side's state after its boundary `d`, and
/// the first state and trace divergences up to the later of the two sides' events at
/// `d`, with each side's predecessor slice of its first differing trace record. An
/// engine failure is the result, with no evidence.
pub fn same_engine(
    plan: &Plan,
    scenario: &Scenario,
    make: &dyn Fn() -> RuntimeBackend,
    inputs: [InputVariant; 2],
    config: BackendRunConfig,
    every: u64,
) -> VerificationResult {
    settle(searched(plan, scenario, make, inputs, config, every))
}

/// Verifies `inputs[1]` against `inputs[0]` from `origins`, each side's checkpoint or, for
/// `None`, its initial state, with the evidence [`same_engine`] gives: a reproducer's
/// result, a function of its material alone (§16.2). A checkpoint must cover at most the
/// divergence minus the report window, and an agreement starts from the initial states.
pub fn from_origins(
    plan: &Plan,
    scenario: &Scenario,
    make: &dyn Fn() -> RuntimeBackend,
    inputs: [InputVariant; 2],
    config: BackendRunConfig,
    origins: [Option<&Checkpoint>; 2],
) -> VerificationResult {
    settle((|| {
        let (mut x, mut y) = (make(), make());
        let located = resume(
            plan,
            scenario,
            (side(&mut x, &inputs[0], config), origins[0]),
            (side(&mut y, &inputs[1], config), origins[1]),
        )?;
        let checkpoints = origins.map(|c| c.map_or(&[][..], std::slice::from_ref));
        evidence(plan, scenario, make, &inputs, config, located, checkpoints)
    })())
}

/// The result, or an engine failure as the result with no evidence.
fn settle(result: Result<VerificationResult, EngineError>) -> VerificationResult {
    result.unwrap_or_else(|e| VerificationResult {
        verdict: Err(e),
        window: None,
        state: None,
        trace: TraceEvidence::default(),
        slices: Vec::new(),
    })
}

fn searched(
    plan: &Plan,
    scenario: &Scenario,
    make: &dyn Fn() -> RuntimeBackend,
    inputs: [InputVariant; 2],
    config: BackendRunConfig,
    every: u64,
) -> Result<VerificationResult, EngineError> {
    let roles = [Role::Expected, Role::Actual];
    let mut indexes = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        let mut b = make();
        indexes.push(record_index(
            plan,
            scenario,
            side(&mut b, input, config),
            roles[i],
            every,
        )?);
    }
    let (mut x, mut y) = (make(), make());
    let located = search(
        plan,
        scenario,
        (side(&mut x, &inputs[0], config), &indexes[0]),
        (side(&mut y, &inputs[1], config), &indexes[1]),
    )?;
    let checkpoints = [&indexes[0].checkpoints[..], &indexes[1].checkpoints[..]];
    evidence(plan, scenario, make, &inputs, config, located, checkpoints)
}

/// The state and trace evidence of a located comparison, scanning from `checkpoints`.
fn evidence(
    plan: &Plan,
    scenario: &Scenario,
    make: &dyn Fn() -> RuntimeBackend,
    inputs: &[InputVariant; 2],
    config: BackendRunConfig,
    located: Located,
    checkpoints: [&[Checkpoint]; 2],
) -> Result<VerificationResult, EngineError> {
    let mut result = VerificationResult {
        verdict: Ok(located.verdict.clone()),
        window: located.window.clone(),
        state: None,
        trace: TraceEvidence::default(),
        slices: Vec::new(),
    };
    let Verdict::Compared(c) = &located.verdict else {
        return Ok(result);
    };
    if !c.auxiliary.is_empty() {
        let mut recordings = Vec::new();
        for input in inputs {
            let mut b = make();
            recordings.push(run(plan, scenario, side(&mut b, input, config)));
        }
        result.trace.auxiliary =
            auxiliary_windows(plan, c, &recordings[0].streams, &recordings[1].streams);
    }
    let StreamResult::Diverged {
        stream, ordinal, ..
    } = &c.primary
    else {
        return Ok(result);
    };
    // Each side's state after its own boundary `ordinal`, or where its stream ended.
    let mut states = Vec::new();
    let mut read_at = [0; 2];
    let mut until = 0;
    for i in 0..2 {
        let mut b = make();
        b.start(scenario, &inputs[i], &config, &plan.profile)?;
        for _ in 0..=*ordinal {
            if b.next(stream)?.is_none() {
                break;
            }
        }
        read_at[i] = b.events().expect("started");
        until = until.max(b.event_index(stream, *ordinal).unwrap_or(read_at[i]));
        states.push(b.state()?);
    }
    let same = |i: usize, backend| SameSide {
        backend,
        input: inputs[i].clone(),
        config,
        checkpoints: checkpoints[i],
    };
    let (mut x, mut y) = (make(), make());
    let first =
        first_state_divergence(plan, scenario, same(0, &mut x), same(1, &mut y), &[], until)?;
    result.state = Some(StateEvidence {
        read_at,
        differences: diff_state(&states[0], &states[1]),
        first: Some(first),
    });
    let (mut x, mut y) = (make(), make());
    let (canonical, slices) = trace_and_slices(
        plan,
        scenario,
        same(0, &mut x),
        same(1, &mut y),
        u64::from(plan.profile.report_window),
        until,
    )?;
    result.trace.canonical = Some(canonical);
    result.slices = slices;
    Ok(result)
}

fn side<'a>(
    backend: &'a mut RuntimeBackend,
    input: &InputVariant,
    config: BackendRunConfig,
) -> Side<'a> {
    Side {
        backend,
        input: input.clone(),
        config,
    }
}
