//! A case's whole verification result: the comparison, located by checkpoint search or
//! from given origins, with the state and trace evidence both sides can give
//! (`docs/m4-design.md` §6.7, §12–§14).

use crate::backend::{Backend, Capability, Side, run};
use crate::case::{BackendRunConfig, InputVariant, Scenario};
use crate::checkpoint::Checkpoint;
use crate::compare::{StreamResult, Verdict};
use crate::error::EngineError;
use crate::latent::{SameSide, first_state_divergence};
use crate::locate::{Index, Located, locate, record_index, resume};
use crate::profile::{Plan, Role};
use crate::result::{StateEvidence, TraceEvidence, VerificationResult};
use crate::runtime::RuntimeBackend;
use crate::state::diff_state;
use crate::trace::{auxiliary_windows, trace_and_slices};

/// How one side's backend is built.
#[derive(Clone, Copy)]
pub enum Make<'a> {
    /// A SystemScope runtime, which gives every evidence.
    Runtime(&'a dyn Fn() -> RuntimeBackend),
    /// Any other backend, which gives the comparison only.
    Other(&'a dyn Fn() -> Box<dyn Backend>),
}

impl Make<'_> {
    /// A new backend.
    pub fn build(&self) -> Box<dyn Backend> {
        match self {
            Make::Runtime(make) => Box::new(make()),
            Make::Other(make) => make(),
        }
    }
}

/// One side of a case: how its backend is built, and its canonical replay configuration
/// (§6.6), in that backend's own unit.
#[derive(Clone, Copy)]
pub struct Builder<'a> {
    /// Builds the backend.
    pub make: Make<'a>,
    /// The canonical replay configuration.
    pub config: BackendRunConfig,
    /// The canonical encoding of the perturbation the backend is built with
    /// (`docs/m5-design.md` §18), or `None` for an unperturbed side. A reproducer holds it
    /// as the side's `perturbation.bin`.
    pub perturbation: Option<&'a [u8]>,
}

impl<'a> Builder<'a> {
    /// Both sides of a same-engine case: SystemScope runtimes `make` builds, under
    /// `config`.
    pub fn same(make: &'a dyn Fn() -> RuntimeBackend, config: BackendRunConfig) -> [Self; 2] {
        [Builder {
            make: Make::Runtime(make),
            config,
            perturbation: None,
        }; 2]
    }

    /// The runtime maker, if this side is a SystemScope runtime.
    fn runtime(&self) -> Option<&'a dyn Fn() -> RuntimeBackend> {
        match self.make {
            Make::Runtime(make) => Some(make),
            Make::Other(_) => None,
        }
    }
}

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
    let sides = Builder::same(make, config);
    settle(searched(plan, scenario, sides, inputs, every).map(|(r, _)| r))
}

/// Verifies `inputs[1]` against `inputs[0]` from `origins`, each side's checkpoint or, for
/// `None`, its initial state, with the evidence [`same_engine`] gives where both sides
/// are SystemScope runtimes: a reproducer's result, a function of its material alone
/// (§16.2). A checkpoint must cover at most the divergence minus the report window, and
/// an agreement starts from the initial states.
pub fn from_origins(
    plan: &Plan,
    scenario: &Scenario,
    sides: [Builder<'_>; 2],
    inputs: [InputVariant; 2],
    origins: [Option<&Checkpoint>; 2],
) -> VerificationResult {
    settle((|| {
        let (mut x, mut y) = (sides[0].make.build(), sides[1].make.build());
        let located = resume(
            plan,
            scenario,
            (side(&mut *x, &inputs[0], sides[0].config), origins[0]),
            (side(&mut *y, &inputs[1], sides[1].config), origins[1]),
        )?;
        let checkpoints = origins.map(|c| c.map_or(&[][..], std::slice::from_ref));
        evidence(plan, scenario, sides, &inputs, located, checkpoints)
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

/// The result of [`locate`] on `sides`, unsettled, with each side's index: every side
/// that can step is indexed with a checkpoint every `every` primary boundaries, and
/// checkpoint search is used when both sides can restore.
pub(crate) fn searched(
    plan: &Plan,
    scenario: &Scenario,
    sides: [Builder<'_>; 2],
    inputs: [InputVariant; 2],
    every: u64,
) -> Result<(VerificationResult, Vec<Index>), EngineError> {
    let roles = [Role::Expected, Role::Actual];
    let mut indexes = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        let mut b = sides[i].make.build();
        indexes.push(if b.capabilities().contains(&Capability::StepBoundary) {
            record_index(
                plan,
                scenario,
                side(&mut *b, input, sides[i].config),
                roles[i],
                every,
            )?
        } else {
            Index::default()
        });
    }
    let (mut x, mut y) = (sides[0].make.build(), sides[1].make.build());
    let located = locate(
        plan,
        scenario,
        side(&mut *x, &inputs[0], sides[0].config),
        side(&mut *y, &inputs[1], sides[1].config),
        Some((&indexes[0], &indexes[1])),
    )?;
    let checkpoints = [&indexes[0].checkpoints[..], &indexes[1].checkpoints[..]];
    let result = evidence(plan, scenario, sides, &inputs, located, checkpoints)?;
    Ok((result, indexes))
}

/// The evidence of a located comparison: the auxiliary windows, and, where both sides are
/// SystemScope runtimes, the state and trace evidence, scanning from `checkpoints`.
fn evidence(
    plan: &Plan,
    scenario: &Scenario,
    sides: [Builder<'_>; 2],
    inputs: &[InputVariant; 2],
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
        for (s, input) in sides.iter().zip(inputs) {
            let mut b = s.make.build();
            recordings.push(run(plan, scenario, side(&mut *b, input, s.config)));
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
    let (Some(make_e), Some(make_a)) = (sides[0].runtime(), sides[1].runtime()) else {
        return Ok(result);
    };
    let make = [make_e, make_a];
    let config = sides.map(|s| s.config);
    // Each side's state after its own boundary `ordinal`, or where its stream ended.
    let mut states = Vec::new();
    let mut read_at = [0; 2];
    let mut until = 0;
    for i in 0..2 {
        let mut b = make[i]();
        b.start(scenario, &inputs[i], &config[i], &plan.profile)?;
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
        config: config[i],
        checkpoints: checkpoints[i],
    };
    let (mut x, mut y) = (make[0](), make[1]());
    let first =
        first_state_divergence(plan, scenario, same(0, &mut x), same(1, &mut y), &[], until)?;
    result.state = Some(StateEvidence {
        read_at,
        differences: diff_state(&states[0], &states[1]),
        first: Some(first),
    });
    let (mut x, mut y) = (make[0](), make[1]());
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
    backend: &'a mut dyn Backend,
    input: &InputVariant,
    config: BackendRunConfig,
) -> Side<'a> {
    Side {
        backend,
        input: input.clone(),
        config,
    }
}
