//! ToyDma against a ToyMemory: bursts, the region, and the outstanding limit
//! (`docs/m0-design.md` §9.1).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::rc::Rc;

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::error::SimError;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_runtime::runtime::{Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_toy::{ToyDma, ToyDmaConfig, ToyMemory, ToyMemoryConfig};

const OPS: u64 = 400;
const BASE: u64 = 256;
const LEN: u32 = 16;
const SLOTS: u32 = 32;

fn build(seed: u64, max_outstanding: u32, max_gap: u64, write_latency: Duration) -> Runtime {
    let dma = ToyDmaConfig {
        max_outstanding,
        max_gap_cycles: NonZeroU64::new(max_gap).unwrap(),
        ..dma_config()
    };
    build_with(seed, dma, write_latency)
}

/// The DMA workload; `build_with` replaces `clock` with the domain it declares.
fn dma_config() -> ToyDmaConfig {
    ToyDmaConfig {
        clock: ClockDomainId(0),
        ops: OPS,
        max_outstanding: 8,
        max_burst: NonZeroU64::new(8).unwrap(),
        max_gap_cycles: NonZeroU64::new(16).unwrap(),
        base: BASE,
        access_len: LEN,
        slots: SLOTS,
    }
}

fn build_with(seed: u64, dma: ToyDmaConfig, write_latency: Duration) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let io = t
        .add_clock(
            Frequency::new(3_000_000_000, 2).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let dma = t.add_component(
        "dma",
        Box::new(ToyDma::new(ToyDmaConfig { clock: io, ..dma })),
    );
    let mem = t.add_component(
        "mem",
        Box::new(ToyMemory::new(ToyMemoryConfig {
            size: BASE as u32 + SLOTS * LEN,
            read_latency: Duration::from_ns(50),
            write_latency,
        })),
    );
    let link = Some(LinkLatency::Cycles { domain: io, k: 1 });
    t.connect((dma, "mem"), (mem, "mem"), link);
    t.elaborate(SessionConfig {
        seed,
        ..SessionConfig::default()
    })
    .unwrap()
}

fn records(mut rt: Runtime) -> Vec<TraceRecord> {
    rt.start_trace().unwrap();
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    rt.take_trace().unwrap().records
}

fn u64_field(r: &TraceRecord, name: &str) -> u64 {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::U64(v))) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn of_kind<'a>(records: &'a [TraceRecord], kind: &str) -> Vec<&'a TraceRecord> {
    records.iter().filter(|r| r.kind == kind).collect()
}

#[test]
fn every_write_completes_once_under_ascending_txns_inside_the_region() {
    let records = records(build(1, 8, 16, Duration::from_ns(30)));
    let issued = of_kind(&records, "toy.dma.issue");
    let txns: Vec<u64> = issued.iter().map(|r| u64_field(r, "txn")).collect();
    assert_eq!(txns, (0..OPS).collect::<Vec<_>>());
    for r in &issued {
        let addr = u64_field(r, "addr");
        assert!(
            (BASE..BASE + u64::from(SLOTS * LEN)).contains(&addr),
            "{addr}"
        );
        assert_eq!((addr - BASE) % u64::from(LEN), 0, "{addr}");
    }
    let mut done: Vec<u64> = of_kind(&records, "toy.dma.done")
        .iter()
        .map(|r| u64_field(r, "txn"))
        .collect();
    done.sort_unstable();
    assert_eq!(done, txns);
}

#[test]
fn bursts_write_consecutive_slots_and_wrap_at_the_region_end() {
    let records = records(build(2, 8, 16, Duration::from_ns(30)));
    let slots: Vec<u64> = of_kind(&records, "toy.dma.issue")
        .iter()
        .map(|r| (u64_field(r, "addr") - BASE) / u64::from(LEN))
        .collect();
    let next = |a: u64| (a + 1) % u64::from(SLOTS);
    let consecutive = slots.windows(2).filter(|w| w[1] == next(w[0])).count();
    let wrapped = slots
        .windows(2)
        .filter(|w| w[0] == u64::from(SLOTS) - 1 && w[1] == 0)
        .count();
    // Bursts average 4.5 writes, so about 3.5 of every 4.5 steps continue a burst.
    assert!(
        consecutive * 2 > slots.len(),
        "{consecutive} of {}",
        slots.len()
    );
    assert!(wrapped > 0);
}

#[test]
fn a_slow_memory_holds_the_dma_at_its_outstanding_limit() {
    for limit in [1, 3] {
        let records = records(build(3, limit, 1, Duration::from_ns(500)));
        let mut in_flight = 0i64;
        let mut peak = 0;
        for r in &records {
            match r.kind {
                "toy.dma.issue" => in_flight += 1,
                "toy.dma.done" => in_flight -= 1,
                _ => continue,
            }
            peak = peak.max(in_flight);
        }
        assert_eq!(in_flight, 0);
        assert_eq!(peak, i64::from(limit));
    }
}

#[test]
fn the_seed_decides_the_writes() {
    let run = |seed| {
        let mut rt = build(seed, 8, 16, Duration::from_ns(30));
        rt.init().unwrap();
        while rt.step().unwrap().is_some() {}
        rt.state_digest().unwrap()
    };
    assert_eq!(run(7), run(7));
    assert_ne!(run(7), run(8));
}

/// Keeps what every component reports after each event: the last report is the end state.
struct EndState {
    views: Rc<RefCell<Vec<StateView>>>,
    components: u32,
}

impl Observer for EndState {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        *self.views.borrow_mut() = (0..self.components)
            .map(|id| world.inspect(ComponentId(id)).unwrap())
            .collect();
        Control::Continue
    }
}

fn end_state(rt: &mut Runtime) -> Rc<RefCell<Vec<StateView>>> {
    let views = Rc::default();
    rt.add_observer(Box::new(EndState {
        views: Rc::clone(&views),
        components: 2,
    }));
    views
}

/// The DMA's fold: FNV-1a style over 64-bit words.
fn fold(checksum: u64, word: u64) -> u64 {
    (checksum ^ word).wrapping_mul(0x0000_0100_0000_01b3)
}

#[test]
fn a_finished_dma_reports_its_counters_and_the_fold_of_its_completions() {
    let mut rt = build(1, 8, 16, Duration::from_ns(30));
    let views = end_state(&mut rt);
    let records = records(rt);
    let addr: BTreeMap<u64, u64> = of_kind(&records, "toy.dma.issue")
        .iter()
        .map(|r| (u64_field(r, "txn"), u64_field(r, "addr")))
        .collect();
    let mut checksum = 0;
    for r in of_kind(&records, "toy.dma.done") {
        let txn = u64_field(r, "txn");
        checksum = fold(fold(checksum, txn), addr[&txn]);
        assert_eq!(u64_field(r, "checksum"), checksum);
    }
    let views = views.borrow();
    assert_eq!(
        views[0].fields,
        [
            ("issued", Value::U64(OPS)),
            ("completed", Value::U64(OPS)),
            ("next_txn", Value::U64(OPS)),
            ("outstanding", Value::U64(0)),
            ("burst_left", Value::U64(0)),
            ("checksum", Value::U64(checksum)),
        ]
    );
    assert_eq!(
        views[1].fields,
        [("size", Value::U64(BASE + u64::from(SLOTS * LEN)))]
    );
}

#[test]
fn one_seed_gives_one_exact_run() {
    // Pinned: every random choice, from burst lengths to the data bytes, shifts the
    // stream behind it, so a change in what the DMA draws changes these.
    let mut rt = build(7, 8, 16, Duration::from_ns(30));
    let views = end_state(&mut rt);
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    let checksum = views.borrow()[0].fields[5].1.clone();
    assert_eq!(
        (rt.now(), checksum),
        (Tick(1_754_666), Value::U64(15_731_638_562_920_060_080))
    );
}

#[test]
fn a_dma_needs_slots_and_room_for_a_write() {
    for (max_outstanding, slots) in [(0, SLOTS), (8, 0)] {
        let dma = ToyDmaConfig {
            max_outstanding,
            slots,
            ..dma_config()
        };
        let mut rt = build_with(1, dma, Duration::from_ns(30));
        assert_eq!(
            rt.init(),
            Err(RuntimeError::Faulted(SimError::ComponentFault(
                "toy dma: max_outstanding and slots must be non-zero"
            )))
        );
    }
}

#[test]
fn a_dma_with_nothing_to_write_schedules_nothing() {
    let dma = ToyDmaConfig {
        ops: 0,
        ..dma_config()
    };
    let mut rt = build_with(1, dma, Duration::from_ns(30));
    rt.init().unwrap();
    assert_eq!(rt.pending(), 0);
}
