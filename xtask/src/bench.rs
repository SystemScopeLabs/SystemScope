//! `bench`: wall-clock throughput of the reference workloads.

use super::*;

/// A named workload that runs once and returns its events and retired instructions.
pub(crate) type Workload = (String, Box<dyn Fn() -> (u64, u64)>);

/// `bench`: the wall-clock throughput of each reference workload. Wall-clock time is read
/// here only, around whole runs, and never reaches a simulation.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "a benchmark measures wall-clock time outside the simulation"
)]
pub(crate) fn bench(runs: usize) -> ExitCode {
    use std::time::Instant;

    let root = root();
    let mut workloads: Vec<Workload> = Vec::new();
    for scenario in Scenario::ALL {
        let (firmware, disk, _) = match m3::read_fixture(&root, scenario) {
            Ok(f) => f,
            Err(e) => return report(Err(vec![format!("{}: {e}", scenario.name())]), ""),
        };
        workloads.push((
            format!("m3-reference {}", scenario.name()),
            Box::new(move || {
                let o = m3::run(&firmware, &disk, Start::Init { traced: false }, Vec::new())
                    .finished
                    .outcome;
                (o.events, o.instret)
            }),
        ));
    }
    let (firmware, disk, _) = match m5::read_fixture(&root) {
        Ok(f) => f,
        Err(e) => return report(Err(vec![format!("m5-reference: {e}")]), ""),
    };
    workloads.push((
        "m5-reference".to_owned(),
        Box::new(move || {
            let o = m5::run(&firmware, &disk, Start::Init { traced: false }, Vec::new())
                .finished
                .outcome;
            (o.events, o.instret)
        }),
    ));
    println!("| workload | events | instret | median ms | events/s | instret/s |");
    println!("|---|---:|---:|---:|---:|---:|");
    for (name, run) in &workloads {
        let mut times = Vec::with_capacity(runs);
        let mut counts = (0, 0);
        for _ in 0..runs {
            let start = Instant::now();
            counts = run();
            times.push(start.elapsed());
        }
        times.sort();
        // Integer microseconds keep the workspace's no-float rule.
        let median = times[runs / 2].as_micros().max(1);
        let (events, instret) = counts;
        let per_second = |n: u64| u128::from(n) * 1_000_000 / median;
        println!(
            "| {name} | {events} | {instret} | {}.{:03} | {} | {} |",
            median / 1000,
            median % 1000,
            per_second(events),
            per_second(instret)
        );
    }
    ExitCode::SUCCESS
}
