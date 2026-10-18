//! Structured state diffs (`docs/m4-design.md` §13.4).

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::trace::Value;
use systemscope_verify::observation::{Observation, ObservedValue};
use systemscope_verify::profile::Role;
use systemscope_verify::state::{
    Change, Source, StateDifference, StateField, arch_registers, diff_state,
};

fn f(component: &str, field: &str, v: u64, source: Source) -> StateField {
    StateField {
        component: component.to_owned(),
        field: field.to_owned(),
        value: Value::U64(v),
        source,
    }
}

fn i(component: &str, field: &str, v: u64) -> StateField {
    f(component, field, v, Source::Inspect)
}

fn changed(component: &str, field: &str, e: u64, a: u64) -> StateDifference {
    StateDifference {
        component: component.to_owned(),
        field: field.to_owned(),
        source: Source::Inspect,
        change: Change::Changed {
            expected: Value::U64(e),
            actual: Value::U64(a),
        },
    }
}

fn missing(component: &str, field: &str, missing: Role, v: u64) -> StateDifference {
    StateDifference {
        component: component.to_owned(),
        field: field.to_owned(),
        source: Source::Inspect,
        change: Change::Missing {
            missing,
            value: Value::U64(v),
        },
    }
}

#[test]
fn equal_states_have_no_difference() {
    let s = vec![i("cpu", "pc", 4), i("cpu", "x1", 0), i("bus", "busy", 1)];
    assert_eq!(diff_state(&s, &s), []);
    assert_eq!(diff_state(&[], &[]), []);
}

#[test]
fn differences_are_ordered_by_component_then_field() {
    let e = vec![
        i("cpu", "pc", 4),
        i("cpu", "x1", 1),
        i("cpu", "x2", 2),
        i("bus", "busy", 0),
    ];
    // The actual side reports its fields in another order, lacks x1, and adds x3 and a
    // component of its own.
    let a = vec![
        i("dma", "len", 9),
        i("bus", "busy", 1),
        i("cpu", "x3", 3),
        i("cpu", "x2", 7),
        i("cpu", "pc", 4),
    ];
    assert_eq!(
        diff_state(&e, &a),
        [
            missing("cpu", "x1", Role::Actual, 1),
            changed("cpu", "x2", 2, 7),
            missing("cpu", "x3", Role::Expected, 3),
            changed("bus", "busy", 0, 1),
            missing("dma", "len", Role::Expected, 9),
        ]
    );
}

#[test]
fn a_field_is_identified_by_component_source_and_name() {
    // The same name in another component or from another source is another field.
    let e = vec![
        i("cpu", "pc", 4),
        f("cpu", "pc", 4, Source::Decoder("rv32.cpu/3".to_owned())),
    ];
    let a = vec![
        i("hart1", "pc", 4),
        f("cpu", "pc", 8, Source::Decoder("rv32.cpu/3".to_owned())),
    ];
    let d = diff_state(&e, &a);
    assert_eq!(d.len(), 3, "{d:?}");
    assert_eq!(d[0], missing("cpu", "pc", Role::Actual, 4));
    assert_eq!(
        (&d[1].source, &d[1].change),
        (
            &Source::Decoder("rv32.cpu/3".to_owned()),
            &Change::Changed {
                expected: Value::U64(4),
                actual: Value::U64(8),
            }
        )
    );
    assert_eq!(d[2], missing("hart1", "pc", Role::Expected, 4));
    // A repeated name is matched by occurrence.
    let e = vec![i("ram", "page", 1), i("ram", "page", 2)];
    let a = vec![
        i("ram", "page", 1),
        i("ram", "page", 3),
        i("ram", "page", 4),
    ];
    assert_eq!(
        diff_state(&e, &a),
        [
            changed("ram", "page", 2, 3),
            missing("ram", "page", Role::Expected, 4),
        ]
    );
}

#[test]
fn state_fields_and_differences_round_trip() {
    let fields = [
        i("cpu", "pc", 4),
        StateField {
            component: "kernel".to_owned(),
            field: "procs[1].name".to_owned(),
            value: Value::Str("hello".to_owned()),
            source: Source::Decoder("os.kernel/1".to_owned()),
        },
        f("cpu", "x", 1, Source::Derived("arch replay".to_owned())),
    ];
    for field in &fields {
        let mut e = Encoder::new();
        field.encode(&mut e);
        let bytes = e.into_bytes();
        let mut d = Decoder::new(&bytes);
        assert_eq!(StateField::decode(&mut d).as_ref(), Ok(field));
        assert_eq!(d.remaining(), 0);
    }
    for diff in [
        changed("cpu", "pc", 4, 8),
        missing("cpu", "x1", Role::Actual, 1),
        missing("cpu", "x1", Role::Expected, 1),
    ] {
        let mut e = Encoder::new();
        diff.encode(&mut e);
        let bytes = e.into_bytes();
        let mut d = Decoder::new(&bytes);
        assert_eq!(StateDifference::decode(&mut d).as_ref(), Ok(&diff));
        assert_eq!(d.remaining(), 0);
    }
    let bad_source = {
        let mut e = Encoder::new();
        e.str("cpu");
        e.str("pc");
        Value::U64(1).encode(&mut e);
        e.u8(3);
        e.into_bytes()
    };
    assert_eq!(
        StateField::decode(&mut Decoder::new(&bad_source)),
        Err(DecodeError::InvalidTag {
            what: "source",
            tag: 3
        })
    );
    let bad = |tail: &[u8]| {
        let mut e = Encoder::new();
        e.str("cpu");
        e.str("pc");
        e.u8(0);
        let mut bytes = e.into_bytes();
        bytes.extend(tail);
        StateDifference::decode(&mut Decoder::new(&bytes))
    };
    assert_eq!(
        bad(&[2]),
        Err(DecodeError::InvalidTag {
            what: "change",
            tag: 2
        })
    );
    assert_eq!(
        bad(&[1, 2]),
        Err(DecodeError::InvalidTag {
            what: "role",
            tag: 2
        })
    );
    assert_eq!(Source::Inspect.to_string(), "inspect");
    assert_eq!(Source::Decoder("a/1".into()).to_string(), "decoder:a/1");
    assert_eq!(Source::Derived("b".into()).to_string(), "derived:b");
}

#[test]
fn registers_are_replayed_from_retirements() {
    let obs = |kind: &str, rd: Option<u64>, rd_value: Option<u64>| Observation {
        stream: "arch".to_owned(),
        ordinal: 0,
        kind: kind.to_owned(),
        fields: [("rd", rd), ("rd_value", rd_value)]
            .into_iter()
            .map(|(n, v)| {
                let v = v.map_or(ObservedValue::Absent, |v| {
                    ObservedValue::Present(Value::U64(v))
                });
                (n.to_owned(), v)
            })
            .collect(),
    };
    let arch = [
        obs("retire", Some(5), Some(1)),
        obs("retire", Some(2), Some(7)),
        // Not a retirement, x0, an out-of-range register, and absent values write
        // nothing.
        obs("exception", Some(2), Some(9)),
        obs("retire", Some(0), Some(9)),
        obs("retire", Some(32), Some(9)),
        obs("retire", None, Some(9)),
        obs("retire", Some(2), None),
        obs("retire", Some(5), Some(3)),
    ];
    let derived =
        |field: &str, v: u64| f("arch", field, v, Source::Derived("arch replay".to_owned()));
    assert_eq!(arch_registers(&arch), [derived("x2", 7), derived("x5", 3)]);
    assert_eq!(arch_registers(&[]), []);
}
