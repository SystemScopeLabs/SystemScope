//! M5.1 and M5.6a: perturbations A, B, and C on the frozen reference platforms
//! (`docs/m5-design.md` §7.1–§7.3, §21).
//!
//! The M2 and M3 reference workloads are race-free, so their results must not depend on
//! timing: under every latency override, jitter seed, and tie seed here they end the same way,
//! print the same bytes, and leave the same disk, while the schedule itself changes. A
//! perturbed run is as deterministic and as restorable as a frozen one.

use systemscope_runtime::jitter::{Jitter, LinkSet};
use systemscope_runtime::runtime::Runtime;
use systemscope_runtime::script::Script;
use systemscope_runtime::ties::Ties;
use systemscope_rv32::block_irq;
use systemscope_rv32::m2ref;
use systemscope_rv32::m3::{self, M3Run, Scenario};
use systemscope_rv32::m3ref;
use systemscope_rv32::perturb::{Latencies, MAX_CYCLES};
use systemscope_rv32::runner::{Outcome, Start};
use systemscope_rv32::workspace_root;

fn all(links: u32) -> LinkSet {
    LinkSet::of(&(0..links).collect::<Vec<_>>())
}

fn jitter(seed: u64, max_cycles: u64, links: u32) -> Option<Jitter> {
    Some(Jitter {
        seed,
        max_cycles,
        links: all(links),
        only: None,
    })
}

fn digests(outcome: &Outcome) -> (Option<[u8; 32]>, [u8; 32], u64) {
    (outcome.state, outcome.execution, outcome.events)
}

fn m2_run(config: &m2ref::Config) -> block_irq::BlockIrqRun {
    let (image, disk) = block_irq::read_fixture(&workspace_root()).unwrap();
    block_irq::run_with(
        &image,
        &disk,
        config,
        Start::Init { traced: false },
        Vec::new(),
    )
}

fn m3_run(config: &m3ref::Config, start: Start) -> M3Run {
    let (firmware, disk, _) = m3::read_fixture(&workspace_root(), Scenario::Reference).unwrap();
    m3::run_with(&firmware, &disk, config, start, Vec::new())
}

fn m3_platform(config: &m3ref::Config) -> Runtime {
    let (firmware, disk, _) = m3::read_fixture(&workspace_root(), Scenario::Reference).unwrap();
    m3ref::build(&firmware, &disk, config).unwrap()
}

fn traced() -> Start {
    Start::Init { traced: true }
}

#[test]
fn m2_reference_passes_under_jitter_on_every_link() {
    let frozen = m2_run(&m2ref::Config::frozen());
    block_irq::judge(&frozen).unwrap();
    let mut moved = 0;
    for (seed, max_cycles) in [(1, 1), (2, 2), (3, 3), (4, 4), (0x5eed, 8)] {
        let config = m2ref::Config {
            jitter: jitter(seed, max_cycles, m2ref::LINKS),
            ..m2ref::Config::frozen()
        };
        let run = m2_run(&config);
        block_irq::judge(&run).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(run.output, frozen.output, "seed {seed}");
        let again = m2_run(&config);
        assert_eq!(
            digests(&run.finished.finished.outcome),
            digests(&again.finished.finished.outcome),
            "seed {seed} is deterministic"
        );
        if run.finished.finished.outcome.execution != frozen.finished.finished.outcome.execution {
            moved += 1;
        }
    }
    assert_eq!(moved, 5, "every seed changes the schedule");
}

#[test]
fn m2_reference_passes_under_latency_overrides() {
    let frozen = m2_run(&m2ref::Config::frozen());
    let sweeps = [
        Latencies {
            links: (0..m2ref::LINKS).map(|i| (i, 3)).collect(),
            ..Latencies::default()
        },
        Latencies {
            respond: m2ref::TARGETS
                .iter()
                .map(|p| ((*p).to_owned(), 5))
                .collect(),
            ..Latencies::default()
        },
        Latencies {
            links: [(0, MAX_CYCLES), (4, 2)].into_iter().collect(),
            respond: [
                ("soc.disk".to_owned(), 0),
                ("soc.ram".to_owned(), MAX_CYCLES),
            ]
            .into_iter()
            .collect(),
        },
    ];
    for (k, latencies) in sweeps.into_iter().enumerate() {
        let config = m2ref::Config {
            latencies,
            ..m2ref::Config::frozen()
        };
        let run = m2_run(&config);
        block_irq::judge(&run).unwrap_or_else(|e| panic!("sweep {k}: {e}"));
        assert_ne!(
            run.finished.finished.outcome.execution, frozen.finished.finished.outcome.execution,
            "sweep {k} changes the schedule"
        );
    }
}

#[test]
fn the_identity_override_keeps_the_frozen_schedule() {
    let frozen = m2_run(&m2ref::Config::frozen());
    let identity = m2ref::Config {
        latencies: Latencies {
            links: (0..m2ref::LINKS).map(|i| (i, 1)).collect(),
            respond: [("soc.ram".to_owned(), 0)].into_iter().collect(),
        },
        ..m2ref::Config::frozen()
    };
    let run = m2_run(&identity);
    assert_eq!(
        digests(&run.finished.finished.outcome),
        digests(&frozen.finished.finished.outcome)
    );
}

#[test]
fn builders_reject_invalid_overrides() {
    let (image, disk) = block_irq::read_fixture(&workspace_root()).unwrap();
    let (firmware, m3_disk, _) = m3::read_fixture(&workspace_root(), Scenario::Reference).unwrap();
    let cases: [(Latencies, &str); 5] = [
        (
            Latencies {
                links: [(m3ref::LINKS, 2)].into_iter().collect(),
                ..Latencies::default()
            },
            "an overridden link does not exist",
        ),
        (
            Latencies {
                links: [(0, 0)].into_iter().collect(),
                ..Latencies::default()
            },
            "a link latency is outside 1 to MAX_CYCLES",
        ),
        (
            Latencies {
                links: [(0, MAX_CYCLES + 1)].into_iter().collect(),
                ..Latencies::default()
            },
            "a link latency is outside 1 to MAX_CYCLES",
        ),
        (
            Latencies {
                respond: [("soc.cpu".to_owned(), 1)].into_iter().collect(),
                ..Latencies::default()
            },
            "an overridden path is not a responding target",
        ),
        (
            Latencies {
                respond: [("soc.ram".to_owned(), MAX_CYCLES + 1)]
                    .into_iter()
                    .collect(),
                ..Latencies::default()
            },
            "a response latency is above MAX_CYCLES",
        ),
    ];
    for (latencies, why) in cases {
        let m2 = m2ref::Config {
            latencies: latencies.clone(),
            ..m2ref::Config::frozen()
        };
        assert!(
            matches!(m2ref::build(&image, &disk, &m2), Err(m2ref::BuildError::Latencies(w)) if w == why),
            "{why}"
        );
        let m3 = m3ref::Config {
            latencies,
            ..m3ref::Config::frozen()
        };
        assert!(
            matches!(m3ref::build(&firmware, &m3_disk, &m3), Err(m3ref::BuildError::Latencies(w)) if w == why),
            "{why}"
        );
    }
    // The kernel responds on m3-reference only.
    let kernel = Latencies {
        respond: [("soc.kernel".to_owned(), 2)].into_iter().collect(),
        ..Latencies::default()
    };
    let m2 = m2ref::Config {
        latencies: kernel.clone(),
        ..m2ref::Config::frozen()
    };
    assert!(matches!(
        m2ref::build(&image, &disk, &m2),
        Err(m2ref::BuildError::Latencies(_))
    ));
    let m3 = m3ref::Config {
        latencies: kernel,
        ..m3ref::Config::frozen()
    };
    assert!(m3ref::build(&firmware, &m3_disk, &m3).is_ok());
    // Jitter on a link that does not exist does not elaborate.
    let m3 = m3ref::Config {
        jitter: Some(Jitter {
            seed: 1,
            max_cycles: 1,
            links: LinkSet::of(&[m3ref::LINKS]),
            only: None,
        }),
        ..m3ref::Config::frozen()
    };
    assert!(matches!(
        m3ref::build(&firmware, &m3_disk, &m3),
        Err(m3ref::BuildError::Elaboration(_))
    ));
}

#[test]
fn m3_reference_passes_under_jitter_on_every_link() {
    let frozen = m3_run(&m3ref::Config::frozen(), traced());
    m3::judge_without_trace(&frozen, Scenario::Reference).unwrap();
    for (seed, max_cycles) in [(1, 1), (2, 2), (3, 4), (0x5eed, 8)] {
        let config = m3ref::Config {
            jitter: jitter(seed, max_cycles, m3ref::LINKS),
            ..m3ref::Config::frozen()
        };
        let run = m3_run(&config, traced());
        m3::judge_without_trace(&run, Scenario::Reference)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_ne!(
            run.finished.outcome.execution, frozen.finished.outcome.execution,
            "seed {seed} changes the schedule"
        );
    }
}

#[test]
fn m3_reference_passes_under_latency_overrides() {
    let config = m3ref::Config {
        latencies: Latencies {
            links: (0..m3ref::LINKS).map(|i| (i, 2)).collect(),
            respond: m3ref::TARGETS
                .iter()
                .map(|p| ((*p).to_owned(), 3))
                .collect(),
        },
        ..m3ref::Config::frozen()
    };
    let run = m3_run(&config, traced());
    m3::judge_without_trace(&run, Scenario::Reference).unwrap();
}

/// Checkpoints across a jittered run, each restored into a freshly built platform with the
/// same setting: the same end, digests, and final snapshot as never stopping. A platform
/// with another setting, or none, refuses the snapshot.
#[test]
fn a_jittered_run_restores_to_the_same_end() {
    let config = m3ref::Config {
        jitter: jitter(7, 3, m3ref::LINKS),
        ..m3ref::Config::frozen()
    };
    let reference = m3_run(&config, Start::Init { traced: false });
    let n = reference.finished.dispatched.len();
    let mut rt = m3_platform(&config);
    rt.init().unwrap();
    let mut done = 0;
    for k in (1..10).map(|i| i * n / 10) {
        while done < k {
            let ev = rt.step().unwrap().unwrap();
            assert_eq!(ev, reference.finished.dispatched[done], "event {done}");
            done += 1;
        }
        let snapshot = rt.snapshot().unwrap();
        let mut restored = m3_platform(&config);
        restored.restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot, "{k}");

        for other in [
            m3ref::Config::frozen(),
            m3ref::Config {
                jitter: jitter(8, 3, m3ref::LINKS),
                ..m3ref::Config::frozen()
            },
        ] {
            assert!(m3_platform(&other).restore(&snapshot).is_err(), "{k}");
        }

        let resumed = m3_run(
            &config,
            Start::Restore {
                snapshot,
                prefix: None,
            },
        );
        let (x, y) = (&reference.finished.outcome, &resumed.finished.outcome);
        assert_eq!(
            (x.state, x.execution, &x.end),
            (y.state, y.execution, &y.end),
            "{k}"
        );
        assert_eq!(reference.snapshot, resumed.snapshot, "{k}");
        assert_eq!(
            &reference.finished.dispatched[k..],
            resumed.finished.dispatched.as_slice(),
            "{k}"
        );
    }
}

#[test]
fn m2_reference_passes_under_tie_order() {
    let frozen = m2_run(&m2ref::Config::frozen());
    for seed in [1, 2, 3, 0x5eed] {
        for j in [None, jitter(seed, 2, m2ref::LINKS)] {
            let config = m2ref::Config {
                ties: Some(Ties { seed }),
                jitter: j,
                ..m2ref::Config::frozen()
            };
            let run = m2_run(&config);
            block_irq::judge(&run).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            assert_eq!(run.output, frozen.output, "seed {seed}");
            assert_eq!(
                digests(&run.finished.finished.outcome),
                digests(&m2_run(&config).finished.finished.outcome),
                "seed {seed} is deterministic"
            );
        }
    }
}

#[test]
fn m3_reference_passes_under_tie_order() {
    for seed in [1, 2, 0x5eed] {
        for j in [None, jitter(seed, 2, m3ref::LINKS)] {
            let config = m3ref::Config {
                ties: Some(Ties { seed }),
                jitter: j,
                ..m3ref::Config::frozen()
            };
            let run = m3_run(&config, traced());
            m3::judge_without_trace(&run, Scenario::Reference)
                .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        }
    }
}

/// Checkpoints across a tied run restore to the same end; a platform with another tie
/// seed, or none, refuses them.
#[test]
fn a_tied_run_restores_to_the_same_end() {
    let config = m3ref::Config {
        ties: Some(Ties { seed: 7 }),
        ..m3ref::Config::frozen()
    };
    let reference = m3_run(&config, Start::Init { traced: false });
    let n = reference.finished.dispatched.len();
    let mut rt = m3_platform(&config);
    rt.init().unwrap();
    let mut done = 0;
    for k in (1..5).map(|i| i * n / 5) {
        while done < k {
            let ev = rt.step().unwrap().unwrap();
            assert_eq!(ev, reference.finished.dispatched[done], "event {done}");
            done += 1;
        }
        let snapshot = rt.snapshot().unwrap();
        for other in [
            m3ref::Config::frozen(),
            m3ref::Config {
                ties: Some(Ties { seed: 8 }),
                ..m3ref::Config::frozen()
            },
        ] {
            assert!(m3_platform(&other).restore(&snapshot).is_err(), "{k}");
        }
        let resumed = m3_run(
            &config,
            Start::Restore {
                snapshot,
                prefix: None,
            },
        );
        let (x, y) = (&reference.finished.outcome, &resumed.finished.outcome);
        assert_eq!(
            (x.state, x.execution, &x.end),
            (y.state, y.execution, &y.end),
            "{k}"
        );
        assert_eq!(reference.snapshot, resumed.snapshot, "{k}");
    }
}

/// An empty script (`docs/m6-design.md` §7.2) changes no rank and no tick, so a scripted
/// session runs the frozen schedule: the same events, execution digest, output, and
/// component states. Only the state digest differs, since it covers the snapshot's
/// format and script.
#[test]
fn an_empty_script_runs_the_frozen_schedule() {
    let schedule = |o: &Outcome| (o.execution, o.events);
    let frozen = m2_run(&m2ref::Config::frozen());
    let scripted = m2_run(&m2ref::Config {
        script: Some(Script::default()),
        ..m2ref::Config::frozen()
    });
    assert_eq!(
        schedule(&scripted.finished.finished.outcome),
        schedule(&frozen.finished.finished.outcome)
    );
    assert_ne!(
        scripted.finished.finished.outcome.state,
        frozen.finished.finished.outcome.state
    );
    assert_eq!(scripted.output, frozen.output);
    let frozen = m3_run(&m3ref::Config::frozen(), Start::Init { traced: false });
    let scripted = m3_run(
        &m3ref::Config {
            script: Some(Script::default()),
            ..m3ref::Config::frozen()
        },
        Start::Init { traced: false },
    );
    assert_eq!(
        schedule(&scripted.finished.outcome),
        schedule(&frozen.finished.outcome)
    );
    assert_eq!(scripted.output, frozen.output);
    let states = |run: &M3Run| m2ref::components(run.snapshot.as_ref().unwrap()).unwrap();
    assert_eq!(states(&scripted), states(&frozen));
}

/// An unjittered snapshot keeps format version 1; a jittered one is version 2; a tied
/// one, jittered or not, is version 3.
#[test]
fn snapshot_versions_follow_the_jitter() {
    let version = |config: &m3ref::Config| {
        let mut rt = m3_platform(config);
        rt.init().unwrap();
        let snapshot = rt.snapshot().unwrap();
        u32::from_le_bytes(snapshot[8..12].try_into().unwrap())
    };
    assert_eq!(version(&m3ref::Config::frozen()), 1);
    let jittered = m3ref::Config {
        jitter: jitter(1, 1, m3ref::LINKS),
        ..m3ref::Config::frozen()
    };
    assert_eq!(version(&jittered), 2);
    for config in [m3ref::Config::frozen(), jittered] {
        let tied = m3ref::Config {
            ties: Some(Ties { seed: 1 }),
            ..config
        };
        assert_eq!(version(&tied), 3);
    }
}
