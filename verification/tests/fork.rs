//! The runtime adapter forked from a parent's checkpoint (`docs/m6-design.md` §9.3,
//! §9.4).

mod common;

use common::{config, pair_backend, pair_scenario, profile};
use systemscope_runtime::choice::Recording;
use systemscope_runtime::jitter::LinkSet;
use systemscope_runtime::script::Script;
use systemscope_runtime::search::{Bounds, children};
use systemscope_verify::backend::Backend;
use systemscope_verify::case::InputVariant;
use systemscope_verify::checkpoint::Checkpoint;
use systemscope_verify::error::EngineError;
use systemscope_verify::observation::Observation;
use systemscope_verify::runtime::RuntimeBackend;

/// A side's run to the stop rule: its recording, its observations of each compared
/// stream, and its checkpoint before each event.
struct Run {
    rec: Recording,
    streams: Vec<Vec<Observation>>,
    checkpoints: Vec<Checkpoint>,
}

fn run_on(b: &mut RuntimeBackend, mut rec: Recording) -> Run {
    let mut checkpoints = Vec::new();
    while !b.finished().unwrap() {
        checkpoints.push(b.checkpoint().unwrap());
        b.step_recorded(&mut rec).unwrap();
    }
    rec.stop(b.runtime().unwrap()).unwrap();
    let streams = profile()
        .streams()
        .map(|s| b.emitted(&s.stream).unwrap())
        .collect();
    Run {
        rec,
        streams,
        checkpoints,
    }
}

fn from_init(script: &Script) -> Run {
    let mut b = pair_backend(script.clone());
    b.start(&pair_scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    let rec = Recording::after_init(b.runtime().unwrap());
    run_on(&mut b, rec)
}

fn picks(deviations: usize) -> Bounds {
    Bounds {
        deviations,
        delay: 0,
        shift: 0,
        scope: LinkSet::default(),
        asynchronous: Vec::new(),
    }
}

fn fork(script: &Script, from: &Checkpoint) -> Result<RuntimeBackend, EngineError> {
    let mut b = pair_backend(script.clone());
    b.fork(
        &pair_scenario(),
        &InputVariant::None,
        &config(),
        &profile(),
        from,
    )?;
    Ok(b)
}

#[test]
fn a_fork_from_a_tick_boundary_runs_on_as_the_child_does_from_init() {
    let parent = from_init(&Script::default());
    let tick = |k: usize| parent.rec.steps[k].event.key.tick;
    let kids = children(&Script::default(), None, &parent.rec, &picks(1)).unwrap();
    // One pick at each tick's first dispatch. The stop rule is the first counter's last
    // record, and the second counter's wake, pending there, is ready at it.
    assert_eq!(kids.len(), 12);
    let mut forks = 0;
    for child in &kids {
        let whole = from_init(&child.script);
        assert_ne!(whole.streams, parent.streams, "{:?}", child.script);
        let grandchildren = children(&child.script, Some(child.branch), &whole.rec, &picks(2));
        for (k, from) in parent.checkpoints.iter().enumerate() {
            let boundary = k == 0 || tick(k - 1) < tick(k);
            if !boundary || k as u64 >= child.branch.at {
                continue;
            }
            let mut b = fork(&child.script, from).unwrap();
            let rec = Recording::after_checkpoint(b.runtime().unwrap(), k as u64);
            let part = run_on(&mut b, rec);
            assert_eq!(part.rec.steps, whole.rec.steps[k..], "{k}");
            assert_eq!(part.checkpoints, whole.checkpoints[k..], "{k}");
            for (i, s) in profile().streams().enumerate() {
                let covered = from.mark(&s.stream).unwrap().covered;
                let covered = usize::try_from(covered).unwrap();
                assert_eq!(part.streams[i], whole.streams[i][covered..], "{k}");
            }
            assert_eq!(
                children(&child.script, Some(child.branch), &part.rec, &picks(2)),
                grandchildren,
                "{k}"
            );
            forks += 1;
        }
    }
    // Every tick boundary before each pick's dispatch: 1 + 2 + ... + 12.
    assert_eq!(forks, 78);
}

#[test]
fn a_fork_refuses_other_streams_and_a_script_that_is_no_extension() {
    let parent = from_init(&Script::default());
    let kids = children(&Script::default(), None, &parent.rec, &picks(1)).unwrap();
    let from = &parent.checkpoints[0];
    let mut marks = from.clone();
    marks.streams.pop();
    assert!(matches!(
        fork(&Script::default(), &marks),
        Err(EngineError::Internal(_))
    ));
    // The child's checkpoint, forked under the parent's script, drops its pick.
    let child = from_init(&kids[0].script);
    assert!(matches!(
        fork(&Script::default(), &child.checkpoints[0]),
        Err(EngineError::BackendFailed(_))
    ));
    let mut b = pair_backend(Script::default());
    assert_eq!(b.finished(), None);
    assert!(matches!(
        b.step_recorded(&mut Recording::default()),
        Err(EngineError::Internal(_))
    ));
}
