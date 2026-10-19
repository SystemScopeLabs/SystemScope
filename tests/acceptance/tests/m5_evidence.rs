//! M5.4b on `m5-reference` (`docs/m5-design.md` §7.4, §15): schedule evidence. The
//! jitter recomputed from the dispatch records matches the runtime's own lanes; the
//! evidence's first differing decision is the engine's `schedule` divergence; a process's
//! shared-page reads come from its last dispatch; and the state decoder reads the
//! shutdown's words from RAM.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::evidence::{
    applied_jitter, decode_state, schedule_evidence, shared_reads, traced,
};
use systemscope_acceptance::m5::verify::{self, JITTER_LINKS, OUTCOME, outcome_profile};
use systemscope_contracts::canonical::Decoder;
use systemscope_contracts::trace::Value;
use systemscope_runtime::jitter::{Jitter, JitterState, Lane, LinkSet};
use systemscope_runtime::trace::Trace;
use systemscope_rv32::m5::expected_final;
use systemscope_rv32::m5ref::{SHARED, SHARED_VA};
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{StreamResult, Verdict, compare};
use systemscope_verify::profile::plan;
use systemscope_verify::state::Source;

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

fn jitter(seed: u64, max_cycles: u64) -> Perturbation {
    Perturbation {
        jitter: Some(Jitter {
            seed,
            max_cycles,
            links: LinkSet::of(&JITTER_LINKS),
            only: None,
        }),
        ..Perturbation::default()
    }
}

fn run_traced(fixture: &Fixture, p: &Perturbation) -> (Trace, Vec<u8>) {
    traced(fixture, p).unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

/// The jitter lanes of a jittered snapshot, read from its header.
fn lanes(snapshot: &[u8]) -> (Jitter, Vec<[Lane; 2]>) {
    let mut d = Decoder::new(snapshot);
    assert_eq!(d.raw(8).unwrap(), b"SSSNAP\0\0");
    assert_eq!(d.u32().unwrap(), 2, "a jittered snapshot");
    for _ in 0..3 {
        d.u64().unwrap();
    }
    d.str().unwrap();
    for _ in 0..d.len().unwrap() {
        d.raw(4 + 3 * 8 + 1).unwrap();
    }
    d.raw(32).unwrap();
    JitterState::decode(&mut d).unwrap()
}

#[test]
fn the_recomputed_jitter_matches_the_runtime_lanes() {
    let fixture = fixture();
    for p in [jitter(1, 4), jitter(0x5eed, 8)] {
        let setting = p.jitter.clone().unwrap();
        let (trace, snapshot) = run_traced(&fixture, &p);
        let (delivered, applied) = applied_jitter(&trace, &setting, u64::MAX).unwrap();
        let (found, lanes) = lanes(&snapshot);
        assert_eq!(found, setting);
        // Every message sent on a jittered link was delivered by the end, one per
        // ordinal, so the recomputed count is the lane's.
        for (link, lane) in setting.links.iter().zip(&lanes) {
            for dir in 0..2u8 {
                let sent = lane[usize::from(dir)].sent;
                assert_eq!(delivered.get(&(link, dir)).copied().unwrap_or(0), sent);
            }
        }
        assert!(!applied.is_empty());
        for a in &applied {
            assert!(setting.links.contains(a.link), "{a:?}");
            assert!((1..=setting.max_cycles).contains(&a.cycles), "{a:?}");
            assert_eq!(a.cycles, setting.draw(a.link, a.dir, a.ordinal));
        }
        assert!(applied.is_sorted_by_key(|a| a.event));
        // Up to an event, the recompute is the prefix.
        let half = applied[applied.len() / 2].event;
        let (_, prefix) = applied_jitter(&trace, &setting, half).unwrap();
        assert_eq!(prefix[..], applied[..prefix.len()]);
        assert!(prefix.iter().all(|a| a.event <= half));
    }
}

#[test]
fn the_evidence_names_the_engines_schedule_divergence() {
    let fixture = fixture();
    let none = Perturbation::default();
    let perturbed = jitter(1, 4);
    // The engine's comparison, through the M4 backend.
    let profile = outcome_profile();
    let record = |p: &Perturbation| {
        let mut b = verify::backend(&fixture, p);
        let plan = plan(&profile, &b.support(), &b.support()).unwrap();
        let side = Side {
            backend: &mut b,
            input: InputVariant::None,
            config: verify::config(),
        };
        let streams = run(&plan, &verify::scenario(&fixture), side)
            .complete()
            .unwrap();
        (plan, streams)
    };
    let (plan, a) = record(&none);
    let (_, b) = record(&perturbed);
    let Verdict::Compared(c) = compare(&plan, &a, &b).unwrap() else {
        panic!("not compared");
    };
    let StreamResult::Diverged { ordinal, .. } = &c.auxiliary[2] else {
        panic!("{:?}", c.auxiliary[2]);
    };
    // The evidence, from the traces.
    let (t0, _) = run_traced(&fixture, &none);
    let (t1, _) = run_traced(&fixture, &perturbed);
    assert_eq!(schedule_evidence([(&none, &t0), (&none, &t0)], 2), Ok(None));
    let e = schedule_evidence([(&none, &t0), (&perturbed, &t1)], 2)
        .unwrap()
        .expect("the schedules differ");
    assert_eq!(e.ordinal as u64, *ordinal);
    let [x, y] = &e.sides;
    assert_eq!(x.identity, none.identity());
    assert_eq!(y.identity, perturbed.identity());
    let at = |s: &systemscope_acceptance::m5::evidence::SideEvidence| {
        let from = e.ordinal.saturating_sub(2);
        s.around[e.ordinal - from].clone()
    };
    assert_ne!(at(x).boundary, at(y).boundary);
    assert_eq!(Some(at(x).event), x.event);
    assert_eq!(Some(at(y).event), y.event);
    assert!(x.jitter.is_empty());
    assert!(!y.jitter.is_empty());
    assert!(y.jitter.iter().all(|a| a.event <= y.event.unwrap()));
    // The process preempted at the first differing decision read the shared page during
    // its dispatch, and those reads are in the evidence's reach.
    let pid = 1;
    let reads = shared_reads(&t0, pid, x.event.unwrap()).unwrap();
    assert!(!reads.is_empty());
    for r in &reads {
        assert_eq!(r.paddr & !0xfff, u64::from(SHARED), "{r:?}");
        assert_eq!(r.va & !0xfff, u64::from(SHARED_VA), "{r:?}");
        assert!(r.event <= x.event.unwrap());
    }
    assert!(reads.is_sorted_by_key(|r| r.event));
    assert!(shared_reads(&t0, 3, x.event.unwrap()).is_err());
}

#[test]
fn the_state_decoder_reads_the_shutdowns_words() {
    let fixture = fixture();
    let words = |fields: &[systemscope_verify::state::StateField]| -> Vec<u64> {
        fields
            .iter()
            .filter(|f| f.field.starts_with("shared["))
            .map(|f| {
                assert_eq!(f.source, Source::Decoder("m5-ram/1".to_owned()));
                match f.value {
                    Value::U64(v) => v,
                    ref other => panic!("{other:?}"),
                }
            })
            .collect()
    };
    for p in [Perturbation::default(), jitter(0x5eed, 8)] {
        let (_, snapshot) = run_traced(&fixture, &p);
        let fields = decode_state(&snapshot).unwrap();
        assert_eq!(words(&fields), expected_final());
        assert!(fields.iter().any(|f| f.field.starts_with("page[")));
    }
    // Through the engine's state read, after the run.
    let mut b = verify::backend(&fixture, &Perturbation::default());
    b.start(
        &verify::scenario(&fixture),
        &InputVariant::None,
        &verify::config(),
        &outcome_profile(),
    )
    .unwrap();
    while b.next(OUTCOME).unwrap().is_some() {}
    assert_eq!(words(&b.state().unwrap()), expected_final());
    assert!(decode_state(b"not a snapshot").is_err());
}
