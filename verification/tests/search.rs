//! The search driver (`docs/m6-design.md` §9.9) on the two-counter fixture: it runs the
//! tree once each, and its results do not depend on threads, the store, or forks.

mod common;

use std::cmp::Reverse;
use std::collections::BTreeSet;

use common::{config, pair_backend, pair_scenario, profile};
use systemscope_contracts::trace::TraceRecord;
use systemscope_runtime::choice::{BranchPoint, Recording as Choices, Step};
use systemscope_runtime::jitter::LinkSet;
use systemscope_runtime::script::Script;
use systemscope_runtime::search::{Bounds, children};
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::case::{BackendRunConfig, InputVariant};
use systemscope_verify::compare::{StreamResult, Verdict, compare_recordings};
use systemscope_verify::profile::{Plan, plan};
use systemscope_verify::runtime::RuntimeBackend;
use systemscope_verify::search::{
    Class, Guide, Params, Reader, Search, Store, StoreParams, Target, Unguided,
};

fn the_plan() -> Plan {
    let support = pair_backend(Script::default()).support();
    plan(&profile(), &support, &support).unwrap()
}

fn adapter(script: &Script) -> RuntimeBackend {
    pair_backend(script.clone())
}

/// `script`'s whole recording, from init, under `config`.
fn recorded(script: &Script, config: BackendRunConfig) -> systemscope_verify::compare::Recording {
    let mut b = adapter(script);
    let side = Side {
        backend: &mut b,
        input: InputVariant::None,
        config,
    };
    run(&the_plan(), &pair_scenario(), side)
}

fn target<G>(guide: &G, config: BackendRunConfig) -> Target<'_, G> {
    Target {
        scenario: pair_scenario(),
        input: InputVariant::None,
        config,
        plan: the_plan(),
        reference: recorded(&Script::default(), config),
        adapter: &adapter,
        guide,
    }
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

/// Picks, and shifts of `±1..=shift` cycles of the first counter's wakes.
fn mixed(deviations: usize, shift: u64) -> Bounds {
    Bounds {
        shift,
        asynchronous: vec![0],
        ..picks(deviations)
    }
}

fn params(bounds: Bounds, round: usize, runs: u64) -> Params {
    Params {
        bounds,
        round,
        runs,
    }
}

const NO_FORKS: StoreParams = StoreParams {
    interval: 0,
    memory: 0,
};

/// Every script of the tree below `script`, from runs from init stepped by hand, with
/// whether the runtime classified its new deviation a duplicate; duplicates are not
/// extended.
fn tree(script: Script, last: Option<BranchPoint>, bounds: &Bounds, out: &mut Vec<(Script, bool)>) {
    let (b, rec) = stepped(&script);
    let flagged = b.runtime().unwrap().script().unwrap().duplicates();
    let duplicate = last.is_some_and(|l| flagged.contains(&l.id));
    out.push((script.clone(), duplicate));
    if duplicate {
        return;
    }
    for child in children(&script, last, &rec, bounds).unwrap() {
        tree(child.script, Some(child.branch), bounds, out);
    }
}

/// `script`'s run from init, stepped by hand to its stop.
fn stepped(script: &Script) -> (RuntimeBackend, Choices) {
    let mut b = adapter(script);
    b.start(&pair_scenario(), &InputVariant::None, &config(), &profile())
        .unwrap();
    let mut rec = Choices::after_init(b.runtime().unwrap());
    while !b.finished().unwrap() {
        b.step_recorded(&mut rec).unwrap();
    }
    rec.stop(b.runtime().unwrap()).unwrap();
    (b, rec)
}

/// Scripts as a set, by their debug form.
fn set(scripts: &[Script]) -> BTreeSet<String> {
    scripts.iter().map(|s| format!("{s:?}")).collect()
}

fn order<I>(search: &Search<I>) -> Vec<Script> {
    search.explored.iter().map(|e| e.script.clone()).collect()
}

/// The scripts and duplicates of `mixed(2, 2)`, a committed expectation.
const EXPECTED_SCRIPTS: usize = 1337;
const EXPECTED_DUPLICATES: usize = 265;

#[test]
fn the_driver_runs_every_script_of_the_tree_once_and_judges_it() {
    let bounds = mixed(2, 2);
    let mut search = Search::new(params(bounds.clone(), 5, u64::MAX), Script::default());
    search
        .run(
            &target(&Unguided, config()),
            &mut Store::default(),
            NO_FORKS,
            2,
        )
        .unwrap();
    assert!(search.exhausted());
    let mut expected = Vec::new();
    tree(Script::default(), None, &bounds, &mut expected);
    let scripts: Vec<Script> = expected.iter().map(|e| e.0.clone()).collect();
    let duplicates = expected.iter().filter(|e| e.1).count();
    assert_eq!(
        (expected.len(), duplicates),
        (EXPECTED_SCRIPTS, EXPECTED_DUPLICATES)
    );
    let ran = order(&search);
    assert_eq!(ran.len(), expected.len());
    assert_eq!(set(&ran), set(&scripts));
    // Depth first, and within a depth by branch point, unguided.
    let keys: Vec<_> = search
        .explored
        .iter()
        .map(|e| (e.script.deviations(), e.branch))
        .collect();
    assert!(keys.is_sorted_by_key(|k| k.0), "{keys:?}");
    let depth_one: Vec<_> = keys.iter().filter(|k| k.0 == 1).collect();
    assert!(depth_one.is_sorted(), "{depth_one:?}");
    let reference = recorded(&Script::default(), config());
    for e in &search.explored {
        let (_, duplicate) = expected.iter().find(|x| x.0 == e.script).unwrap();
        if *duplicate {
            assert_eq!(e.class, Class::Duplicate, "{:?}", e.script);
            continue;
        }
        let class =
            match compare_recordings(&the_plan(), &reference, &recorded(&e.script, config()))
                .unwrap()
            {
                Verdict::Compared(c) => match c.primary {
                    StreamResult::Agree { .. } => Class::Agreed,
                    StreamResult::Diverged { ordinal, .. } => Class::Diverged(ordinal),
                },
                Verdict::Incomparable(_) => unreachable!(),
            };
        assert_eq!(e.class, class, "{:?}", e.script);
        assert!(!e.truncated);
    }
    assert_eq!(search.explored[0].class, Class::Agreed);
    assert_eq!(search.overshoot(), 0);
}

/// Counts each component's events: an event covers its component's count `n`, and
/// `100 + n` if it is the first of its tick, else `200 + n`; each event offers both, with
/// weight `n`, so a pick offers the picked event's and the default's. Its counts and the
/// last tick are state across event boundaries.
struct Counts;

#[derive(Default)]
struct CountReader {
    counts: [u64; 2],
    last: Option<u64>,
}

impl Guide for Counts {
    type Item = (u32, u64);
    fn traced(&self) -> bool {
        true
    }
    fn reader(&self) -> Box<dyn Reader<(u32, u64)>> {
        Box::new(CountReader::default())
    }
    fn weight(&self, item: &(u32, u64)) -> u64 {
        item.1 % 100
    }
}

impl Reader<(u32, u64)> for CountReader {
    fn read(
        &mut self,
        step: &Step,
        records: &[TraceRecord],
        covered: &mut BTreeSet<(u32, u64)>,
    ) -> Vec<(u32, u64)> {
        assert!(records.iter().any(|r| r.kind == "t.step"));
        let c = step.event.target.0;
        self.counts[c as usize] += 1;
        let n = self.counts[c as usize];
        covered.insert((c, n));
        let tick = step.event.key.tick.0;
        let first = self.last != Some(tick);
        covered.insert((c, if first { 100 } else { 200 } + n));
        self.last = Some(tick);
        vec![(c, 100 + n), (c, 200 + n)]
    }
    fn save(&self) -> Vec<u8> {
        let last = self.last.unwrap_or(u64::MAX);
        [self.counts[0], self.counts[1], last]
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect()
    }
    fn load(&mut self, state: &[u8]) -> Result<(), String> {
        let words: Vec<u64> = state
            .chunks(8)
            .map(|c| c.try_into().map(u64::from_le_bytes))
            .collect::<Result<_, _>>()
            .map_err(|_| "8-byte words")?;
        let [a, b, last] = words[..] else {
            return Err(format!("{} words", words.len()));
        };
        self.counts = [a, b];
        self.last = (last != u64::MAX).then_some(last);
        Ok(())
    }
}

/// Runs a search round by round, checking the store's budget after each.
fn searched<G: Guide>(guide: &G, p: Params, store: StoreParams, threads: usize) -> Search<G::Item> {
    let target = target(guide, config());
    let mut search = Search::new(p, Script::default());
    let mut s = Store::default();
    let mut kept = 0;
    while !search.done() {
        search.round(&target, &mut s, store, threads).unwrap();
        assert!(s.bytes() <= store.memory, "{}", s.bytes());
        kept += s.kept().count();
    }
    if store.interval > 0 && store.memory > 0 {
        assert!(kept > 0);
    }
    search
}

#[test]
fn results_do_not_depend_on_threads_the_store_or_forks() {
    let stores = [
        NO_FORKS,
        StoreParams {
            interval: 1000,
            memory: usize::MAX,
        },
        StoreParams {
            interval: 3000,
            memory: usize::MAX,
        },
        // Room for about two checkpoints: eviction decides which forks happen.
        StoreParams {
            interval: 1000,
            memory: 2 * 400,
        },
    ];
    let p = params(mixed(2, 2), 4, u64::MAX);
    let base = searched(&Counts, p.clone(), NO_FORKS, 1);
    assert!(base.exhausted());
    assert!(base.explored.iter().any(|e| e.class == Class::Duplicate));
    assert!(!base.coverage.is_empty());
    for store in stores {
        for threads in [1, 3] {
            assert_eq!(
                searched(&Counts, p.clone(), store, threads),
                base,
                "{store:?} {threads}"
            );
        }
    }
    // The second round takes the four depth-one scripts of highest score against the
    // root's coverage, ties by branch point.
    let root = searched(&Counts, params(mixed(2, 2), 4, 1), NO_FORKS, 1);
    let mut ranked: Vec<_> = root
        .frontier
        .iter()
        .map(|e| (Reverse(root.score(&Counts, e)), e.branch, e.script.clone()))
        .collect();
    ranked.sort_by_key(|a| (a.0, a.1));
    assert!(ranked[0].0 != ranked[ranked.len() - 1].0);
    let second: Vec<Script> = ranked[..4].iter().map(|r| r.2.clone()).collect();
    assert_eq!(order(&base)[1..5], second);
    // A pick offers the picked event's and the default's items, a shift the wake's, of
    // the events the root dispatched.
    let (_, rec) = stepped(&Script::default());
    let dispatched: BTreeSet<_> = rec.steps.iter().filter_map(|s| s.event.id).collect();
    let offers = |id| if dispatched.contains(&id) { 2 } else { 0 };
    for e in &root.frontier {
        let branch = e.branch.unwrap();
        let picked = e
            .script
            .picks
            .iter()
            .map(|p| p.1[0])
            .map(offers)
            .sum::<usize>();
        let moved = offers(branch.id) + if branch.kind == 0 { picked } else { 0 };
        assert_eq!(e.candidates.len(), moved, "{e:?}");
    }
    assert!(root.frontier.iter().any(|e| e.candidates.len() == 4));
    // The guide reorders a depth: its first entries cover the most weight.
    let unguided = order(&searched(&Unguided, p, NO_FORKS, 1));
    let guided = order(&base);
    assert_ne!(guided, unguided);
    assert_eq!(set(&guided), set(&unguided));
}

/// The event indices at the first boundary at or after each multiple of `interval`
/// ticks, of `script`'s run from init.
fn boundaries(script: &Script, interval: u64) -> BTreeSet<u64> {
    let (_, rec) = stepped(script);
    let ticks: Vec<u64> = rec
        .steps
        .iter()
        .map(|s| s.event.key.tick.0 / interval)
        .collect();
    (0..ticks.len())
        .filter(|&i| i == 0 || ticks[i - 1] < ticks[i])
        .map(|i| u64::try_from(i).unwrap())
        .collect()
}

#[test]
fn checkpoints_sit_at_the_first_boundary_of_each_interval() {
    let store = StoreParams {
        interval: 1000,
        memory: usize::MAX,
    };
    let target = target(&Unguided, config());
    let mut search = Search::new(params(mixed(2, 2), 4, u64::MAX), Script::default());
    let mut s = Store::default();
    let mut kept = 0;
    while !search.done() {
        search.round(&target, &mut s, store, 2).unwrap();
        for (run, at) in s.kept() {
            let script = &search.explored[run].script;
            assert!(boundaries(script, 1000).contains(&at), "{run} {at}");
            kept += 1;
        }
    }
    assert!(kept > 0);
}

#[test]
fn the_run_budget_is_checked_between_rounds() {
    let search = searched(&Unguided, params(picks(2), 3, 5), NO_FORKS, 2);
    // The first round runs the root alone, then 3 picks a round.
    assert_eq!((search.rounds, search.taken, search.overshoot()), (3, 7, 2));
    assert!(search.done() && !search.exhausted());
    assert_eq!(search.explored.len(), 7);
    // The root's 12 children, less the 6 run, and the children of those 6.
    assert_eq!(search.frontier.len(), 12 - 6 + (6..12).sum::<usize>());
}

#[test]
fn a_run_past_the_event_limit_is_truncated_and_extended_where_it_ran() {
    let limit = BackendRunConfig { budget: 9 };
    let mut search = Search::new(params(picks(1), 4, u64::MAX), Script::default());
    search
        .run(
            &target(&Unguided, limit),
            &mut Store::default(),
            NO_FORKS,
            1,
        )
        .unwrap();
    assert!(search.explored.iter().all(|e| e.truncated));
    // The nine events dispatched end at the first of tick 5, whose tied wake is not
    // known: a run that stops short of its stop rule keeps no pending events.
    assert_eq!(search.explored.len(), 1 + 4);
    assert!(
        search
            .explored
            .iter()
            .all(|e| matches!(e.class, Class::Failed(_)) || e.branch.is_some())
    );
}
