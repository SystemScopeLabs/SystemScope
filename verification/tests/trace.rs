//! Trace evidence on the counter (`docs/m4-design.md` §14): the first differing canonical
//! trace record against a naive full capture, auxiliary windows, and diagnostics.

mod common;

use common::*;
use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_verify::backend::{Backend, run};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{StreamResult, Verdict, compare};
use systemscope_verify::error::EngineError;
use systemscope_verify::latent::SameSide;
use systemscope_verify::locate::{Index, record_index};
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::runtime::RuntimeBackend;
use systemscope_verify::trace::{
    Diagnostic, TraceDivergence, TraceLine, auxiliary_windows, first_trace_divergence,
};

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

fn index(input: &InputVariant, every: u64) -> Index {
    let mut b = backend();
    let side = side(&mut b, input.clone());
    record_index(&the_plan(), &scenario(), side, Role::Expected, every).unwrap()
}

fn same<'a>(
    backend: &'a mut RuntimeBackend,
    input: &InputVariant,
    index: &'a Index,
) -> SameSide<'a> {
    SameSide {
        backend,
        input: input.clone(),
        config: config(),
        checkpoints: &index.checkpoints,
    }
}

fn scan(input: &InputVariant, every: u64, window: u64, until: u64) -> TraceDivergence {
    let (ie, ia) = (index(&InputVariant::None, every), index(input, every));
    let (mut x, mut y) = (backend(), backend());
    first_trace_divergence(
        &the_plan(),
        &scenario(),
        same(&mut x, &InputVariant::None, &ie),
        same(&mut y, input, &ia),
        window,
        until,
    )
    .unwrap()
}

/// Every record a side emits after `init` until its last event, with the events
/// dispatched once each was emitted.
fn whole(input: &InputVariant) -> Vec<(u64, TraceRecord)> {
    let mut b = backend();
    b.start(&scenario(), input, &config(), &profile()).unwrap();
    b.capture().unwrap();
    let mut out = Vec::new();
    while b.step_event().is_ok() {
        let at = b.events().unwrap();
        out.extend(b.captured().into_iter().map(|r| (at, r)));
    }
    out
}

fn lines(records: &[(u64, TraceRecord)]) -> Vec<TraceLine> {
    records
        .iter()
        .map(|(_, r)| TraceLine {
            at: r.at,
            origin: r.origin,
            component: "counter".to_owned(),
            kind: r.kind.to_owned(),
            fields: r
                .fields
                .iter()
                .map(|(n, v)| ((*n).to_owned(), v.clone()))
                .collect(),
        })
        .collect()
}

#[test]
fn the_first_differing_record_matches_a_full_capture() {
    for k in [1, 4, 7] {
        let input = patch_n(k, k, k + 1);
        let (e, a) = (whole(&InputVariant::None), whole(&input));
        let d = e.iter().zip(&a).position(|(x, y)| x.1 != y.1).unwrap();
        for (every, window) in [(0, 3), (1, 3), (2, 0), (0, 100)] {
            let found = scan(&input, every, window, LIMIT);
            // Records after the origin: the full capture's, less those before it.
            let skipped = e.iter().take_while(|(at, _)| *at <= found.origin).count();
            assert_eq!(found.event, Some(e[d].0), "{k} {every}");
            assert_eq!(found.record, (d - skipped) as u64, "{k} {every}");
            let first = d.saturating_sub(window as usize).max(skipped);
            assert_eq!(found.first, (first - skipped) as u64, "{k} {every}");
            let end = |s: &[(u64, TraceRecord)]| (d + 1 + window as usize).min(s.len());
            assert_eq!(found.expected, lines(&e[first..end(&e)]), "{k} {every}");
            assert_eq!(found.actual, lines(&a[first..end(&a)]), "{k} {every}");
            assert_ne!(found.expected[d - first], found.actual[d - first]);
        }
        // The checkpoint after event k holds the patch: the origin is the one before.
        assert_eq!(scan(&input, 1, 3, LIMIT).origin, k - 1);
    }
}

#[test]
fn equal_traces_and_a_short_scan_find_nothing() {
    for (input, until) in [(InputVariant::None, LIMIT - 1), (patch_n(6, 6, 7), 6)] {
        let found = scan(&input, 2, 3, until);
        assert_eq!(
            (found.event, &found.expected[..], &found.actual[..]),
            (None, &[][..], &[][..]),
            "{until}"
        );
        assert_eq!(found.first, found.record);
    }
}

#[test]
fn trace_lines_and_diagnostics_round_trip() {
    let mut records = whole(&patch_n(4, 4, 5));
    records.push((
        0,
        TraceRecord {
            at: TraceAt::Init,
            origin: TraceOrigin::Component,
            component: ComponentId(0),
            kind: "t.init",
            fields: vec![("s", Value::Str("x".to_owned()))],
        },
    ));
    assert!(
        records
            .iter()
            .any(|(_, r)| r.origin == TraceOrigin::Runtime)
    );
    for line in lines(&records) {
        let mut e = Encoder::new();
        line.encode(&mut e);
        let bytes = e.into_bytes();
        let mut d = Decoder::new(&bytes);
        assert_eq!(TraceLine::decode(&mut d), Ok(line));
        assert_eq!(d.remaining(), 0);
    }
    for (bytes, what, tag) in [(&[2][..], "at", 2), (&[0, 2][..], "origin", 2)] {
        assert_eq!(
            TraceLine::decode(&mut Decoder::new(bytes)),
            Err(DecodeError::InvalidTag { what, tag })
        );
    }
    for d in [
        Diagnostic {
            role: Role::Actual,
            label: "spike log".to_owned(),
            lines: vec!["core 0: 0x80000000".to_owned(), String::new()],
        },
        Diagnostic {
            role: Role::Expected,
            label: String::new(),
            lines: Vec::new(),
        },
    ] {
        let mut e = Encoder::new();
        d.encode(&mut e);
        let bytes = e.into_bytes();
        let mut r = Decoder::new(&bytes);
        assert_eq!(Diagnostic::decode(&mut r), Ok(d));
        assert_eq!(r.remaining(), 0);
    }
    assert_eq!(
        Diagnostic::decode(&mut Decoder::new(&[2])),
        Err(DecodeError::InvalidTag {
            what: "role",
            tag: 2
        })
    );
}

#[test]
fn auxiliary_windows_surround_each_auxiliary_divergence() {
    let plan = the_plan();
    let record = |input| {
        let mut b = backend();
        run(&plan, &scenario(), side(&mut b, input))
            .complete()
            .unwrap()
    };
    let expected = record(InputVariant::None);
    // The patch skips n = 4, so the effect stream lacks its first byte.
    let actual = record(patch_n(3, 3, 4));
    let Verdict::Compared(c) = compare(&plan, &expected, &actual).unwrap() else {
        panic!()
    };
    let windows = auxiliary_windows(&plan, &c, &expected, &actual);
    let w = u64::from(plan.profile.report_window);
    let diverged: Vec<(String, u64)> = c
        .auxiliary
        .iter()
        .filter_map(|r| match r {
            StreamResult::Diverged {
                stream, ordinal, ..
            } => Some((stream.clone(), *ordinal)),
            StreamResult::Agree { .. } => None,
        })
        .collect();
    assert!(!diverged.is_empty());
    assert_eq!(windows.len(), diverged.len());
    for ((stream, window), (s, ordinal)) in windows.iter().zip(&diverged) {
        assert_eq!(stream, s);
        let i = plan.profile.streams().position(|x| &x.stream == s).unwrap();
        let first = ordinal.saturating_sub(w);
        assert_eq!(window.first, first);
        for (kept, all) in [
            (&window.expected, &expected[i]),
            (&window.actual, &actual[i]),
        ] {
            let end = (ordinal + w + 1).min(all.len() as u64) as usize;
            assert_eq!(kept[..], all[first as usize..end]);
        }
        assert!(window.failures.is_empty());
    }
    // Agreeing streams have no window.
    let Verdict::Compared(c) = compare(&plan, &expected, &expected).unwrap() else {
        panic!()
    };
    assert_eq!(auxiliary_windows(&plan, &c, &expected, &expected), []);
}

#[test]
fn capture_needs_a_started_side() {
    let mut b = backend();
    assert_eq!(
        b.capture(),
        Err(EngineError::Internal("capture before start".to_owned()))
    );
    assert_eq!(b.captured(), []);
}
