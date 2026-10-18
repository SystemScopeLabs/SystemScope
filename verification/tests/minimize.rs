//! Prefix and replay-window minimization on the counter (`docs/m4-design.md` §17).

mod common;

use common::*;
use std::cell::Cell;
use systemscope_verify::backend::Backend;

use systemscope_contracts::trace::Value;
use systemscope_verify::bundle::{Case, replay, write};
use systemscope_verify::case::InputVariant;
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::compare::{
    Comparison, Divergence, FieldDiff, Outcome, StreamResult, Verdict,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::record_index;
use systemscope_verify::minimize::{accepts, minimize, same_failure, window_origins};
use systemscope_verify::observation::ObservedValue;
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::report::same_engine;
use systemscope_verify::result::ResultOutcome;

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

const DIVERGED: u64 = 8;
/// The counter's report window.
const W: u64 = 3;

fn diverging() -> [InputVariant; 2] {
    [
        InputVariant::None,
        patch_n(DIVERGED, DIVERGED, DIVERGED + 1),
    ]
}

/// One side's checkpoints, after every `every` primary boundaries.
fn checkpoints(input: InputVariant, role: Role, every: u64) -> Vec<Checkpoint> {
    let mut b = backend();
    record_index(&the_plan(), &scenario(), side(&mut b, input), role, every)
        .unwrap()
        .checkpoints
}

fn covered(c: Option<&Checkpoint>) -> Option<u64> {
    c.map(|c| c.mark("arch").unwrap().covered)
}

fn u(v: u64) -> ObservedValue {
    ObservedValue::Present(Value::U64(v))
}

fn fields(name: &str, expected: u64, actual: u64) -> Divergence {
    Divergence::Fields {
        kind: "retire".to_owned(),
        fields: vec![FieldDiff {
            name: name.to_owned(),
            expected: u(expected),
            actual: u(actual),
        }],
    }
}

fn diverged(stream: &str, ordinal: u64, divergence: Divergence) -> StreamResult {
    StreamResult::Diverged {
        stream: stream.to_owned(),
        ordinal,
        divergence,
    }
}

#[test]
fn the_same_failure_may_move_but_never_change() {
    let a = diverged("arch", 8, fields("pc", 1, 2));
    assert!(same_failure(&a, &a));
    assert!(same_failure(&a, &diverged("arch", 3, fields("pc", 1, 2))));
    for other in [
        diverged("os", 8, fields("pc", 1, 2)),
        diverged("arch", 8, fields("rd_value", 1, 2)),
        diverged("arch", 8, fields("pc", 0, 2)),
        diverged("arch", 8, fields("pc", 1, 3)),
        diverged(
            "arch",
            8,
            Divergence::Kind {
                expected: "retire".to_owned(),
                actual: "exception".to_owned(),
            },
        ),
        diverged(
            "arch",
            8,
            Divergence::Ended {
                ended: Role::Actual,
                kind: "retire".to_owned(),
            },
        ),
        StreamResult::Agree {
            stream: "arch".to_owned(),
            compared: 8,
            final_prefix: [0; 32],
        },
    ] {
        assert!(!same_failure(&a, &other), "{other:?}");
        assert!(!same_failure(&other, &a), "{other:?}");
    }
    let agree = StreamResult::Agree {
        stream: "arch".to_owned(),
        compared: 8,
        final_prefix: [0; 32],
    };
    assert!(!same_failure(&agree, &agree));
}

#[test]
fn the_window_starts_at_the_latest_common_checkpoint() {
    let [e, a] = diverging();
    let (ke, ka) = (
        checkpoints(e.clone(), Role::Expected, 1),
        checkpoints(a.clone(), Role::Actual, 1),
    );
    for limit in 0..=6 {
        let o = window_origins("arch", [&ke, &ka], [true, true], limit);
        let want = (limit > 0).then_some(limit);
        assert_eq!(o.map(covered), [want, want], "{limit}");
        if let [Some(x), Some(y)] = o {
            assert_eq!(x.execution, ke[0].execution);
            assert_eq!(y.execution, ka[0].execution);
        }
    }
    // A side that cannot restore starts from its initial state.
    let o = window_origins("arch", [&ke, &ka], [true, false], 5);
    assert_eq!(o.map(covered), [Some(5), None]);
    let o = window_origins("arch", [&ke, &ka], [false, true], 5);
    assert_eq!(o.map(covered), [None, Some(5)]);
    let o = window_origins("arch", [&ke, &ka], [false, false], 5);
    assert_eq!(o.map(covered), [None, None]);
    // Only a checkpoint both restoring sides hold.
    let (k2, k3) = (
        checkpoints(e, Role::Expected, 2),
        checkpoints(a, Role::Actual, 3),
    );
    let o = window_origins("arch", [&k2, &k3], [true, true], 7);
    assert_eq!(o.map(covered), [Some(6), Some(6)]);
    let o = window_origins("arch", [&k2, &k3], [true, true], 5);
    assert_eq!(o.map(covered), [None, None]);
    let o = window_origins("arch", [&k2, &k3], [false, true], 5);
    assert_eq!(o.map(covered), [None, Some(3)]);
    let o = window_origins("arch", [&k2, &[]], [true, true], 5);
    assert_eq!(o.map(covered), [None, None]);
    // Another stream marks nothing.
    let o = window_origins("os", [&ke, &ka], [true, true], 5);
    assert_eq!(o.map(covered), [None, None]);
}

#[test]
fn a_minimized_divergence_replays_from_the_latest_window_checkpoint() {
    let plan = the_plan();
    let scenario = scenario();
    let found = same_engine(&plan, &scenario, &backend, diverging(), config(), 1);
    for every in [0, 1, 2, 3, 4, 5, 6, 10] {
        let m = minimize(&plan, &scenario, &backend, diverging(), config(), every).unwrap();
        let limit = DIVERGED - W;
        let want = (every > 0 && every <= limit).then(|| limit / every * every);
        assert_eq!(m.origins, [want, want], "{every}");
        assert_eq!(
            (&m.result.verdict, &m.result.window),
            (&found.verdict, &found.window)
        );
        let at = want.map(|n| {
            [Role::Expected, Role::Actual].map(|role| {
                let ks = checkpoints(diverging()[role as usize].clone(), role, 1);
                ks.into_iter()
                    .find(|c| covered(Some(c)) == Some(n))
                    .unwrap()
            })
        });
        let origins = match &at {
            Some([e, a]) => [Some(e), Some(a)],
            None => [None, None],
        };
        let written = write(&plan, &scenario, &backend, diverging(), config(), origins);
        assert_eq!(written.unwrap(), m.bundle, "{every}");
        let case = Case {
            plan: &plan,
            scenario: &scenario,
            config: config(),
            make: &backend,
        };
        assert_eq!(replay(&m.bundle, case).unwrap(), m.result);
    }
}

#[test]
fn an_agreement_starts_from_the_initial_states() {
    let plan = the_plan();
    let same = [InputVariant::None, InputVariant::None];
    let m = minimize(&plan, &scenario(), &backend, same, config(), 1).unwrap();
    assert_eq!(m.origins, [None, None]);
    assert_eq!(m.result.outcome(), ResultOutcome::Compared(Outcome::Agree));
    // Every call of `make` alternates the sides, so only the auxiliary stream differs.
    let calls = Cell::new(0);
    let make = || {
        calls.set(calls.get() + 1);
        if calls.get() % 2 == 1 {
            backend()
        } else {
            shifted_backend()
        }
    };
    let same = [InputVariant::None, InputVariant::None];
    let m = minimize(&plan, &scenario(), &make, same, config(), 1).unwrap();
    assert_eq!(m.origins, [None, None]);
    assert_eq!(
        m.result.outcome(),
        ResultOutcome::Compared(Outcome::AuxiliaryDivergence)
    );
}

#[test]
fn a_failing_search_has_no_reproducer() {
    let plan = the_plan();
    let bad = [InputVariant::None, patch_n(DIVERGED, 0, 1)];
    assert!(minimize(&plan, &scenario(), &backend, bad, config(), 1).is_err());
}

fn compared(primary: StreamResult) -> Result<Verdict, EngineError> {
    Ok(Verdict::Compared(Comparison {
        primary,
        auxiliary: Vec::new(),
        not_compared: Vec::new(),
        failures: Vec::new(),
    }))
}

#[test]
fn a_candidate_must_keep_the_searched_failure() {
    let full = compared(diverged("arch", 8, fields("pc", 1, 2)));
    assert!(accepts(&full, &full));
    assert!(accepts(
        &full,
        &compared(diverged("arch", 5, fields("pc", 1, 2)))
    ));
    assert!(!accepts(
        &full,
        &compared(diverged("arch", 8, fields("pc", 1, 3)))
    ));
    let agree = compared(StreamResult::Agree {
        stream: "arch".to_owned(),
        compared: 8,
        final_prefix: [0; 32],
    });
    assert!(!accepts(&full, &agree));
    assert!(!accepts(&agree, &full));
    assert!(accepts(&agree, &agree));
    let other = compared(StreamResult::Agree {
        stream: "arch".to_owned(),
        compared: 9,
        final_prefix: [0; 32],
    });
    assert!(!accepts(&agree, &other));
    let failed = Err(EngineError::Internal("x".to_owned()));
    assert!(!accepts(&full, &failed));
    assert!(!accepts(&failed, &full));
    assert!(accepts(&failed, &failed));
}
