//! The search driver (`docs/m6-design.md` §9.9): runs the scripts of the search tree
//! (`runtime::search`) in rounds against a reference, forks each child from its parent's
//! checkpoints, and orders the frontier by a guide's coverage score.
//!
//! Every result is the same for any thread count, store budget, and checkpoint
//! interval, and with or without forks: the store only saves work.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use systemscope_contracts::trace::TraceRecord;
use systemscope_runtime::choice::{BranchPoint, Recording as Choices, Step};
use systemscope_runtime::script::{EventId, Script};
use systemscope_runtime::search::{Bounds, Child, children};

use crate::backend::Backend;
use crate::case::{BackendRunConfig, InputVariant, Scenario};
use crate::checkpoint::Checkpoint;
use crate::compare::{Recorded, Recording, StreamResult, Verdict, compare_recordings};
use crate::error::EngineError;
use crate::profile::Plan;
use crate::runtime::RuntimeBackend;

/// Reads runs for coverage (`docs/m6-design.md` §11.2).
pub trait Guide: Sync {
    /// A coverage item.
    type Item: Clone + Ord + Send + Sync + Debug;
    /// Whether its readers read trace records; if not, they get none.
    fn traced(&self) -> bool {
        false
    }
    /// A reader of one run, from its initial state.
    fn reader(&self) -> Box<dyn Reader<Self::Item>>;
    /// An item's weight in a score.
    fn weight(&self, _item: &Self::Item) -> u64 {
        1
    }
}

/// Reads one run's events in order.
pub trait Reader<I> {
    /// Reads a dispatched event and the trace records its dispatch emitted: adds the
    /// items it covers to `covered`, and returns the candidate items its deviation offers.
    fn read(&mut self, step: &Step, records: &[TraceRecord], covered: &mut BTreeSet<I>) -> Vec<I>;
    /// The state it holds across the current event boundary, as bytes; empty when none.
    fn save(&self) -> Vec<u8> {
        Vec::new()
    }
    /// Takes up the state [`Reader::save`] gave, in place of the initial state.
    fn load(&mut self, state: &[u8]) -> Result<(), String> {
        if state.is_empty() {
            Ok(())
        } else {
            Err("the reader keeps no state".to_owned())
        }
    }
}

/// The guide that covers nothing: the frontier is ordered by branch point alone.
pub struct Unguided;

impl Guide for Unguided {
    type Item = ();
    fn reader(&self) -> Box<dyn Reader<()>> {
        Box::new(Unguided)
    }
}

impl Reader<()> for Unguided {
    fn read(&mut self, _: &Step, _: &[TraceRecord], _: &mut BTreeSet<()>) -> Vec<()> {
        Vec::new()
    }
}

/// An adapter for any script.
pub type Adapter<'a> = &'a (dyn Fn(&Script) -> RuntimeBackend + Sync);

/// What the search runs against.
pub struct Target<'a, G> {
    /// The scenario.
    pub scenario: Scenario,
    /// The input variant every run takes.
    pub input: InputVariant,
    /// Each run's configuration: its event limit.
    pub config: BackendRunConfig,
    /// The plan of every comparison.
    pub plan: Plan,
    /// The recording every run is compared with.
    pub reference: Recording,
    /// The adapter of each script.
    pub adapter: Adapter<'a>,
    /// The guide.
    pub guide: &'a G,
}

/// The search's parameters: each changes the results, and the report records them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// The tree's bounds.
    pub bounds: Bounds,
    /// `B`, the scripts per round.
    pub round: usize,
    /// The runs the search may take, checked between rounds.
    pub runs: u64,
}

/// What the store may keep: neither changes a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreParams {
    /// The tick interval of checkpoints; 0 takes none.
    pub interval: u64,
    /// The most encoded checkpoint bytes kept between rounds.
    pub memory: usize,
}

/// How a run compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    /// Its new deviation's effective schedule is its parent's or an earlier sibling's.
    Duplicate,
    /// It agrees with the reference.
    Agreed,
    /// It diverged from the reference at this ordinal of the primary stream.
    Diverged(u64),
    /// A side failed before any divergence, with the failure.
    Failed(String),
}

/// A script the search ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explored {
    /// The script.
    pub script: Script,
    /// Its new deviation's branch point; `None` for the root.
    pub branch: Option<BranchPoint>,
    /// How it compared.
    pub class: Class,
    /// Whether it reached the event limit.
    pub truncated: bool,
}

/// A script the search has yet to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<I> {
    /// The script.
    pub script: Script,
    /// Its new deviation's branch point; `None` for the root.
    pub branch: Option<BranchPoint>,
    /// Its parent's place among the explored scripts.
    pub parent: Option<usize>,
    /// Its place among its siblings.
    pub sibling: usize,
    /// The items its new deviation could cover.
    pub candidates: Vec<I>,
}

/// The search state, complete at each round boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search<I> {
    /// The parameters.
    pub params: Params,
    /// The scripts to run.
    pub frontier: Vec<Entry<I>>,
    /// The scripts run, in the order their results were merged.
    pub explored: Vec<Explored>,
    /// The items the explored runs covered.
    pub coverage: BTreeSet<I>,
    /// The rounds run.
    pub rounds: u64,
    /// The scripts taken from the frontier.
    pub taken: u64,
}

/// Checkpoints and observations kept for forks, by run.
#[derive(Default)]
pub struct Store {
    checkpoints: BTreeMap<(usize, u64), Stored>,
    observations: BTreeMap<usize, Arc<Recorded>>,
}

struct Stored {
    checkpoint: Arc<Checkpoint>,
    reader: Arc<Vec<u8>>,
    bytes: usize,
}

/// Where a fork starts.
struct From {
    checkpoint: Arc<Checkpoint>,
    reader: Arc<Vec<u8>>,
    observations: Arc<Recorded>,
}

impl Store {
    /// The encoded checkpoint bytes kept.
    pub fn bytes(&self) -> usize {
        self.checkpoints.values().map(|s| s.bytes).sum()
    }

    /// The checkpoints kept, as run and event index.
    pub fn kept(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        self.checkpoints.keys().copied()
    }

    /// Run `run`'s latest checkpoint before branch position `at`.
    fn latest(&self, run: usize, at: u64) -> Option<(usize, u64)> {
        self.checkpoints
            .range((run, 0)..(run, at))
            .next_back()
            .map(|(&k, _)| k)
    }

    fn from(&self, run: usize, at: u64) -> Option<From> {
        let key = self.latest(run, at)?;
        let stored = &self.checkpoints[&key];
        Some(From {
            checkpoint: Arc::clone(&stored.checkpoint),
            reader: Arc::clone(&stored.reader),
            observations: Arc::clone(self.observations.get(&run)?),
        })
    }

    /// Keeps each entry's parent's latest checkpoint before its branch point, within
    /// `memory`: the farthest from the nearest branch point it serves goes first, ties
    /// by run and event index.
    fn prune<I>(&mut self, frontier: &[Entry<I>], memory: usize) {
        let mut nearest: BTreeMap<(usize, u64), u64> = BTreeMap::new();
        for e in frontier {
            let (Some(run), Some(branch)) = (e.parent, e.branch) else {
                continue;
            };
            if let Some(key) = self.latest(run, branch.at) {
                let d = nearest.entry(key).or_insert(u64::MAX);
                *d = (*d).min(branch.at - key.1);
            }
        }
        self.checkpoints.retain(|k, _| nearest.contains_key(k));
        let mut order: Vec<_> = nearest.into_iter().collect();
        order.sort_by_key(|&(key, d)| (Reverse(d), key));
        let mut bytes = self.bytes();
        for (key, _) in order {
            if bytes <= memory {
                break;
            }
            bytes -= self.checkpoints.remove(&key).map_or(0, |s| s.bytes);
        }
        let runs: BTreeSet<usize> = self.checkpoints.keys().map(|k| k.0).collect();
        self.observations.retain(|r, _| runs.contains(r));
    }
}

/// What one run produced.
struct Output<I> {
    explored: Explored,
    children: Vec<(Child, Vec<I>)>,
    covered: BTreeSet<I>,
    checkpoints: Vec<(Checkpoint, Vec<u8>)>,
    observations: Recorded,
}

impl<I: Clone + Ord + Send + Sync + Debug> Search<I> {
    /// A search of `params` from `root`, the script every explored one extends.
    pub fn new(params: Params, root: Script) -> Search<I> {
        Search {
            params,
            frontier: vec![Entry {
                script: root,
                branch: None,
                parent: None,
                sibling: 0,
                candidates: Vec::new(),
            }],
            explored: Vec::new(),
            coverage: BTreeSet::new(),
            rounds: 0,
            taken: 0,
        }
    }

    /// Whether every script within the bounds was run or classified a duplicate.
    pub fn exhausted(&self) -> bool {
        self.frontier.is_empty()
    }

    /// Whether the search ends: exhausted, or no runs are left.
    pub fn done(&self) -> bool {
        self.exhausted() || self.taken >= self.params.runs
    }

    /// The scripts taken beyond the run budget.
    pub fn overshoot(&self) -> u64 {
        self.taken.saturating_sub(self.params.runs)
    }

    /// The score of `entry`: the weight of its candidates not yet covered.
    pub fn score<G: Guide<Item = I>>(&self, guide: &G, entry: &Entry<I>) -> u64 {
        entry
            .candidates
            .iter()
            .filter(|c| !self.coverage.contains(c))
            .map(|c| guide.weight(c))
            .sum()
    }

    /// Runs one round on `threads` threads, keeping checkpoints in `store`. An error is
    /// the driver's own, such as a fork the runtime refused, and leaves the state as it
    /// was.
    pub fn round<G: Guide<Item = I>>(
        &mut self,
        target: &Target<'_, G>,
        store: &mut Store,
        store_params: StoreParams,
        threads: usize,
    ) -> Result<(), EngineError> {
        let mut ranked: Vec<(u64, Entry<I>)> = self
            .frontier
            .iter()
            .map(|e| (self.score(target.guide, e), e.clone()))
            .collect();
        ranked.sort_by(|(sa, a), (sb, b)| {
            (
                a.script.deviations(),
                Reverse(sa),
                a.branch,
                a.parent,
                a.sibling,
            )
                .cmp(&(
                    b.script.deviations(),
                    Reverse(sb),
                    b.branch,
                    b.parent,
                    b.sibling,
                ))
        });
        let n = self.params.round.max(1).min(ranked.len());
        let jobs: Vec<(Entry<I>, Option<From>)> = ranked[..n]
            .iter()
            .map(|(_, e)| {
                let from = match (e.parent, e.branch) {
                    (Some(run), Some(b)) => store.from(run, b.at),
                    _ => None,
                };
                (e.clone(), from)
            })
            .collect();
        let outputs = run_all(target, &self.params.bounds, store_params, &jobs, threads)?;
        self.frontier = ranked.into_iter().skip(n).map(|(_, e)| e).collect();
        for output in outputs {
            let run = self.explored.len();
            self.explored.push(output.explored);
            self.coverage.extend(output.covered);
            let parent = Some(run);
            let extended = !output.children.is_empty();
            for (sibling, (child, candidates)) in output.children.into_iter().enumerate() {
                self.frontier.push(Entry {
                    script: child.script,
                    branch: Some(child.branch),
                    parent,
                    sibling,
                    candidates,
                });
            }
            if extended && !output.checkpoints.is_empty() {
                for (checkpoint, reader) in output.checkpoints {
                    let bytes = checkpoint.to_bytes().len();
                    let key = (run, checkpoint.event_index);
                    let stored = Stored {
                        checkpoint: Arc::new(checkpoint),
                        reader: Arc::new(reader),
                        bytes,
                    };
                    store.checkpoints.insert(key, stored);
                }
                store
                    .observations
                    .insert(run, Arc::new(output.observations));
            }
        }
        self.rounds += 1;
        self.taken += n as u64;
        store.prune(&self.frontier, store_params.memory);
        Ok(())
    }

    /// Runs rounds until the search is done.
    pub fn run<G: Guide<Item = I>>(
        &mut self,
        target: &Target<'_, G>,
        store: &mut Store,
        store_params: StoreParams,
        threads: usize,
    ) -> Result<(), EngineError> {
        while !self.done() {
            self.round(target, store, store_params, threads)?;
        }
        Ok(())
    }
}

/// One job's output, or the driver's error.
type Ran<I> = Result<Output<I>, EngineError>;

/// Runs every job on up to `threads` threads, the outputs in job order.
fn run_all<G: Guide>(
    target: &Target<'_, G>,
    bounds: &Bounds,
    store: StoreParams,
    jobs: &[(Entry<G::Item>, Option<From>)],
    threads: usize,
) -> Result<Vec<Output<G::Item>>, EngineError> {
    let next = AtomicUsize::new(0);
    let outputs: Mutex<Vec<Option<Ran<G::Item>>>> =
        Mutex::new((0..jobs.len()).map(|_| None).collect());
    std::thread::scope(|sc| {
        for _ in 0..threads.clamp(1, jobs.len().max(1)) {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((entry, from)) = jobs.get(i) else {
                        break;
                    };
                    let output = run_one(target, bounds, store, entry, from.as_ref());
                    outputs.lock().expect("no worker panics")[i] = Some(output);
                }
            });
        }
    });
    outputs
        .into_inner()
        .expect("no worker panics")
        .into_iter()
        .map(|o| o.expect("every job ran"))
        .collect()
}

/// Runs one script: forked from `from` if given, else from its initial state.
fn run_one<G: Guide>(
    target: &Target<'_, G>,
    bounds: &Bounds,
    store: StoreParams,
    entry: &Entry<G::Item>,
    from: Option<&From>,
) -> Result<Output<G::Item>, EngineError> {
    let profile = &target.plan.profile;
    let mut b = (target.adapter)(&entry.script);
    let mut reader = target.guide.reader();
    let started = match from {
        Some(f) => {
            b.fork(
                &target.scenario,
                &target.input,
                &target.config,
                profile,
                &f.checkpoint,
            )?;
            reader
                .load(&f.reader)
                .map_err(|why| EngineError::Internal(format!("reader state: {why}")))?;
            Ok(())
        }
        None => b.start(&target.scenario, &target.input, &target.config, profile),
    };
    let explored = |class, truncated| Explored {
        script: entry.script.clone(),
        branch: entry.branch,
        class,
        truncated,
    };
    if let Err(e) = started {
        let recording = Recording {
            streams: vec![Vec::new(); profile.streams().count()],
            failure: Some(e),
        };
        return Ok(Output {
            explored: explored(judge(target, &recording), false),
            children: Vec::new(),
            covered: BTreeSet::new(),
            checkpoints: Vec::new(),
            observations: recording.streams,
        });
    }
    if target.guide.traced() {
        b.capture()?;
    }
    let mut rec = match from {
        Some(f) => Choices::after_checkpoint(runtime(&b), f.checkpoint.event_index),
        None => Choices::after_init(runtime(&b)),
    };
    let mut covered = BTreeSet::new();
    let mut candidates: BTreeMap<EventId, Vec<G::Item>> = BTreeMap::new();
    let mut checkpoints = Vec::new();
    let mut truncated = false;
    let failure = loop {
        if let Some(branch) = entry.branch
            && let Some(script) = runtime(&b).script()
            && script.duplicates().contains(&branch.id)
        {
            return Ok(Output {
                explored: explored(Class::Duplicate, false),
                children: Vec::new(),
                covered: BTreeSet::new(),
                checkpoints: Vec::new(),
                observations: Vec::new(),
            });
        }
        if b.finished() == Some(true) {
            rec.stop(runtime(&b))
                .map_err(|_| EngineError::Internal("an unscripted run".to_owned()))?;
            break None;
        }
        let rt = runtime(&b);
        if store.interval > 0
            && let Some(next) = rt.peek_key()
            && rt
                .last_dispatched()
                .is_none_or(|l| l.tick.0 / store.interval < next.tick.0 / store.interval)
        {
            checkpoints.push((b.checkpoint()?, reader.save()));
        }
        match b.step_recorded(&mut rec) {
            Ok(()) => {
                let step = rec.steps.last().expect("a step was recorded");
                let records = b.captured();
                let offered = reader.read(step, &records, &mut covered);
                if !offered.is_empty()
                    && let Some(id) = step.event.id
                {
                    candidates.insert(id, offered);
                }
            }
            Err(e) => {
                truncated = b.events().is_some_and(|n| n >= target.config.budget);
                break Some(e);
            }
        }
    };
    let mut observations = Vec::new();
    for s in profile.streams() {
        let mut stream = match from {
            Some(f) => {
                let covered = f.checkpoint.mark(&s.stream).map_or(0, |m| m.covered);
                let i = observations.len();
                f.observations[i][..usize::try_from(covered).expect("fits")].to_vec()
            }
            None => Vec::new(),
        };
        stream.extend(b.emitted(&s.stream)?);
        observations.push(stream);
    }
    let recording = Recording {
        streams: observations,
        failure,
    };
    let class = judge(target, &recording);
    let kids = children(&entry.script, entry.branch, &rec, bounds)
        .map_err(|_| EngineError::Internal("an unscripted run".to_owned()))?;
    let children = kids
        .into_iter()
        .map(|child| {
            let offered = moved(&child)
                .into_iter()
                .filter_map(|id| candidates.get(&id))
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>();
            (child, offered.into_iter().collect())
        })
        .collect();
    Ok(Output {
        explored: explored(class, truncated),
        children,
        covered,
        checkpoints,
        observations: recording.streams,
    })
}

/// A started adapter's runtime.
fn runtime(b: &RuntimeBackend) -> &systemscope_runtime::runtime::Runtime {
    b.runtime().expect("started")
}

/// The events a child's new deviation moves: the delayed message, the shifted wake, or
/// the picked event and the default.
fn moved(child: &Child) -> Vec<EventId> {
    let id = child.branch.id;
    if child.branch.kind != 0 {
        return vec![id];
    }
    let picked = child
        .script
        .picks
        .iter()
        .find(|(default, _)| *default == id)
        .and_then(|(_, events)| events.last())
        .copied();
    picked.into_iter().chain([id]).collect()
}

/// How `recording` compares with the reference, as M5 judges.
fn judge<G>(target: &Target<'_, G>, recording: &Recording) -> Class {
    match compare_recordings(&target.plan, &target.reference, recording) {
        Ok(Verdict::Compared(c)) => match &c.primary {
            StreamResult::Agree { .. } if c.failures.is_empty() => Class::Agreed,
            StreamResult::Agree { .. } => Class::Failed(format!("{:?}", c.failures)),
            StreamResult::Diverged { ordinal, .. } => Class::Diverged(*ordinal),
        },
        Ok(Verdict::Incomparable(why)) => Class::Failed(format!("incomparable: {why:?}")),
        Err(e) => Class::Failed(format!("{e}")),
    }
}
