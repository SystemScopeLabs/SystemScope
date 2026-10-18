//! Each toy component against the component contract directly, without a runtime: its
//! names, its snapshot schema, and the states its restore accepts.

use std::num::NonZeroU64;

use systemscope_contracts::canonical::{Decoder, Encoder};
use systemscope_contracts::component::Component;
use systemscope_contracts::snapshot::{RestoreError, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Duration};
use systemscope_contracts::trace::Value;
use systemscope_toy::{ToyBus, ToyBusConfig, ToyCpu, ToyCpuConfig, ToyDma, ToyDmaConfig};
use systemscope_toy::{ToyMemory, ToyMemoryConfig, bus, cpu, dma, memory};

const SLOTS: u32 = 8;

fn cpu_config() -> ToyCpuConfig {
    ToyCpuConfig {
        clock: ClockDomainId(0),
        ops: 10,
        max_outstanding: 2,
        max_think_cycles: NonZeroU64::new(3).unwrap(),
        access_len: 8,
        slots: SLOTS,
        write_percent: 50,
    }
}

fn dma_config() -> ToyDmaConfig {
    ToyDmaConfig {
        clock: ClockDomainId(0),
        ops: 10,
        max_outstanding: 2,
        max_burst: NonZeroU64::new(4).unwrap(),
        max_gap_cycles: NonZeroU64::new(4).unwrap(),
        base: 64,
        access_len: 8,
        slots: SLOTS,
    }
}

fn memory_config() -> ToyMemoryConfig {
    ToyMemoryConfig {
        size: 64,
        read_latency: Duration::from_ns(10),
        write_latency: Duration::from_ns(5),
    }
}

fn bus_config(clock: u32) -> ToyBusConfig {
    ToyBusConfig {
        clock: ClockDomainId(clock),
    }
}

fn snapshot(c: &dyn Component) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    c.snapshot(&mut w);
    w.into_bytes()
}

/// Restores `bytes` into `c`, which must read all of them when it accepts.
fn restore(c: &mut dyn Component, bytes: &[u8]) -> Result<(), RestoreError> {
    let mut r = Decoder::new(bytes);
    c.restore(&mut r, c.snapshot_schema_version())?;
    r.finish()?;
    Ok(())
}

fn invalid(c: &mut dyn Component, bytes: &[u8]) -> &'static str {
    match restore(c, bytes) {
        Err(RestoreError::InvalidState(what)) => what,
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_toy_names_its_type_and_snapshot_schema() {
    let toys: [(Box<dyn Component>, &str, u32); 4] = [
        (
            Box::new(ToyCpu::new(cpu_config())),
            "toy.cpu",
            cpu::SNAPSHOT_SCHEMA,
        ),
        (
            Box::new(ToyDma::new(dma_config())),
            "toy.dma",
            dma::SNAPSHOT_SCHEMA,
        ),
        (
            Box::new(ToyMemory::new(memory_config())),
            "toy.memory",
            memory::SNAPSHOT_SCHEMA,
        ),
        (
            Box::new(ToyBus::new(bus_config(0))),
            "toy.bus",
            bus::SNAPSHOT_SCHEMA,
        ),
    ];
    for (toy, name, schema) in toys {
        assert_eq!(toy.type_name(), name);
        assert_eq!(toy.snapshot_schema_version(), schema);
    }
}

/// A toy to snapshot, one configured the same, one configured otherwise, and the error
/// the last one gives.
type ConfigCase = (
    Box<dyn Component>,
    Box<dyn Component>,
    Box<dyn Component>,
    &'static str,
);

#[test]
fn a_snapshot_restores_only_into_the_same_configuration() {
    let cases: [ConfigCase; 4] = [
        (
            Box::new(ToyCpu::new(cpu_config())),
            Box::new(ToyCpu::new(cpu_config())),
            Box::new(ToyCpu::new(ToyCpuConfig {
                write_percent: 51,
                ..cpu_config()
            })),
            "toy cpu: snapshot was taken with a different configuration",
        ),
        (
            Box::new(ToyDma::new(dma_config())),
            Box::new(ToyDma::new(dma_config())),
            Box::new(ToyDma::new(ToyDmaConfig {
                slots: SLOTS + 1,
                ..dma_config()
            })),
            "toy dma: snapshot was taken with a different configuration",
        ),
        (
            Box::new(ToyMemory::new(memory_config())),
            Box::new(ToyMemory::new(memory_config())),
            Box::new(ToyMemory::new(ToyMemoryConfig {
                write_latency: Duration::from_ns(6),
                ..memory_config()
            })),
            "toy memory: snapshot was taken with a different configuration",
        ),
        (
            Box::new(ToyBus::new(bus_config(0))),
            Box::new(ToyBus::new(bus_config(0))),
            Box::new(ToyBus::new(bus_config(1))),
            "toy bus: snapshot was taken with a different configuration",
        ),
    ];
    for (from, mut same, mut other, error) in cases {
        let bytes = snapshot(from.as_ref());
        restore(same.as_mut(), &bytes).unwrap();
        assert_eq!(snapshot(same.as_ref()), bytes);
        assert_eq!(invalid(other.as_mut(), &bytes), error);
    }
}

/// A DMA state: `next_txn`, the burst slot, and the outstanding writes by `TxnId`.
fn dma_state(next_txn: u64, burst_slot: u32, outstanding: &[u64]) -> Vec<u8> {
    let fresh = snapshot(&ToyDma::new(dma_config()));
    // The configuration: clock, ops, max_outstanding, max_burst, max_gap_cycles, base,
    // access_len, slots.
    let config = 4 + 8 + 4 + 8 + 8 + 8 + 4 + 4;
    let mut e = Encoder::new();
    e.raw(&fresh[..config]);
    e.u64(next_txn - outstanding.len() as u64);
    e.u64(next_txn - outstanding.len() as u64);
    e.u64(next_txn);
    e.u32(burst_slot);
    e.u64(1);
    e.bool(true);
    e.len(outstanding.len());
    for txn in outstanding {
        e.u64(*txn);
        e.u64(64 + 8 * (txn % u64::from(SLOTS)));
    }
    e.u64(0xC0FFEE);
    e.into_bytes()
}

#[test]
fn a_dma_restores_exactly_the_states_a_run_can_reach() {
    let mut dma = ToyDma::new(dma_config());
    let good = dma_state(5, SLOTS - 1, &[3, 4]);
    restore(&mut dma, &good).unwrap();
    assert_eq!(snapshot(&dma), good);
    let mut dma = ToyDma::new(dma_config());
    assert_eq!(
        invalid(&mut dma, &dma_state(5, SLOTS, &[])),
        "toy dma: burst slot out of range"
    );
    for outstanding in [&[4, 4][..], &[4, 3], &[5]] {
        assert_eq!(
            invalid(&mut dma, &dma_state(5, 0, outstanding)),
            "toy dma: outstanding writes out of order",
            "{outstanding:?}"
        );
    }
}

/// A bus state with empty queues: `next_downstream` and the routes by downstream `TxnId`.
fn bus_state(next_downstream: u64, routes: &[u64]) -> Vec<u8> {
    let mut e = Encoder::new();
    e.u32(0);
    e.u64(next_downstream);
    e.u16(1);
    e.bool(false);
    e.len(0);
    e.len(0);
    e.len(routes.len());
    for downstream in routes {
        e.u64(*downstream);
        e.u16(0);
        e.u64(downstream + 100);
        e.bool(false);
    }
    e.into_bytes()
}

#[test]
fn a_bus_routes_only_allocated_downstream_txns_in_order() {
    let mut bus = ToyBus::new(bus_config(0));
    let good = bus_state(5, &[3, 4]);
    restore(&mut bus, &good).unwrap();
    assert_eq!(snapshot(&bus), good);
    assert_eq!(
        bus.inspect().fields,
        [
            ("queued_cpu", Value::U64(0)),
            ("queued_dma", Value::U64(0)),
            ("priority", Value::U64(1)),
            ("next_downstream", Value::U64(5)),
            ("routes", Value::U64(2)),
        ]
    );
    for routes in [&[4, 4][..], &[4, 3], &[5]] {
        assert_eq!(
            invalid(&mut bus, &bus_state(5, routes)),
            "toy bus: routes out of order or not yet allocated",
            "{routes:?}"
        );
    }
}
