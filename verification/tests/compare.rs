//! Profiles, the field-support plan, and the comparator (`docs/m4-design.md` §6.4–§6.7,
//! §10, and the M4.1 engine mutation classes of §25.2).

use systemscope_contracts::trace::Value;
use systemscope_verify::compare::{
    Comparison, Divergence, FieldDiff, Outcome, Recorded, Recording, StreamResult, Verdict,
    compare, compare_recordings, naive_first_divergence,
};
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::{Observation, ObservedValue, PrefixDigest};
use systemscope_verify::profile::{
    ComparisonProfile, FieldSpec, Incomparable, KindSpec, Plan, Requirement, Role, StreamSpec,
    Support, plan,
};

fn field(name: &str, requirement: Requirement, may_be_absent: bool) -> FieldSpec {
    FieldSpec {
        name: name.to_owned(),
        requirement,
        may_be_absent,
    }
}

/// `arch` (primary: `retire` with `pc`, `rd_value`, optional `paddr`; `exception` with
/// `cause`) and `effect` (auxiliary: `uart_tx` with `byte`).
fn profile() -> ComparisonProfile {
    ComparisonProfile {
        name: "test".to_owned(),
        version: 1,
        primary: StreamSpec {
            stream: "arch".to_owned(),
            kinds: vec![
                KindSpec {
                    kind: "exception".to_owned(),
                    fields: vec![field("cause", Requirement::Required, false)],
                    excluded: Vec::new(),
                },
                KindSpec {
                    kind: "retire".to_owned(),
                    fields: vec![
                        field("pc", Requirement::Required, false),
                        field("rd_value", Requirement::Required, true),
                        field("paddr", Requirement::Optional, false),
                    ],
                    excluded: vec!["csr".to_owned()],
                },
            ],
            window: String::new(),
        },
        auxiliary: vec![StreamSpec {
            stream: "effect".to_owned(),
            kinds: vec![KindSpec {
                kind: "uart_tx".to_owned(),
                fields: vec![field("byte", Requirement::Required, false)],
                excluded: Vec::new(),
            }],
            window: String::new(),
        }],
        stop_rule: "end".to_owned(),
        report_window: 3,
    }
}

fn full_support() -> Support {
    let mut s = Support::default();
    s.streams.extend(["arch", "effect"].map(str::to_owned));
    s.fields.extend(
        [
            "arch/exception/cause",
            "arch/retire/pc",
            "arch/retire/rd_value",
            "arch/retire/paddr",
            "effect/uart_tx/byte",
        ]
        .map(str::to_owned),
    );
    s
}

fn full_plan() -> Plan {
    plan(&profile(), &full_support(), &full_support()).unwrap()
}

fn u(v: u64) -> ObservedValue {
    ObservedValue::Present(Value::U64(v))
}

fn retire(ordinal: u64, pc: u64, rd_value: ObservedValue) -> Observation {
    Observation {
        stream: "arch".to_owned(),
        ordinal,
        kind: "retire".to_owned(),
        fields: vec![
            ("pc".to_owned(), u(pc)),
            ("rd_value".to_owned(), rd_value),
            ("paddr".to_owned(), u(pc)),
        ],
    }
}

fn exception(ordinal: u64, cause: u64) -> Observation {
    Observation {
        stream: "arch".to_owned(),
        ordinal,
        kind: "exception".to_owned(),
        fields: vec![("cause".to_owned(), u(cause))],
    }
}

fn uart(ordinal: u64, byte: u64) -> Observation {
    Observation {
        stream: "effect".to_owned(),
        ordinal,
        kind: "uart_tx".to_owned(),
        fields: vec![("byte".to_owned(), u(byte))],
    }
}

/// Five retirements: pc 0, 4, … and `rd_value` = pc + 1.
fn arch() -> Vec<Observation> {
    (0..5).map(|i| retire(i, 4 * i, u(4 * i + 1))).collect()
}

fn effect() -> Vec<Observation> {
    (0..3).map(|i| uart(i, u64::from(b'a') + i)).collect()
}

fn compared(plan: &Plan, expected: &Recorded, actual: &Recorded) -> Comparison {
    match compare(plan, expected, actual).unwrap() {
        Verdict::Compared(c) => c,
        other => panic!("{other:?}"),
    }
}

fn primary(expected: Vec<Observation>, actual: Vec<Observation>) -> StreamResult {
    let c = compared(
        &full_plan(),
        &vec![expected, effect()],
        &vec![actual, effect()],
    );
    c.primary
}

fn digest(stream: &[Observation]) -> [u8; 32] {
    let mut p = PrefixDigest::new();
    stream.iter().for_each(|o| p.absorb(o));
    p.digest()
}

#[test]
fn equal_streams_agree_with_the_full_prefix() {
    let c = compared(
        &full_plan(),
        &vec![arch(), effect()],
        &vec![arch(), effect()],
    );
    assert_eq!(c.outcome(), Outcome::Agree);
    assert_eq!(
        c.primary,
        StreamResult::Agree {
            stream: "arch".to_owned(),
            compared: 5,
            final_prefix: digest(&arch()),
        }
    );
    assert_eq!(
        c.auxiliary,
        [StreamResult::Agree {
            stream: "effect".to_owned(),
            compared: 3,
            final_prefix: digest(&effect()),
        }]
    );
    assert!(c.not_compared.is_empty());
}

#[test]
fn the_first_differing_ordinal_is_reported_at_every_position() {
    for d in 0..5 {
        let mut actual = arch();
        // Later differences too: the first one must win.
        for o in &mut actual[d..] {
            o.fields[1].1 = u(99);
        }
        let expected = arch();
        assert_eq!(naive_first_divergence(&expected, &actual), Some(d as u64));
        assert_eq!(
            primary(expected.clone(), actual.clone()),
            StreamResult::Diverged {
                stream: "arch".to_owned(),
                ordinal: d as u64,
                divergence: Divergence::Fields {
                    kind: "retire".to_owned(),
                    fields: vec![FieldDiff {
                        name: "rd_value".to_owned(),
                        expected: expected[d].fields[1].1.clone(),
                        actual: u(99),
                    }],
                },
            },
            "{d}"
        );
    }
}

#[test]
fn every_differing_field_is_listed_in_profile_order() {
    let mut actual = arch();
    actual[2] = retire(2, 100, ObservedValue::Absent);
    let StreamResult::Diverged { divergence, .. } = primary(arch(), actual) else {
        panic!()
    };
    let Divergence::Fields { fields, .. } = divergence else {
        panic!()
    };
    let names: Vec<_> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["pc", "rd_value", "paddr"]);
}

#[test]
fn a_stream_that_ends_first_diverges_at_the_missing_ordinal() {
    for (expected, actual, ended) in [
        (arch(), arch()[..3].to_vec(), Role::Actual),
        (arch()[..3].to_vec(), arch(), Role::Expected),
        (arch(), Vec::new(), Role::Actual),
    ] {
        let at = expected.len().min(actual.len()) as u64;
        assert_eq!(naive_first_divergence(&expected, &actual), Some(at));
        assert_eq!(
            primary(expected, actual),
            StreamResult::Diverged {
                stream: "arch".to_owned(),
                ordinal: at,
                divergence: Divergence::Ended {
                    ended,
                    kind: "retire".to_owned(),
                },
            }
        );
    }
}

#[test]
fn the_final_observation_is_compared() {
    let mut actual = arch();
    actual[4].fields[0].1 = u(1000);
    assert!(matches!(
        primary(arch(), actual),
        StreamResult::Diverged { ordinal: 4, .. }
    ));
}

#[test]
fn different_kinds_diverge_before_any_field() {
    let mut actual = arch();
    actual[1] = exception(1, 2);
    assert_eq!(
        primary(arch(), actual),
        StreamResult::Diverged {
            stream: "arch".to_owned(),
            ordinal: 1,
            divergence: Divergence::Kind {
                expected: "retire".to_owned(),
                actual: "exception".to_owned(),
            },
        }
    );
}

#[test]
fn absent_differs_from_zero() {
    let expected = vec![retire(0, 0, ObservedValue::Absent)];
    let actual = vec![retire(0, 0, u(0))];
    assert!(matches!(
        primary(expected.clone(), actual),
        StreamResult::Diverged { ordinal: 0, .. }
    ));
    assert!(primary(expected.clone(), expected).agrees());
}

#[test]
fn auxiliary_divergence_never_decides_the_first_divergence() {
    let mut other = effect();
    other[0].fields[0].1 = u(0);
    // Primary agrees, auxiliary diverges.
    let c = compared(
        &full_plan(),
        &vec![arch(), effect()],
        &vec![arch(), other.clone()],
    );
    assert_eq!(c.outcome(), Outcome::AuxiliaryDivergence);
    assert!(c.primary.agrees());
    assert!(matches!(
        &c.auxiliary[0],
        StreamResult::Diverged { stream, ordinal: 0, .. } if stream == "effect"
    ));
    // Both diverge: the primary decides, the auxiliary stays evidence.
    let mut late = arch();
    late[3].fields[0].1 = u(7);
    let c = compared(&full_plan(), &vec![arch(), effect()], &vec![late, other]);
    assert_eq!(c.outcome(), Outcome::Diverged);
    assert!(matches!(
        c.primary,
        StreamResult::Diverged { ordinal: 3, .. }
    ));
    assert!(!c.auxiliary[0].agrees());
}

#[test]
fn a_primary_divergence_with_agreeing_auxiliaries_is_diverged() {
    let mut actual = arch();
    actual[0].fields[0].1 = u(8);
    let c = compared(
        &full_plan(),
        &vec![arch(), effect()],
        &vec![actual, effect()],
    );
    assert_eq!(c.outcome(), Outcome::Diverged);
    assert!(c.auxiliary[0].agrees());
}

#[test]
fn an_unsupported_optional_field_is_dropped_and_listed() {
    let mut partial = full_support();
    partial.fields.remove("arch/retire/paddr");
    let p = plan(&profile(), &full_support(), &partial).unwrap();
    assert_eq!(p.not_compared, ["arch/retire/paddr"]);
    let retire_fields: Vec<_> = p.profile.primary.kinds[1]
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(retire_fields, ["pc", "rd_value"]);
    // Observations now carry only the planned fields; paddr differences are never read.
    let strip = |s: Vec<Observation>| -> Vec<Observation> {
        s.into_iter()
            .map(|mut o| {
                o.fields.retain(|(n, _)| n != "paddr");
                o
            })
            .collect()
    };
    let c = compared(
        &p,
        &vec![strip(arch()), effect()],
        &vec![strip(arch()), effect()],
    );
    assert_eq!(c.outcome(), Outcome::Agree);
    assert_eq!(c.not_compared, ["arch/retire/paddr"]);
}

#[test]
fn a_missing_stream_or_required_field_is_incomparable() {
    let mut no_effect = full_support();
    no_effect.streams.remove("effect");
    assert_eq!(
        plan(&profile(), &full_support(), &no_effect),
        Err(Incomparable::Stream {
            side: Role::Actual,
            stream: "effect".to_owned()
        })
    );
    let mut no_pc = full_support();
    no_pc.fields.remove("arch/retire/pc");
    assert_eq!(
        plan(&profile(), &no_pc, &full_support()),
        Err(Incomparable::Field {
            side: Role::Expected,
            field: "arch/retire/pc".to_owned()
        })
    );
    let mut no_arch = full_support();
    no_arch.streams.remove("arch");
    assert!(matches!(
        plan(&profile(), &no_arch, &full_support()),
        Err(Incomparable::Stream {
            side: Role::Expected,
            ..
        })
    ));
}

#[test]
fn an_absent_value_the_profile_forbids_is_incomparable() {
    let mut actual = arch();
    actual[2].fields[0].1 = ObservedValue::Absent;
    assert_eq!(
        compare(
            &full_plan(),
            &vec![arch(), effect()],
            &vec![actual, effect()]
        ),
        Ok(Verdict::Incomparable(Incomparable::AbsentRequired {
            side: Role::Actual,
            field: "arch/retire/pc".to_owned(),
            ordinal: 2,
        }))
    );
}

#[test]
fn observations_that_break_the_profile_are_engine_errors() {
    let internal = |actual: Vec<Observation>| {
        matches!(
            compare(
                &full_plan(),
                &vec![arch(), effect()],
                &vec![actual, effect()]
            ),
            Err(EngineError::Internal(_))
        )
    };
    let mut wrong_ordinal = arch();
    wrong_ordinal[1].ordinal = 2;
    let mut wrong_stream = arch();
    wrong_stream[1].stream = "os".to_owned();
    let mut unknown_kind = arch();
    unknown_kind[1].kind = "interrupt".to_owned();
    let mut extra_field = arch();
    extra_field[1].fields.push(("csr".to_owned(), u(0)));
    let mut reordered = arch();
    reordered[1].fields.swap(0, 1);
    for broken in [
        wrong_ordinal,
        wrong_stream,
        unknown_kind,
        extra_field,
        reordered,
    ] {
        assert!(internal(broken.clone()), "{broken:?}");
    }
    assert!(matches!(
        compare(&full_plan(), &vec![arch()], &vec![arch(), effect()]),
        Err(EngineError::Internal(_))
    ));
}

#[test]
fn the_profile_identity_covers_every_rule() {
    let base = profile().identity();
    let mut changes: Vec<ComparisonProfile> = Vec::new();
    let mut p = profile();
    p.name.push('x');
    changes.push(p);
    let mut p = profile();
    p.version = 2;
    changes.push(p);
    let mut p = profile();
    p.primary.stream = "os".to_owned();
    changes.push(p);
    let mut p = profile();
    p.primary.kinds[1].fields[2].requirement = Requirement::Required;
    changes.push(p);
    let mut p = profile();
    p.primary.kinds[1].fields[1].may_be_absent = false;
    changes.push(p);
    let mut p = profile();
    p.primary.kinds[1].fields.swap(0, 1);
    changes.push(p);
    let mut p = profile();
    p.primary.kinds[1].excluded.clear();
    changes.push(p);
    let mut p = profile();
    p.primary.kinds[0].kind = "interrupt".to_owned();
    changes.push(p);
    let mut p = profile();
    p.primary.window = "elf-entry".to_owned();
    changes.push(p);
    let mut p = profile();
    p.auxiliary.clear();
    changes.push(p);
    let mut p = profile();
    p.stop_rule = "shutdown".to_owned();
    changes.push(p);
    let mut p = profile();
    p.report_window = 4;
    changes.push(p);
    for (i, p) in changes.iter().enumerate() {
        assert_ne!(p.identity(), base, "change {i}");
    }
    assert_eq!(profile().identity(), base);
}

#[test]
fn the_naive_oracle_finds_no_divergence_in_equal_streams() {
    assert_eq!(naive_first_divergence(&arch(), &arch()), None);
    assert_eq!(naive_first_divergence(&[], &[]), None);
}

fn done(streams: Recorded) -> Recording {
    Recording {
        streams,
        failure: None,
    }
}

fn failed(streams: Recorded, why: &str) -> Recording {
    Recording {
        streams,
        failure: Some(EngineError::BackendFailed(why.to_owned())),
    }
}

#[test]
fn complete_recordings_compare_as_complete_streams() {
    let (e, a) = (vec![arch(), effect()], vec![arch(), effect()]);
    assert_eq!(
        compare_recordings(&full_plan(), &done(e.clone()), &done(a.clone())),
        compare(&full_plan(), &e, &a)
    );
    assert_eq!(done(e.clone()).complete(), Ok(e));
    assert_eq!(
        failed(a, "x").complete(),
        Err(EngineError::BackendFailed("x".to_owned()))
    );
}

#[test]
fn a_divergence_before_a_failure_is_still_reported() {
    let mut actual = arch();
    actual[1].fields[0].1 = u(50);
    // The actual side fails after three observations, past its divergence at 1.
    let verdict = compare_recordings(
        &full_plan(),
        &done(vec![arch(), effect()]),
        &failed(vec![actual[..3].to_vec(), Vec::new()], "budget"),
    )
    .unwrap();
    let Verdict::Compared(c) = verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!(c.outcome(), Outcome::Diverged);
    assert!(matches!(
        c.primary,
        StreamResult::Diverged { ordinal: 1, .. }
    ));
    assert!(c.auxiliary.is_empty());
    assert_eq!(
        c.failures,
        [(
            Role::Actual,
            EngineError::BackendFailed("budget".to_owned())
        )]
    );
}

#[test]
fn a_failure_with_no_divergence_before_it_is_the_result() {
    // A stream cut short by a failure makes no length divergence.
    for (e, a) in [
        (
            done(vec![arch(), effect()]),
            failed(vec![arch()[..3].to_vec()], "a"),
        ),
        (failed(vec![Vec::new()], "e"), done(vec![arch(), effect()])),
        (
            failed(vec![arch()], "e"),
            failed(vec![arch()[..2].to_vec()], "a"),
        ),
    ] {
        let first = e.failure.clone().or(a.failure.clone()).unwrap();
        assert_eq!(compare_recordings(&full_plan(), &e, &a), Err(first));
    }
    // Both failed after a divergence: both failures are evidence, expected first.
    let mut actual = arch();
    actual[0].fields[0].1 = u(50);
    let Ok(Verdict::Compared(c)) = compare_recordings(
        &full_plan(),
        &failed(vec![arch()], "e"),
        &failed(vec![actual], "a"),
    ) else {
        panic!()
    };
    let roles: Vec<Role> = c.failures.iter().map(|f| f.0).collect();
    assert_eq!(roles, [Role::Expected, Role::Actual]);
}

#[test]
fn an_incomparable_prefix_before_a_failure_is_incomparable() {
    let mut actual = arch();
    actual[1].fields[0].1 = ObservedValue::Absent;
    assert!(matches!(
        compare_recordings(
            &full_plan(),
            &done(vec![arch(), effect()]),
            &failed(vec![actual], "a"),
        ),
        Ok(Verdict::Incomparable(Incomparable::AbsentRequired {
            ordinal: 1,
            ..
        }))
    ));
}
