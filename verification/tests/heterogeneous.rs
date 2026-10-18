//! A side that can only record, as Spike, against a SystemScope runtime on the counter:
//! the origins, results, minimization, and bundles of a heterogeneous case
//! (`docs/m4-design.md` §11.4, §16.2, §17.2).

mod common;

use common::*;

use systemscope_verify::backend::Backend;
use systemscope_verify::bundle::{Case, ReplayError, replay, validate, write};
use systemscope_verify::case::InputVariant;
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::compare::Outcome;
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::{Strategy, record_index, resume};
use systemscope_verify::minimize::minimize;
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::report::{Builder, Make, from_origins, same_engine};
use systemscope_verify::result::ResultOutcome;

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

const DIVERGED: u64 = 8;

fn diverging() -> [InputVariant; 2] {
    [
        InputVariant::None,
        patch_n(DIVERGED, DIVERGED, DIVERGED + 1),
    ]
}

/// The expected side records only; the actual side is a runtime.
fn sides() -> [Builder<'static>; 2] {
    [
        Builder {
            make: Make::Other(&post_run),
            config: config(),
        },
        Builder {
            make: Make::Runtime(&backend),
            config: config(),
        },
    ]
}

/// The actual side's checkpoint covering `covered` boundaries.
fn actual_checkpoint(input: InputVariant, covered: u64) -> Checkpoint {
    let mut b = backend();
    record_index(
        &the_plan(),
        &scenario(),
        side(&mut b, input),
        Role::Actual,
        1,
    )
    .unwrap()
    .checkpoints
    .into_iter()
    .find(|c| c.mark("arch").unwrap().covered == covered)
    .unwrap()
}

#[test]
fn a_recorded_side_finds_what_two_runtimes_find() {
    let plan = the_plan();
    let found = same_engine(&plan, &scenario(), &backend, diverging(), config(), 1);
    // The report window is 3: a checkpoint covers at most 8 - 3 boundaries.
    for covered in [None, Some(1), Some(5)] {
        let k = covered.map(|n| actual_checkpoint(diverging()[1].clone(), n));
        let r = from_origins(&plan, &scenario(), sides(), diverging(), [None, k.as_ref()]);
        assert_eq!(
            (&r.verdict, &r.window),
            (&found.verdict, &found.window),
            "{covered:?}"
        );
        // Only two runtimes give state and trace evidence.
        assert_eq!(
            (r.state, r.trace.canonical, r.slices),
            (None, None, Vec::new())
        );
    }
    let mut e = post_run();
    let mut a = backend();
    let k = actual_checkpoint(diverging()[1].clone(), 5);
    let located = resume(
        &plan,
        &scenario(),
        (side_of(&mut *e, diverging()[0].clone()), None),
        (side_of(&mut a, diverging()[1].clone()), Some(&k)),
    )
    .unwrap();
    assert_eq!(located.strategy, Strategy::StreamCompare);
    let (mut x, mut y) = (backend(), backend());
    let located = resume(
        &plan,
        &scenario(),
        (side_of(&mut x, diverging()[0].clone()), None),
        (side_of(&mut y, diverging()[1].clone()), Some(&k)),
    )
    .unwrap();
    assert_eq!(located.strategy, Strategy::Lockstep);
}

fn side_of(b: &mut dyn Backend, input: InputVariant) -> systemscope_verify::backend::Side<'_> {
    systemscope_verify::backend::Side {
        backend: b,
        input,
        config: config(),
    }
}

#[test]
fn a_recorded_side_must_reach_the_origin_with_its_prefix() {
    let plan = the_plan();
    // A checkpoint of a side patched at event 2 has a prefix the unpatched side never has.
    let early = patch_n(2, 2, 3);
    let k = actual_checkpoint(early.clone(), 5);
    let r = from_origins(
        &plan,
        &scenario(),
        sides(),
        [InputVariant::None, early],
        [None, Some(&k)],
    );
    assert!(
        matches!(r.verdict, Err(EngineError::BoundaryDesync(_))),
        "{r:?}"
    );
    // A recorded side that ends before the origin.
    let k = actual_checkpoint(InputVariant::None, 9);
    let short = [
        Builder {
            make: Make::Other(&|| Box::new(PostRun(backend_with(true, Some(3))))),
            config: config(),
        },
        sides()[1],
    ];
    let r = from_origins(&plan, &scenario(), short, diverging(), [None, Some(&k)]);
    assert!(
        matches!(r.verdict, Err(EngineError::BoundaryDesync(_))),
        "{r:?}"
    );
    // A recorded side never restores.
    let r = from_origins(&plan, &scenario(), sides(), diverging(), [Some(&k), None]);
    assert!(
        matches!(r.verdict, Err(EngineError::Unsupported(_))),
        "{r:?}"
    );
}

#[test]
fn a_recorded_side_agrees_only_from_the_initial_states() {
    let plan = the_plan();
    let same = [InputVariant::None, InputVariant::None];
    let r = from_origins(&plan, &scenario(), sides(), same.clone(), [None, None]);
    assert_eq!(r.outcome(), ResultOutcome::Compared(Outcome::Agree));
    let k = actual_checkpoint(InputVariant::None, 5);
    let r = from_origins(&plan, &scenario(), sides(), same, [None, Some(&k)]);
    assert!(matches!(r.verdict, Err(EngineError::Internal(_))), "{r:?}");
}

#[test]
fn a_heterogeneous_reproducer_starts_the_runtime_at_its_window() {
    let plan = the_plan();
    let scenario = scenario();
    let found = same_engine(&plan, &scenario, &backend, diverging(), config(), 1);
    for (every, want) in [(0, None), (1, Some(5)), (2, Some(4)), (6, None)] {
        let m = minimize(&plan, &scenario, sides(), diverging(), every).unwrap();
        assert_eq!(m.origins, [None, want], "{every}");
        assert_eq!(
            (&m.result.verdict, &m.result.window),
            (&found.verdict, &found.window)
        );
        let case = Case {
            plan: &plan,
            scenario: &scenario,
            sides: sides(),
        };
        assert_eq!(replay(&m.bundle, case).unwrap(), m.result);
    }
    let same = [InputVariant::None, InputVariant::None];
    let m = minimize(&plan, &scenario, sides(), same, 1).unwrap();
    assert_eq!(m.origins, [None, None]);
    assert_eq!(m.result.outcome(), ResultOutcome::Compared(Outcome::Agree));
}

#[test]
fn only_a_runtime_restores_a_bundled_checkpoint() {
    let plan = the_plan();
    let scenario = scenario();
    let k = actual_checkpoint(diverging()[1].clone(), 5);
    let b = write(&plan, &scenario, sides(), diverging(), [None, Some(&k)]).unwrap();
    let case = |sides| Case {
        plan: &plan,
        scenario: &scenario,
        sides,
    };
    assert!(validate(&b, case(sides())).is_ok());
    // The same backend, built as any other backend, cannot restore it.
    let other = [
        sides()[0],
        Builder {
            make: Make::Other(&|| Box::new(backend())),
            config: config(),
        },
    ];
    assert!(matches!(
        replay(&b, case(other)),
        Err(ReplayError::Corrupt(EngineError::CorruptReproducer(why))) if why.contains("cannot restore")
    ));
    // The recorded side is not the runtime the manifest records.
    let swapped = [sides()[1], sides()[1]];
    assert!(validate(&b, case(swapped)).is_err());
}
