//! M6.2's feasibility gate on `m5-reference` (`docs/m6-design.md` §9.7): the choice
//! points of the reference run under the empty script. A measurement, run on demand with
//! `--ignored`; its numbers are recorded in the design.

use systemscope_acceptance::m5::Fixture;
use systemscope_acceptance::m5::verify::{JITTER_LINKS, platform_config};
use systemscope_runtime::choice::{Choice, Recording, choice_points};
use systemscope_runtime::jitter::LinkSet;
use systemscope_runtime::script::{EventId, Script};
use systemscope_rv32::m5ref;
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;

#[test]
#[ignore = "a measurement, minutes long"]
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "a measurement reads wall-clock time around the whole run, outside it"
)]
fn measure_the_reference_choice_points() {
    use std::time::Instant;

    let f = Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"));
    let mut config = platform_config(&Perturbation::default());
    config.m3.script = Some(Script::default());
    let mut rt = m5ref::build(&f.firmware, &f.disk, &config).unwrap();
    let started = Instant::now();
    rt.init().unwrap();
    let mut rec = Recording::after_init(&rt);
    while rec.step(&mut rt).unwrap().is_some() {}
    let ran = started.elapsed();
    // Snapshot sizes through the run, from a second run so the timing above is clean.
    let mut rt2 = m5ref::build(&f.firmware, &f.disk, &config).unwrap();
    rt2.init().unwrap();
    let mut sizes = Vec::new();
    let mut n = 0u64;
    loop {
        if n.is_multiple_of(100_000) {
            sizes.push(rt2.snapshot().unwrap().len());
        }
        if rt2.step().unwrap().is_none() {
            break;
        }
        n += 1;
    }
    drop(rt2);
    let snapshot = rt.snapshot().unwrap().len();
    let points = choice_points(&rec, LinkSet::of(&JITTER_LINKS), &[], None).unwrap();
    let mut delays = [0u64; 14];
    let mut offered = std::collections::BTreeMap::<usize, u64>::new();
    let paths: Vec<String> = rt.component_paths().map(str::to_owned).collect();
    let mut at = std::collections::BTreeMap::new();
    for s in &rec.steps {
        at.insert(s.event.id.unwrap(), s.event.target.0 as usize);
    }
    let what = |id: EventId| {
        let kind = match id {
            EventId::Msg { link, .. } => format!("msg l{link}"),
            EventId::Wake { .. } => "wake".to_owned(),
        };
        format!("{kind} -> {}", paths[at[&id]])
    };
    let mut pairs = std::collections::BTreeMap::<(String, String), u64>::new();
    let (mut dispatches, mut picks) = (0u64, 0u64);
    for p in &points {
        match &p.choice {
            Choice::Delay((link, _, _)) => delays[*link as usize] += 1,
            Choice::Shift(_) => {}
            Choice::Pick { default, ready } => {
                for &r in ready {
                    *pairs.entry((what(*default), what(r))).or_default() += 1;
                }
                dispatches += 1;
                picks += ready.len() as u64;
                *offered.entry(ready.len()).or_default() += 1;
            }
        }
    }
    println!("events {} in {ran:?}", rec.steps.len());
    println!("final snapshot {snapshot} bytes");
    println!("scoped messages by link {delays:?}");
    println!("dispatches with a pick {dispatches}, picks {picks}, by count {offered:?}");
    for ((default, ready), n) in &pairs {
        println!("  {n:>6}  default {default}, ready {ready}");
    }
    // Groups holding a component's self-wake and a message to it.
    let mut both = std::collections::BTreeMap::<String, u64>::new();
    let steps = &rec.steps;
    let mut i = 0;
    while i < steps.len() {
        let key = steps[i].event.key;
        let mut j = i;
        while j < steps.len()
            && (steps[j].event.key.tick, steps[j].event.key.phase) == (key.tick, key.phase)
        {
            j += 1;
        }
        let mut seen = std::collections::BTreeMap::<usize, (bool, bool)>::new();
        for s in &steps[i..j] {
            let e = seen.entry(s.event.target.0 as usize).or_default();
            match s.event.id.unwrap() {
                EventId::Wake { .. } => e.0 = true,
                EventId::Msg { .. } => e.1 = true,
            }
        }
        for (c, (w, m)) in seen {
            if w && m {
                *both
                    .entry(format!("{} {:?}", paths[c], key.phase))
                    .or_default() += 1;
            }
        }
        i = j;
    }
    println!("groups with a self-wake and a message to one component {both:?}");

    // A scoped message is clamped by its lane if it runs at the tick of the lane's
    // previous message; otherwise its base tick is its tick, and every delay moves it to
    // a distinct later tick.
    let mut tick = std::collections::HashMap::new();
    for s in steps {
        tick.insert(s.event.id.unwrap(), s.event.key.tick);
    }
    let mut clamped = [0u64; 14];
    let mut unclamped = [0u64; 14];
    for p in &points {
        if let Choice::Delay((link, dir, ordinal)) = p.choice {
            let me = tick[&EventId::Msg { link, dir, ordinal }];
            let previous = ordinal.checked_sub(1).map(|o| {
                tick[&EventId::Msg {
                    link,
                    dir,
                    ordinal: o,
                }]
            });
            if previous == Some(me) {
                clamped[link as usize] += 1;
            } else {
                unclamped[link as usize] += 1;
            }
        }
    }
    println!("scoped messages clamped by link {clamped:?}");
    println!("scoped messages unclamped by link {unclamped:?}");

    // The timer's wakes: how many cycles ahead each was scheduled, from the cycle edge
    // at or after its scheduling tick. A shift of -s with s above that is raised.
    let shifts = choice_points(&rec, LinkSet::of(&[]), &[m5ref::TIMER.0], None).unwrap();
    let mut ahead = std::collections::BTreeMap::<u64, u64>::new();
    let (mut wakes, mut wake_after) = (0u64, 0u64);
    for p in &shifts {
        if let Choice::Shift((component, ordinal)) = p.choice {
            let at = p.branch.at;
            wakes += 1;
            wake_after += steps.len() as u64 - at;
            let from = if at == 0 {
                0
            } else {
                steps[at as usize - 1].event.key.tick.0
            };
            let edge = from.div_ceil(1000) * 1000;
            *ahead
                .entry((tick[&EventId::Wake { component, ordinal }].0 - edge) / 1000)
                .or_default() += 1;
        }
    }
    println!("timer wakes {wakes}, events after: sum {wake_after}, by cycles ahead {ahead:?}");

    // Events after each branch point, by kind.
    let total = steps.len() as u64;
    for kind in 0..2u8 {
        let after: Vec<u64> = points
            .iter()
            .filter(|p| p.branch.kind == kind)
            .map(|p| total - p.branch.at)
            .collect();
        let sum: u64 = after.iter().sum();
        println!(
            "kind {kind}: {} points, events after: sum {sum}, mean {}, min {:?}, max {:?}",
            after.len(),
            sum.checked_div(after.len() as u64).unwrap_or(0),
            after.iter().min(),
            after.iter().max(),
        );
    }
    println!(
        "snapshot bytes every 100000 events: min {:?}, max {:?}, all {sizes:?}",
        sizes.iter().min(),
        sizes.iter().max()
    );

    // The full trace of the run.
    let mut rt = m5ref::build(&f.firmware, &f.disk, &config).unwrap();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    let trace = rt.take_trace().unwrap();
    println!(
        "trace: {} records, {} canonical bytes",
        trace.records.len(),
        trace.canonical_bytes().len()
    );
}
