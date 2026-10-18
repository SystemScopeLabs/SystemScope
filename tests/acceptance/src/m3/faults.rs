//! The fault-injection campaign (`docs/m4-design.md` §18, §25): seeded register faults
//! on `m3-reference`, each located by the same-engine checkpoint search and by the naive
//! oracle, which must agree, whether the fault diverges, is masked, or stops the run.
//!
//! A seed draws one fault: a boundary `b` past the first, a register `x1` to `x31`, and a
//! bit. The fault flips that bit after the event of boundary `b − 1`; its old value is the
//! register as the retirements before `b` wrote it, from reset's 0.

use systemscope_contracts::rng::SimRng;
use systemscope_contracts::trace::Value;
use systemscope_runtime::rng::{Xoshiro256StarStar, seed_material};
use systemscope_verify::backend::{Backend, Side, verify as naive};
use systemscope_verify::case::{InputVariant, StatePatch};
use systemscope_verify::compare::{StreamResult, Verdict};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::report::same_engine;

use super::Fixture;
use super::expected::the_plan;
use super::verify::{self, CPU_PATH};

/// BLAKE3 `derive_key` context for fault seeds, in m0-design §5.2's form.
pub const CONTEXT: &str = "SystemScope 2026-10 m4 fault injection v1";
/// The path in the seed material, after the seed.
pub const PATH: &str = "m4.faults";
/// The seeds every CI run injects.
pub const FIXED_SEEDS: std::ops::Range<u64> = 0..32;
/// Environment variable holding the nightly seed, in decimal or `0x` hex.
pub const SEED_VAR: &str = "M4_FAULT_SEED";
/// The checkpoint density: the same-engine report's, a checkpoint about every 10,000
/// events (§30.5).
pub const EVERY: u64 = 133;

/// One drawn fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    /// Its seed.
    pub seed: u64,
    /// The first boundary after the fault.
    pub boundary: u64,
    /// The register, `1` to `31`.
    pub register: u32,
    /// The flipped bit.
    pub bit: u32,
    /// The state patch.
    pub patch: InputVariant,
}

/// The unmodified run's `arch` stream and the event emitting each boundary.
pub struct Reference {
    arch: Vec<Observation>,
    events: Vec<u64>,
}

/// Records the unmodified run.
pub fn reference(fixture: &Fixture) -> Result<Reference, String> {
    let mut b = verify::backend(fixture);
    let e = |e: EngineError| e.to_string();
    b.start(
        &verify::scenario(fixture),
        &InputVariant::None,
        &verify::config(),
        &verify::profile(),
    )
    .map_err(e)?;
    let mut arch = Vec::new();
    while let Some(o) = b.next("arch").map_err(e)? {
        arch.push(o);
    }
    let events = (0..arch.len() as u64)
        .map(|i| {
            b.event_index("arch", i)
                .ok_or(format!("no event at boundary {i}"))
        })
        .collect::<Result<_, _>>()?;
    Ok(Reference { arch, events })
}

fn u32_of(o: &Observation, field: &str) -> Result<u32, String> {
    match o.field(field) {
        Some(ObservedValue::Present(Value::U64(v))) => u32::try_from(*v)
            .map_err(|_| format!("arch[{}].{field} {v:#x} is not 32-bit", o.ordinal)),
        other => Err(format!("arch[{}].{field} is {other:?}", o.ordinal)),
    }
}

/// The fault `seed` draws on `r`.
pub fn fault(r: &Reference, seed: u64) -> Result<Fault, String> {
    let key = blake3::derive_key(CONTEXT, &seed_material(seed, PATH));
    let mut rng = Xoshiro256StarStar::from_seed_bytes(key);
    let n = r.arch.len() as u64;
    let boundary = 1 + rng.next_u64() % (n - 1);
    let register = 1 + (rng.next_u64() % 31) as u32;
    let bit = (rng.next_u64() % 32) as u32;
    let mut x = [0u32; 32];
    for o in &r.arch[..boundary as usize] {
        if let Some(ObservedValue::Present(Value::U64(rd))) = o.field("rd") {
            x[*rd as usize] = u32_of(o, "rd_value")?;
        }
    }
    let old = x[register as usize];
    Ok(Fault {
        seed,
        boundary,
        register,
        bit,
        patch: InputVariant::State(StatePatch {
            event_index: r.events[boundary as usize - 1],
            field: format!("{CPU_PATH}/x{register}"),
            old: Value::U64(old.into()),
            new: Value::U64((old ^ (1 << bit)).into()),
        }),
    })
}

/// What the locator and the naive oracle both find for `fault`: `Agree` for a masked
/// fault, `Diverged at arch[d]`, or `BackendFailed`. An error when they disagree.
pub fn check(fixture: &Fixture, fault: &Fault) -> Result<String, String> {
    let plan = the_plan(fixture)?;
    let scenario = verify::scenario(fixture);
    let make = || verify::backend(fixture);
    let inputs = [InputVariant::None, fault.patch.clone()];
    let located = same_engine(
        &plan,
        &scenario,
        &make,
        inputs.clone(),
        verify::config(),
        EVERY,
    )
    .verdict;
    let (mut e, mut a) = (make(), make());
    let oracle = naive(
        &plan.profile,
        &scenario,
        Side {
            backend: &mut e,
            input: inputs[0].clone(),
            config: verify::config(),
        },
        Side {
            backend: &mut a,
            input: inputs[1].clone(),
            config: verify::config(),
        },
    );
    let primary = |v: &Result<Verdict, EngineError>| match v {
        Ok(Verdict::Compared(c)) => Ok(c.primary.clone()),
        Ok(other) => Err(format!("{other:?}")),
        Err(EngineError::BackendFailed(_)) => Err("BackendFailed".to_owned()),
        Err(e) => Err(format!("engine error: {e}")),
    };
    let (l, o) = (primary(&located), primary(&oracle));
    if l != o {
        return Err(format!(
            "seed {}: the locator finds {l:?}, the naive oracle {o:?}",
            fault.seed
        ));
    }
    Ok(match l {
        Ok(StreamResult::Agree { .. }) => "Agree".to_owned(),
        Ok(StreamResult::Diverged { ordinal, .. }) => format!("Diverged at arch[{ordinal}]"),
        Err(what) if what == "BackendFailed" => what,
        Err(what) => return Err(format!("seed {}: {what}", fault.seed)),
    })
}

/// Checks the fault of every seed in `seeds`: each seed's fault and finding, or every
/// error.
pub fn campaign(
    fixture: &Fixture,
    seeds: impl IntoIterator<Item = u64>,
) -> Result<Vec<(Fault, String)>, Vec<String>> {
    let r = reference(fixture).map_err(|e| vec![e])?;
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for seed in seeds {
        match fault(&r, seed).and_then(|f| check(fixture, &f).map(|found| (f, found))) {
            Ok(x) => ok.push(x),
            Err(e) => errors.push(e),
        }
    }
    match errors.is_empty() {
        true => Ok(ok),
        false => Err(errors),
    }
}
