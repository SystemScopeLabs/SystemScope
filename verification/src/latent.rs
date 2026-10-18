//! The first state divergence of two same-engine sides (`docs/m4-design.md` §12.4).
//!
//! A divergence can exist in state before any compared boundary shows it. Two sides of
//! the same engine and topology share global time, so their states can be compared event
//! by event. The result is evidence, labeled as such: the first divergence of a
//! comparison is always the primary stream's.

use systemscope_runtime::snapshot::{ComponentState, component_states};

use crate::backend::Backend;
use crate::case::{BackendRunConfig, InputVariant, Scenario};
use crate::checkpoint::Checkpoint;
use crate::error::EngineError;
use crate::profile::Plan;
use crate::runtime::RuntimeBackend;

/// One same-engine side: a runtime backend, its input variant, run configuration, and
/// the checkpoints it may resume from.
pub struct SameSide<'a> {
    /// The backend.
    pub backend: &'a mut RuntimeBackend,
    /// Its input variant.
    pub input: InputVariant,
    /// Its run configuration.
    pub config: BackendRunConfig,
    /// Its own checkpoints, such as an index's.
    pub checkpoints: &'a [Checkpoint],
}

/// Where two sides' states first differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDivergence {
    /// The events both sides had dispatched where the scan began: the latest checkpoint
    /// pair at one event index with equal states, or 0.
    pub origin: u64,
    /// The events both sides had dispatched when their states first differed, at or
    /// after `origin`, or `None` if they were equal through the end of the scan. Equal
    /// to `origin` only when the initial states differ.
    pub event: Option<u64>,
    /// The topology paths of the components whose stored state differs there, in
    /// component order, and `runtime` when the session's own state differs.
    pub differing: Vec<String>,
}

/// Scans both sides event by event from a common origin to `until` events, comparing
/// their snapshots, and returns the first event count where they differ (§12.4).
///
/// `scope` names the components compared, by topology path; empty compares the whole
/// snapshot. A scope sees past a difference that is already there, such as a patched
/// disk's media identity, to where it reaches another component.
///
/// The origin is the latest pair of the sides' checkpoints at one event index, at most
/// `until`, whose states are byte-equal within the scope: the `StateDigest` accelerator,
/// since equal snapshots are equal full state. Without one, both sides start from their
/// initial states. States that differ and then agree again before the origin are not
/// seen: the result is the first divergence after the origin.
pub fn first_state_divergence(
    plan: &Plan,
    scenario: &Scenario,
    expected: SameSide<'_>,
    actual: SameSide<'_>,
    scope: &[&str],
    until: u64,
) -> Result<StateDivergence, EngineError> {
    let mut sides = [expected, actual];
    for side in &mut sides {
        side.backend
            .start(scenario, &side.input, &side.config, &plan.profile)?;
    }
    let paths: Vec<String> = sides[0]
        .backend
        .runtime()
        .expect("started")
        .component_paths()
        .map(str::to_owned)
        .collect();
    let ids: Vec<u32> = scope
        .iter()
        .map(|c| {
            paths
                .iter()
                .position(|p| p == c)
                .map(|i| i as u32)
                .ok_or_else(|| EngineError::Internal(format!("no component {c}")))
        })
        .collect::<Result<_, _>>()?;
    let view = |bytes: &[u8]| scoped(bytes, &ids);
    let (ke, ka) = (sides[0].checkpoints, sides[1].checkpoints);
    let mut origin: Option<(&Checkpoint, &Checkpoint)> = None;
    for ce in ke.iter().filter(|c| c.event_index <= until) {
        if origin.is_some_and(|(o, _)| o.event_index >= ce.event_index) {
            continue;
        }
        let want = view(&ce.state)?;
        for ca in ka.iter().filter(|ca| ca.event_index == ce.event_index) {
            if view(&ca.state)? == want {
                origin = Some((ce, ca));
                break;
            }
        }
    }
    if let Some((ce, ca)) = origin {
        sides[0].backend.restore(ce)?;
        sides[1].backend.restore(ca)?;
    }
    let origin = origin.map_or(0, |(c, _)| c.event_index);
    let [e, a] = sides.map(|s| s.backend);
    loop {
        let at = e.events().expect("started");
        let (se, sa) = (snapshot(e)?, snapshot(a)?);
        if view(&se)? != view(&sa)? {
            let mut differing = differing(&paths, &se, &sa)?;
            if !scope.is_empty() {
                differing.retain(|c| scope.contains(&c.as_str()));
            }
            return Ok(StateDivergence {
                origin,
                event: Some(at),
                differing,
            });
        }
        if at >= until {
            return Ok(StateDivergence {
                origin,
                event: None,
                differing: Vec::new(),
            });
        }
        e.step_event()?;
        a.step_event()?;
    }
}

fn read(bytes: &[u8]) -> Result<Vec<ComponentState<'_>>, EngineError> {
    component_states(bytes)
        .map_err(|e| EngineError::Internal(format!("the snapshot does not read back: {e}")))
}

/// The compared bytes of a snapshot: all of them, or the components `ids`' own state.
fn scoped(bytes: &[u8], ids: &[u32]) -> Result<Vec<u8>, EngineError> {
    if ids.is_empty() {
        return Ok(bytes.to_vec());
    }
    let states = read(bytes)?;
    let mut out = Vec::new();
    for id in ids {
        let (_, _, state) = states
            .iter()
            .find(|s| s.0 == *id)
            .ok_or_else(|| EngineError::Internal(format!("the snapshot has no component {id}")))?;
        out.extend((state.len() as u64).to_le_bytes());
        out.extend(*state);
    }
    Ok(out)
}

fn snapshot(b: &RuntimeBackend) -> Result<Vec<u8>, EngineError> {
    b.runtime()
        .expect("started")
        .snapshot()
        .map_err(|e| EngineError::BackendFailed(format!("snapshot: {e}")))
}

/// The components whose stored bytes differ, then `runtime` if anything else does: the
/// session's own state and the component table.
fn differing(paths: &[String], e: &[u8], a: &[u8]) -> Result<Vec<String>, EngineError> {
    let (ce, ca) = (read(e)?, read(a)?);
    let mut out: Vec<String> = Vec::new();
    for (x, y) in ce.iter().zip(&ca) {
        if x != y {
            out.push(
                paths
                    .get(x.0 as usize)
                    .map_or("?", String::as_str)
                    .to_owned(),
            );
        }
    }
    if ce.len() != ca.len() || rest(e, &ce) != rest(a, &ca) {
        out.push("runtime".to_owned());
    }
    Ok(out)
}
/// The bytes of a snapshot outside its components' own state.
fn rest(bytes: &[u8], states: &[ComponentState<'_>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    for (_, _, state) in states {
        let start = state.as_ptr() as usize - bytes.as_ptr() as usize;
        out.extend(&bytes[at..start]);
        at = start + state.len();
    }
    out.extend(&bytes[at..]);
    out
}
