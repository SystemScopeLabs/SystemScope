//! The `M5` CPU behind a `SimpleTimer` and a `SimpleIrqController` in the real runtime
//! (`docs/m5-design.md` §27.2, M5.2): the timer's `irq` on the controller's `src0`, the
//! controller's `cpu` on the CPU's `sirq`, and both windows on the address bus next to the
//! RAM. Every `irq.v0` link has latency `Cycles { cpu, 1 }`.
//!
//! The program delegates SEI to S, enables `sie.SEIE` and the controller's source, arms
//! the timer one-shot, and drops to U, where it spins. The S handler clears `EXPIRED` and
//! returns with `SRET`. The tests check that the expiry reaches `mip.SEIP` and is taken from
//! U into S, and that checkpoints at every event boundary resume identically.

mod common;

use std::cell::RefCell;
use std::num::{NonZeroU32, NonZeroU64};
use std::rc::Rc;

use common::asm::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::irq_v0;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, Value};
use systemscope_platform::timer::{self, CONTROL, EXPIRE_KIND, STATUS};
use systemscope_platform::{
    AddressBus, IrqControllerConfig, Ram, RamConfig, RamImage, Region, Segment,
    SimpleIrqController, SimpleTimer, TimerConfig, irqc,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::INTERRUPT_KIND;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

const RAM_BASE: u32 = 0x8000_0000;
const RAM_SIZE: u32 = 0x1000;
const U_LOOP: u32 = RAM_BASE + 0x100;
const S_HANDLER: u32 = RAM_BASE + 0x200;
/// The controller's window, as in `m2-reference`.
const IRQC_BASE: u32 = 0x1000_1000;
const TIMER_BASE: u32 = 0x1000_2000;
/// The timer's `COMPARE`, in CPU cycles.
const COMPARE: i32 = 40;
const SEIP: u64 = 1 << 9;

const CPU: ComponentId = ComponentId(0);
const IRQC: ComponentId = ComponentId(3);
const TIMER: ComponentId = ComponentId(4);

const MRET: u32 = 0x3020_0073;
const SRET: u32 = 0x1020_0073;

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}

/// In M: `stvec`, `mideleg.SEIP`, `sie.SEIE`, the controller's `ENABLE`, the timer's
/// `COMPARE` and `CONTROL.ENABLE`, then `MRET` to U, which counts in `x5` forever. The S
/// handler counts in `x6`, clears `EXPIRED`, and returns.
fn program() -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let main = vec![
        lui(1, RAM_BASE >> 12),
        addi(1, 1, 0x200),
        csr_word(1, 0, 1, 0x105), // csrrw x0, stvec, x1
        addi(2, 0, 0x200),
        csr_word(1, 0, 2, 0x303), // csrrw x0, mideleg, x2
        csr_word(2, 0, 2, 0x104), // csrrs x0, sie, x2
        lui(3, IRQC_BASE >> 12),
        addi(4, 0, 1),
        sw(4, 3, irqc::ENABLE as i32),
        lui(8, TIMER_BASE >> 12),
        addi(4, 0, COMPARE),
        sw(4, 8, timer::COMPARE as i32),
        addi(4, 0, timer::ENABLE as i32),
        sw(4, 8, CONTROL as i32), // arms
        lui(9, 2),
        addi(9, 9, -0x800),
        csr_word(3, 0, 9, 0x300), // csrrc x0, mstatus, x9: MPP = U
        lui(1, RAM_BASE >> 12),
        addi(1, 1, 0x100),
        csr_word(1, 0, 1, 0x341), // csrrw x0, mepc, x1
        MRET,
    ];
    let u_loop = vec![addi(5, 5, 1), jal(0, -4)];
    let handler = vec![
        addi(6, 6, 1),
        addi(4, 0, timer::EXPIRED as i32),
        sw(4, 8, STATUS as i32),
        SRET,
    ];
    (main, u_loop, handler)
}

/// A stateless `irq.v0` source that never sends, on the CPU's `irq`: this SoC has no
/// machine external interrupt.
struct TiedLow;

impl Component for TiedLow {
    fn type_name(&self) -> &'static str {
        "test.irq_tied_low"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "irq",
            protocol: irq_v0::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Err(SimError::ComponentFault(
            "tied low: a source receives nothing",
        ))
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// The CPU's and the timer's views after every event.
struct Recorder(Rc<RefCell<Vec<(StateView, StateView)>>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        self.0
            .borrow_mut()
            .push((world.inspect(CPU).unwrap(), world.inspect(TIMER).unwrap()));
        Control::Continue
    }
}

/// CPU, bus (RAM, controller, timer), RAM, controller (1 source), timer, tied-low `irq`.
fn build(limit: u64) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock,
        entry: RAM_BASE,
        max_instructions: NonZeroU64::new(limit).unwrap(),
        profile: Rv32iProfile::M5,
    })
    .unwrap();
    let cpu = t.add_component("soc.cpu", Box::new(cpu));
    let bus = AddressBus::new(vec![
        Region {
            name: "ram",
            base: u64::from(RAM_BASE),
            size: u64::from(RAM_SIZE),
        },
        Region {
            name: "irqc",
            base: u64::from(IRQC_BASE),
            size: irqc::SIZE,
        },
        Region {
            name: "timer",
            base: u64::from(TIMER_BASE),
            size: timer::SIZE,
        },
    ])
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let (main, u_loop, handler) = program();
    let bytes = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let segment = |at: u32, words: &[u32]| Segment {
        offset: u64::from(at - RAM_BASE),
        bytes: bytes(words),
    };
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency: cycles(1),
        },
        &RamImage {
            image_hash: [0x5e; 32],
            segments: vec![
                segment(RAM_BASE, &main),
                segment(U_LOOP, &u_loop),
                segment(S_HANDLER, &handler),
            ],
        },
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    let controller = SimpleIrqController::new(IrqControllerConfig {
        sources: 1,
        latency: cycles(0),
    })
    .unwrap();
    let controller = t.add_component("soc.irqc", Box::new(controller));
    let timer = SimpleTimer::new(TimerConfig {
        clock,
        latency: cycles(1),
        compare_reset: NonZeroU32::MAX,
    });
    let timer = t.add_component("soc.timer", Box::new(timer));
    let low = t.add_component("soc.mei_low", Box::new(TiedLow));
    assert_eq!((cpu, controller, timer), (CPU, IRQC, TIMER));
    t.connect((cpu, "mem"), (bus, "cpu"), None);
    t.connect((bus, "ram"), (ram, "mem"), Some(cycles(1)));
    t.connect((bus, "irqc"), (controller, "mem"), Some(cycles(1)));
    t.connect((bus, "timer"), (timer, "mem"), Some(cycles(1)));
    t.connect((timer, "irq"), (controller, "src0"), Some(cycles(1)));
    t.connect((controller, "cpu"), (cpu, "sirq"), Some(cycles(1)));
    t.connect((low, "irq"), (cpu, "irq"), Some(cycles(1)));
    t.elaborate(SessionConfig::default()).unwrap()
}

struct Run {
    events: Vec<Dispatched>,
    views: Vec<(StateView, StateView)>,
    trace: Trace,
}

fn run(limit: u64) -> Run {
    let views = Rc::default();
    let mut rt = build(limit);
    rt.add_observer(Box::new(Recorder(Rc::clone(&views))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    Run {
        events,
        views: views.take(),
        trace: rt.take_trace().unwrap(),
    }
}

fn u(view: &StateView, name: &str) -> u64 {
    match view.get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

impl Run {
    /// The event index of each `component` record of `kind`, with its fields.
    fn records(
        &self,
        component: ComponentId,
        kind: &str,
    ) -> Vec<(usize, Vec<(&'static str, Value)>)> {
        self.trace
            .records
            .iter()
            .filter(|r| r.component == component && r.kind == kind)
            .map(|r| {
                let TraceAt::Event(key) = r.at else {
                    panic!("{r:?}")
                };
                let i = self.events.iter().position(|e| e.key == key).unwrap();
                (i, r.fields.clone())
            })
            .collect()
    }
}

fn field<'a>(fields: &'a [(&str, Value)], name: &str) -> &'a Value {
    &fields.iter().find(|f| f.0 == name).unwrap().1
}

/// The expiry raises `mip.SEIP`, which is taken from U into S at the S handler; the
/// handler's `EXPIRED` clear drops it again, and U runs on.
#[test]
fn a_timer_expiry_is_taken_from_u_into_s() {
    let r = run(120);
    let expiries = r.records(TIMER, EXPIRE_KIND);
    assert_eq!(expiries.len(), 1, "one-shot");
    let entries = r.records(CPU, INTERRUPT_KIND);
    assert!(!entries.is_empty());
    for (at, fields) in &entries {
        assert!(*at > expiries[0].0);
        assert_eq!(field(fields, "cause"), &Value::U64(0x8000_0009));
        assert_eq!(field(fields, "handler"), &Value::U64(u64::from(S_HANDLER)));
        assert_eq!(field(fields, "to"), &Value::Str("S".into()));
    }
    assert_eq!(field(&entries[0].1, "from"), &Value::Str("U".into()));
    // `mip.SEIP` is clear until the expiry and set before the first entry.
    let first = entries[0].0;
    assert!(
        r.views[..expiries[0].0]
            .iter()
            .all(|(cpu, _)| u(cpu, "mip") & SEIP == 0)
    );
    assert_ne!(u(&r.views[first - 1].0, "mip") & SEIP, 0);
    let (cpu, timer) = r.views.last().unwrap();
    assert_eq!(u(cpu, "mip") & SEIP, 0);
    assert_eq!(u(cpu, "x6"), entries.len() as u64);
    assert_eq!(timer.get("expired"), Some(&Value::Bool(false)));
    // U ran on after the last return.
    assert!(u(cpu, "x5") > u(&r.views[entries.last().unwrap().0].0, "x5"));
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let limit = 80;
    let reference = {
        let mut rt = build(limit);
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        (events, rt.state_digest().unwrap(), rt.execution_digest())
    };
    for k in 0..=reference.0.len() {
        let mut rt = build(limit);
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let mut fresh = build(limit);
        fresh.restore(&bytes).unwrap();
        let rest: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None);
        assert_eq!(rest, reference.0[k..], "checkpoint after {k} events");
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.1, reference.2),
            "checkpoint after {k} events"
        );
    }
}
