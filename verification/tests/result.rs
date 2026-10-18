//! The verification result on the counter: a same-engine report with every evidence,
//! its canonical encoding, and its text rendering (`docs/m4-design.md` §6.7, §13, §14).

mod common;

use common::*;
use systemscope_contracts::canonical::DecodeError;
use systemscope_contracts::trace::Value;
use systemscope_verify::backend::Backend;
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{
    Comparison, Divergence, FieldDiff, Outcome, StreamResult, Verdict,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::locate::Window;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::{Incomparable, Plan, Role, plan};
use systemscope_verify::report::same_engine;
use systemscope_verify::result::{
    RESULT_MAGIC, ResultOutcome, StateEvidence, TraceEvidence, VerificationResult,
};
use systemscope_verify::state::{Change, Source, StateDifference};
use systemscope_verify::trace::Diagnostic;

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

fn report(input: InputVariant, every: u64) -> VerificationResult {
    same_engine(
        &the_plan(),
        &scenario(),
        &backend,
        [InputVariant::None, input],
        config(),
        every,
    )
}

fn round_trip(r: &VerificationResult) {
    let bytes = r.to_bytes();
    assert_eq!(VerificationResult::from_bytes(&bytes).as_ref(), Ok(r));
}

#[test]
fn a_same_engine_report_carries_every_evidence() {
    let r = report(patch_n(4, 4, 5), 2);
    assert_eq!(r.outcome(), ResultOutcome::Compared(Outcome::Diverged));
    let Ok(Verdict::Compared(c)) = &r.verdict else {
        panic!("{r:?}")
    };
    let StreamResult::Diverged { ordinal, .. } = c.primary else {
        panic!()
    };
    assert_eq!(ordinal, 4);
    assert!(r.window.is_some());
    let state = r.state.as_ref().unwrap();
    // Both sides read after their boundary 4: event 5 each, n = 5 and 6.
    assert_eq!(state.read_at, [5, 5]);
    let n = |v: u64| Value::U64(v);
    assert!(state.differences.contains(&StateDifference {
        component: "counter".to_owned(),
        field: "n".to_owned(),
        source: Source::Inspect,
        change: Change::Changed {
            expected: n(5),
            actual: n(6),
        },
    }));
    let first = state.first.as_ref().unwrap();
    assert_eq!(
        (first.event, &first.differing[..]),
        (Some(4), &["counter".to_owned()][..])
    );
    let trace = r.trace.canonical.as_ref().unwrap();
    assert_eq!(trace.event, Some(5));
    round_trip(&r);
    // Agreement has no divergence evidence.
    let agree = report(InputVariant::None, 2);
    assert_eq!(agree.outcome(), ResultOutcome::Compared(Outcome::Agree));
    assert_eq!((&agree.window, &agree.state), (&None, &None));
    assert_eq!(agree.trace, TraceEvidence::default());
    round_trip(&agree);
}

#[test]
fn reports_are_byte_identical_across_runs() {
    for input in [patch_n(4, 4, 5), patch_n(3, 3, 4), InputVariant::None] {
        let base = report(input.clone(), 0);
        for every in [0, 1, 3] {
            let r = report(input.clone(), every);
            assert_eq!(
                report(input.clone(), every).to_bytes(),
                r.to_bytes(),
                "{every}"
            );
            assert_eq!(report(input.clone(), every).to_string(), r.to_string());
            // Density never changes the comparison; the evidence scans start from the
            // checkpoints there are.
            assert_eq!(
                (&r.verdict, &r.window),
                (&base.verdict, &base.window),
                "{every}"
            );
            let event = |r: &VerificationResult| {
                let s = r.state.as_ref().map(|s| (s.read_at, s.differences.clone()));
                let first = r
                    .state
                    .as_ref()
                    .and_then(|s| s.first.as_ref())
                    .map(|f| f.event);
                let trace = r.trace.canonical.as_ref().map(|t| t.event);
                (s, first, trace, r.trace.auxiliary.clone())
            };
            assert_eq!(event(&r), event(&base), "{every}");
        }
    }
}

#[test]
fn an_engine_failure_is_the_result() {
    let r = report(patch_n(4, 99, 5), 2);
    assert_eq!(r.outcome(), ResultOutcome::EngineError);
    assert!(
        matches!(&r.verdict, Err(EngineError::BackendFailed(_))),
        "{r:?}"
    );
    assert_eq!((&r.window, &r.state), (&None, &None));
    round_trip(&r);
}

fn obs(ordinal: u64, kind: &str, fields: &[(&str, ObservedValue)]) -> Observation {
    Observation {
        stream: "arch".to_owned(),
        ordinal,
        kind: kind.to_owned(),
        fields: fields
            .iter()
            .map(|(n, v)| ((*n).to_owned(), v.clone()))
            .collect(),
    }
}

fn hand_built() -> VerificationResult {
    let p = |v: u64| ObservedValue::Present(Value::U64(v));
    VerificationResult {
        verdict: Ok(Verdict::Compared(Comparison {
            primary: StreamResult::Diverged {
                stream: "arch".to_owned(),
                ordinal: 1,
                divergence: Divergence::Fields {
                    kind: "retire".to_owned(),
                    fields: vec![FieldDiff {
                        name: "pc".to_owned(),
                        expected: p(4),
                        actual: ObservedValue::Absent,
                    }],
                },
            },
            auxiliary: vec![
                StreamResult::Agree {
                    stream: "effect".to_owned(),
                    compared: 2,
                    final_prefix: [0xab; 32],
                },
                StreamResult::Diverged {
                    stream: "os".to_owned(),
                    ordinal: 0,
                    divergence: Divergence::Ended {
                        ended: Role::Actual,
                        kind: "exit".to_owned(),
                    },
                },
            ],
            not_compared: vec!["arch/retire/x".to_owned()],
            failures: vec![(Role::Actual, EngineError::Unsupported("x".to_owned()))],
        })),
        window: Some(Window {
            first: 0,
            expected: vec![
                obs(0, "retire", &[("pc", p(0))]),
                obs(1, "retire", &[("pc", p(4))]),
            ],
            actual: vec![obs(0, "retire", &[("pc", p(0))])],
            failures: vec![(Role::Actual, EngineError::BackendFailed("down".to_owned()))],
        }),
        state: Some(StateEvidence {
            read_at: [2, 3],
            differences: vec![
                StateDifference {
                    component: "soc.cpu0".to_owned(),
                    field: "pc".to_owned(),
                    source: Source::Inspect,
                    change: Change::Changed {
                        expected: Value::U64(4),
                        actual: Value::U64(8),
                    },
                },
                StateDifference {
                    component: "soc.ram".to_owned(),
                    field: "page[0x0]".to_owned(),
                    source: Source::Derived("page blake3".to_owned()),
                    change: Change::Missing {
                        missing: Role::Expected,
                        value: Value::Bytes(vec![1, 2]),
                    },
                },
            ],
            first: None,
        }),
        trace: TraceEvidence {
            auxiliary: Vec::new(),
            canonical: None,
            diagnostics: vec![Diagnostic {
                role: Role::Actual,
                label: "spike log".to_owned(),
                lines: vec!["core 0: 0x0".to_owned()],
            }],
        },
    }
}

#[test]
fn the_text_rendering_is_a_view_of_the_result() {
    let r = hand_built();
    round_trip(&r);
    let prefix = "ab".repeat(32);
    let want = format!(
        "outcome: diverged
primary: arch: diverged at 1, retire: pc: expected 0x4, actual absent
auxiliary: effect: agree, 2 compared, prefix {prefix}
auxiliary: os: diverged at 0, the actual stream ended; the other has exit
not compared: arch/retire/x
failure: actual: unsupported: x
window from 0:
    expected:
      [0] retire pc=0x0
      [1] retire pc=0x4
    actual:
      [0] retire pc=0x0
    actual failed: backend failed: down
state at events 2 (expected) and 3 (actual):
  soc.cpu0 pc [inspect]: expected 0x4, actual 0x8
  soc.ram page[0x0] [derived:page blake3]: missing(expected), other bytes[2] 0102
diagnostic (actual) spike log:
  core 0: 0x0
"
    );
    assert_eq!(r.to_string(), want);
    let other = VerificationResult {
        verdict: Ok(Verdict::Incomparable(Incomparable::Stream {
            side: Role::Actual,
            stream: "os".to_owned(),
        })),
        window: None,
        state: None,
        trace: TraceEvidence::default(),
    };
    assert_eq!(other.outcome(), ResultOutcome::Incomparable);
    assert!(
        other
            .to_string()
            .starts_with("outcome: incomparable\nincomparable: ")
    );
    round_trip(&other);
}

#[test]
fn every_incomparable_and_engine_error_round_trips() {
    let side = Role::Expected;
    let mut verdicts = vec![
        Ok(Verdict::Incomparable(Incomparable::Field {
            side,
            field: "a/b/c".to_owned(),
        })),
        Ok(Verdict::Incomparable(Incomparable::AbsentRequired {
            side,
            field: "a/b/c".to_owned(),
            ordinal: 7,
        })),
    ];
    for e in [
        EngineError::BackendFailed("a".to_owned()),
        EngineError::Unsupported("b".to_owned()),
        EngineError::BoundaryDesync("c".to_owned()),
        EngineError::Internal("d".to_owned()),
    ] {
        verdicts.push(Err(e));
    }
    for verdict in verdicts {
        round_trip(&VerificationResult {
            verdict,
            window: None,
            state: None,
            trace: TraceEvidence::default(),
        });
    }
}

#[test]
fn broken_bytes_are_refused() {
    let bytes = hand_built().to_bytes();
    assert_eq!(&bytes[..4], &RESULT_MAGIC);
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert_eq!(
        VerificationResult::from_bytes(&bad),
        Err(DecodeError::InvalidTag {
            what: "result magic",
            tag: b'X'
        })
    );
    let mut bad = bytes.clone();
    bad[4] = 2;
    assert_eq!(
        VerificationResult::from_bytes(&bad),
        Err(DecodeError::InvalidTag {
            what: "result version",
            tag: 2
        })
    );
    let mut bad = bytes.clone();
    bad[8] = 3;
    assert_eq!(
        VerificationResult::from_bytes(&bad),
        Err(DecodeError::InvalidTag {
            what: "verdict",
            tag: 3
        })
    );
    let mut long = bytes.clone();
    long.push(0);
    assert!(VerificationResult::from_bytes(&long).is_err());
    for cut in 0..bytes.len() {
        assert!(
            VerificationResult::from_bytes(&bytes[..cut]).is_err(),
            "{cut}"
        );
    }
}
