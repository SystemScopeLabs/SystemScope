//! `m5-reference`, the M5 reference platform (`docs/m5-design.md` §12.1).
//!
//! ```text
//! soc.cpu0     Rv32iCpu, profile M5 (clock "cpu", 100 MHz), entry = firmware entry
//! soc.bus      MultiMasterBus, masters [cpu, dma0, kernel0], clock cpu
//!   ├─ ram     ─▶ soc.ram     Ram                  base 0x8000_0000, size 16 MiB
//!   ├─ uart    ─▶ soc.uart    SimpleUart           base 0x1000_0000, size 0x8
//!   ├─ irqc    ─▶ soc.irqc    SimpleIrqController  base 0x1000_1000, size 0x8, 2 sources
//!   ├─ blk     ─▶ soc.blk     DmaBlockController   base 0x1000_2000, size 0x20
//!   ├─ kgate   ─▶ soc.kernel  ModeledKernel (gate) base 0x1000_3000, size 0x8
//!   └─ timer   ─▶ soc.timer   SimpleTimer          base 0x1000_4000, size 0xC
//! soc.disk     SimpleBlockMedia  capacity 256 blocks, latency Cycles { cpu, 3000 }
//! soc.kernel   ModeledKernel with M5Config, mem ─▶ bus kernel0, booting from soc.disk
//! soc.timer    SimpleTimer, clock cpu, irq ─▶ soc.irqc src1
//! soc.irq_low  TiedLowIrq ─▶ soc.cpu0 irq
//! ```
//!
//! - It is `m3-reference` ([`crate::m3ref`]) with the M5 additions. Components are
//!   declared in that order: the first eight keep their `m3-reference` ids, then
//!   [`TIMER`] and [`IRQ_LOW`].
//! - Links, in order: the `m3-reference` links in `m3-reference` order, except that the
//!   controller's `cpu` output drives the CPU's `sirq` (the S-level external interrupt,
//!   §9) instead of `irq`; then bus `timer` ↔ `soc.timer` `mem`, `soc.timer` `irq` →
//!   `soc.irqc` `src1`, and `soc.irq_low` `irq` → `soc.cpu0` `irq`, the M-level line,
//!   which never asserts. Every link is `Cycles { cpu, 1 }`.
//! - Source 0 is the block controller, its interrupt now used; source 1 is the timer.
//! - The media answers after [`DISK_LATENCY_CYCLES`] (`m3-reference`'s answers after 16):
//!   long enough that the second reader's request queues behind the first, and that the
//!   kernel idles once every other process is done or blocked.
//! - The kernel is M3's with an [`M5Config`] ([`m5_config`]): the controller and timer
//!   windows, the two sources, [`SLICE`], the shared page at [`SHARED`] mapped at
//!   [`SHARED_VA`], and the [`FINAL_WORDS`] words dumped at shutdown. The physical layout
//!   is otherwise `m3-reference`'s; the shared page lies between staging and the frame
//!   pool.
//!
//! [`build`] checks `m3-reference`'s builder invariants, then M5's: the kernel's
//! controller and timer windows are the bus's, and its sources are the ones wired, both
//! below the controller's source count. Everything else is left to the components.
//!
//! [`storage_image`] is `m3-reference`'s storage image with data blocks written at fixed
//! LBAs past the executables, for `blk_read`.

use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_elf::{BLOCK_SIZE, LoadImage, parse_exec_table};
use systemscope_os::{KernelConfig, M5Config, ModeledKernel, Window};
use systemscope_platform::{
    BlockMediaConfig, DmaBlockController, DmaBlockControllerConfig, IrqControllerConfig,
    MultiMasterBus, MultiMasterBusConfig, Ram, RamConfig, Region, SimpleBlockMedia,
    SimpleIrqController, SimpleTimer, SimpleUart, TimerConfig, UartConfig, dma, irqc, timer, uart,
};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::scheduler::SchedulerConfig;
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

use crate::m2ref::media_image;
use crate::m3ref::{
    self, BLK_BASE, DISK_BLOCKS, IRQC_BASE, KGATE_BASE, KGATE_SIZE, STAGING_SIZE, ram_image,
};
use crate::runner::{CPU_HZ, TiedLowIrq};
use crate::{MAX_INSTRUCTIONS, RAM_BASE, RAM_SIZE, UART_BASE};

pub use crate::m3ref::{BLK, BUS, CPU, DISK, IRQC, KERNEL, RAM, UART};

/// `soc.timer`.
pub const TIMER: ComponentId = ComponentId(8);
/// `soc.irq_low`.
pub const IRQ_LOW: ComponentId = ComponentId(9);

/// The number of links, in declaration order.
pub const LINKS: u32 = 14;
/// The targets whose response latency perturbation A may override.
pub const TARGETS: [&str; 7] = [
    "soc.ram",
    "soc.uart",
    "soc.irqc",
    "soc.blk",
    "soc.disk",
    "soc.kernel",
    "soc.timer",
];

/// The component paths, by id.
pub const PATHS: [&str; 10] = [
    "soc.cpu0",
    "soc.bus",
    "soc.ram",
    "soc.uart",
    "soc.irqc",
    "soc.blk",
    "soc.disk",
    "soc.kernel",
    "soc.timer",
    "soc.irq_low",
];

/// The bus regions, by region index: `m3-reference`'s, then the timer.
pub const REGIONS: [&str; 6] = ["ram", "uart", "irqc", "blk", "kgate", "timer"];

/// The base of `soc.timer`; its window is [`timer::SIZE`] bytes.
pub const TIMER_BASE: u32 = 0x1000_4000;
/// The IRQ controller's sources.
pub const IRQ_SOURCES: u8 = 2;
/// The block controller's interrupt source.
pub const BLK_SOURCE: u8 = 0;
/// The timer's interrupt source.
pub const TIMER_SOURCE: u8 = 1;
/// The time slice, in CPU cycles: the timer's `COMPARE` while armed.
pub const SLICE: u32 = 8_000;
/// The timer's `COMPARE` at reset; the kernel replaces it with [`SLICE`] before arming.
pub const COMPARE_RESET: u32 = SLICE;
/// The shared page's physical address: between staging and the frame pool.
pub const SHARED: u32 = 0x8030_0000;
/// The shared page's VA in every process.
pub const SHARED_VA: u32 = 0xF000;
/// The shared words a shutdown dumps: the counter, the mailbox's data, and its flag.
pub const FINAL_WORDS: u32 = 3;
/// The media latency, in CPU cycles.
pub const DISK_LATENCY_CYCLES: u64 = 3_000;
/// The first LBA of the data blocks `reader` reads.
pub const DATA_LBA: u32 = 200;
/// The number of data blocks, one per `reader`.
pub const DATA_BLOCKS: u32 = 2;
/// The CPU profile.
pub const PROFILE: Rv32iProfile = Rv32iProfile::M5;

/// What [`build`] can vary. [`Config::frozen`] is `m5-reference`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Everything `m3-reference` varies. Its latencies are checked against this
    /// platform's links and targets.
    pub m3: m3ref::Config,
    /// The kernel's M5 configuration.
    pub m5: M5Config,
    /// The timer's `COMPARE` at reset.
    pub compare_reset: NonZeroU32,
    /// The CPU's instruction limit: a run that never shuts down stops at it.
    pub max_instructions: NonZeroU64,
}

impl Config {
    /// `m5-reference` as §12.1 freezes it.
    pub fn frozen() -> Config {
        Config {
            m3: m3ref::Config::frozen(),
            m5: m5_config(),
            compare_reset: NonZeroU32::new(COMPARE_RESET).expect("nonzero"),
            max_instructions: NonZeroU64::new(MAX_INSTRUCTIONS).expect("nonzero"),
        }
    }
}

/// The kernel's M5 configuration on `m5-reference`.
pub fn m5_config() -> M5Config {
    M5Config {
        irqc: Window {
            base: u64::from(IRQC_BASE),
            size: irqc::SIZE,
        },
        timer: Window {
            base: u64::from(TIMER_BASE),
            size: timer::SIZE,
        },
        blk_source: BLK_SOURCE,
        timer_source: TIMER_SOURCE,
        slice: SLICE,
        shared: u64::from(SHARED),
        shared_va: SHARED_VA,
        final_words: FINAL_WORDS,
        defect: None,
    }
}

/// The kernel's configuration on `m5-reference`: `m3-reference`'s.
pub fn kernel_config(clock: ClockDomainId) -> KernelConfig {
    m3ref::kernel_config(clock)
}

/// Why [`build`] refused a platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// An `m3-reference` invariant, or a component's error.
    M3(m3ref::BuildError),
    /// The kernel's interrupt controller window is not the bus's.
    IrqcMismatch(Window),
    /// The kernel's timer window is not the bus's.
    TimerMismatch(Window),
    /// The kernel's sources are not the ones wired.
    SourceMismatch {
        /// The kernel's block controller source.
        blk: u8,
        /// The kernel's timer source.
        timer: u8,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::M3(e) => e.fmt(f),
            BuildError::IrqcMismatch(w) => write!(
                f,
                "the kernel's interrupt controller {:#x} + {:#x} is not the bus's irqc",
                w.base, w.size
            ),
            BuildError::TimerMismatch(w) => write!(
                f,
                "the kernel's timer {:#x} + {:#x} is not the bus's timer",
                w.base, w.size
            ),
            BuildError::SourceMismatch { blk, timer } => write!(
                f,
                "the kernel's sources (block controller {blk}, timer {timer}) are not the \
                 wired ones ({BLK_SOURCE}, {TIMER_SOURCE})"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<m3ref::BuildError> for BuildError {
    fn from(e: m3ref::BuildError) -> BuildError {
        BuildError::M3(e)
    }
}

/// The platform's builder invariants, before anything is built.
fn check(config: &Config) -> Result<(), BuildError> {
    m3ref::check(&config.m3)?;
    let m5 = &config.m5;
    let irqc = Window {
        base: u64::from(IRQC_BASE),
        size: irqc::SIZE,
    };
    if m5.irqc != irqc {
        return Err(BuildError::IrqcMismatch(m5.irqc));
    }
    let timer = Window {
        base: u64::from(TIMER_BASE),
        size: timer::SIZE,
    };
    if m5.timer != timer {
        return Err(BuildError::TimerMismatch(m5.timer));
    }
    if (m5.blk_source, m5.timer_source) != (BLK_SOURCE, TIMER_SOURCE) {
        return Err(BuildError::SourceMismatch {
            blk: m5.blk_source,
            timer: m5.timer_source,
        });
    }
    Ok(())
}

/// `m5-reference` with `firmware` in RAM and the raw disk image `disk`, elaborated.
pub fn build(firmware: &LoadImage, disk: &[u8], config: &Config) -> Result<Runtime, BuildError> {
    check(config)?;
    let m3 = &config.m3;
    m3.latencies
        .check(LINKS, &TARGETS)
        .map_err(m3ref::BuildError::Latencies)?;

    let mut t = TopologyBuilder::new(SimulationClock::default());
    let cpu_clock = t
        .add_clock(
            Frequency::from_hz(CPU_HZ).expect("100 MHz is a valid frequency"),
            Tick::ZERO,
            Rounding::Floor,
        )
        .expect("100 MHz divides the simulation clock");
    let respond = |path: &str| LinkLatency::Cycles {
        domain: cpu_clock,
        k: m3.latencies.respond(path, 0),
    };
    let link = |index: u32| {
        Some(LinkLatency::Cycles {
            domain: cpu_clock,
            k: m3.latencies.link(index, 1),
        })
    };

    let kernel = ModeledKernel::with_disk_m5(
        KernelConfig {
            clock: cpu_clock,
            latency: respond(PATHS[7]),
            ..m3.kernel
        },
        m3.boot,
        config.m5,
    )
    .map_err(m3ref::BuildError::Kernel)?;
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock: cpu_clock,
        entry: firmware.entry,
        max_instructions: config.max_instructions,
        profile: PROFILE,
    })
    .expect("the loader guarantees an aligned entry");
    let region = |name, base: u32, size| Region {
        name,
        base: u64::from(base),
        size,
    };
    let bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: m3ref::MASTERS.to_vec(),
        regions: vec![
            region(REGIONS[0], RAM_BASE, u64::from(RAM_SIZE)),
            region(REGIONS[1], UART_BASE, uart::SIZE),
            region(REGIONS[2], IRQC_BASE, irqc::SIZE),
            region(REGIONS[3], BLK_BASE, dma::SIZE),
            region(REGIONS[4], KGATE_BASE, KGATE_SIZE),
            region(REGIONS[5], TIMER_BASE, timer::SIZE),
        ],
        clock: cpu_clock,
    })
    .map_err(m3ref::BuildError::Bus)?;
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency: respond(PATHS[2]),
        },
        &ram_image(firmware),
    )
    .map_err(m3ref::BuildError::Ram)?;
    let device = SimpleUart::new(UartConfig {
        latency: respond(PATHS[3]),
    });
    let irqc = SimpleIrqController::new(IrqControllerConfig {
        sources: IRQ_SOURCES,
        latency: respond(PATHS[4]),
    })
    .map_err(m3ref::BuildError::Irqc)?;
    let blk = DmaBlockController::new(DmaBlockControllerConfig {
        clock: cpu_clock,
        latency: respond(PATHS[5]),
        capacity_blocks: m3.controller_blocks,
        dma_base: m3.dma_base,
        dma_size: m3.dma_size,
    })
    .map_err(m3ref::BuildError::Controller)?;
    let disk = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: m3.media_blocks,
            latency: LinkLatency::Cycles {
                domain: cpu_clock,
                k: m3.latencies.respond(PATHS[6], DISK_LATENCY_CYCLES),
            },
            bad_blocks: m3.bad_blocks.clone(),
        },
        &media_image(disk),
    )
    .map_err(m3ref::BuildError::Media)?;
    let timer = SimpleTimer::new(TimerConfig {
        clock: cpu_clock,
        latency: respond(PATHS[8]),
        compare_reset: config.compare_reset,
    });

    let cpu = t.add_component(PATHS[0], Box::new(cpu));
    let bus = t.add_component(PATHS[1], Box::new(bus));
    let ram = t.add_component(PATHS[2], Box::new(ram));
    let device = t.add_component(PATHS[3], Box::new(device));
    let irqc = t.add_component(PATHS[4], Box::new(irqc));
    let blk = t.add_component(PATHS[5], Box::new(blk));
    let disk = t.add_component(PATHS[6], Box::new(disk));
    let kernel = t.add_component(PATHS[7], Box::new(kernel));
    let timer = t.add_component(PATHS[8], Box::new(timer));
    let low = t.add_component(PATHS[9], Box::new(TiedLowIrq));

    t.connect((cpu, "mem"), (bus, m3ref::MASTERS[0]), link(0));
    t.connect((bus, REGIONS[0]), (ram, "mem"), link(1));
    t.connect((bus, REGIONS[1]), (device, "mem"), link(2));
    t.connect((bus, REGIONS[2]), (irqc, "mem"), link(3));
    t.connect((bus, REGIONS[3]), (blk, "mem"), link(4));
    t.connect((blk, "dma"), (bus, m3ref::MASTERS[1]), link(5));
    t.connect((blk, "blk"), (disk, "blk"), link(6));
    t.connect((blk, "irq"), (irqc, "src0"), link(7));
    t.connect((irqc, "cpu"), (cpu, "sirq"), link(8));
    t.connect((bus, REGIONS[4]), (kernel, "gate"), link(9));
    t.connect((kernel, "mem"), (bus, m3ref::MASTERS[2]), link(10));
    t.connect((bus, REGIONS[5]), (timer, "mem"), link(11));
    t.connect((timer, "irq"), (irqc, "src1"), link(12));
    t.connect((low, "irq"), (cpu, "irq"), link(13));
    t.elaborate(SessionConfig {
        seed: m3.seed,
        scheduler: SchedulerConfig {
            ties: m3.ties,
            ..SchedulerConfig::default()
        },
        jitter: m3.jitter.clone(),
        script: m3.script.clone(),
    })
    .map_err(|e| BuildError::M3(m3ref::BuildError::Elaboration(format!("{e:?}"))))
}

/// The data block at `lba`: byte `i` is `(7 * i + lba) mod 256`.
pub fn data_block(lba: u32) -> [u8; BLOCK_SIZE] {
    std::array::from_fn(|i| (7 * i as u32).wrapping_add(lba) as u8)
}

/// The storage image of `files`, as [`m3ref::storage_image`] builds it, with
/// [`data_block`] written at each of `data_blocks` blocks from `data_lba`. A data block
/// must lie past every executable and inside the capacity.
pub fn storage_image(
    files: &[&[u8]],
    data_lba: u32,
    data_blocks: u32,
    capacity_blocks: u32,
    staging_size: u32,
) -> Result<Vec<u8>, String> {
    let mut disk = m3ref::storage_image(files, capacity_blocks, staging_size)?;
    let block0: &[u8; BLOCK_SIZE] = disk[..BLOCK_SIZE].try_into().expect("one block");
    let table = parse_exec_table(block0, capacity_blocks, staging_size)
        .map_err(|e| format!("the table is invalid: {e}"))?;
    let used = table
        .entries
        .iter()
        .map(|e| e.start_lba + e.blocks())
        .max()
        .unwrap_or(1);
    let end = data_lba
        .checked_add(data_blocks)
        .ok_or("the data blocks wrap")?;
    if data_lba < used {
        return Err(format!(
            "the data blocks from LBA {data_lba} overlap the executables, which end at {used}"
        ));
    }
    if end > capacity_blocks {
        return Err(format!(
            "the data blocks end at LBA {end}, past the capacity {capacity_blocks}"
        ));
    }
    for lba in data_lba..end {
        let at = lba as usize * BLOCK_SIZE;
        disk[at..at + BLOCK_SIZE].copy_from_slice(&data_block(lba));
    }
    Ok(disk)
}

/// The `m5-reference` disk of `files`: [`storage_image`] with the frozen data blocks.
pub fn disk_image(files: &[&[u8]]) -> Result<Vec<u8>, String> {
    storage_image(
        files,
        DATA_LBA,
        DATA_BLOCKS,
        DISK_BLOCKS as u32,
        STAGING_SIZE as u32,
    )
}

#[cfg(test)]
mod tests {
    use systemscope_os::UserLayout;

    use super::*;

    #[test]
    fn the_data_pattern_depends_on_the_lba() {
        let b = data_block(200);
        assert_eq!(b[0], 200);
        assert_eq!(b[1], 207);
        assert_eq!(b[8], (200 + 56) as u8);
        assert_ne!(data_block(201), b);
    }

    #[test]
    fn the_layout_is_frozen() {
        let config = Config::frozen();
        assert_eq!(check(&config), Ok(()));
        assert!(
            config
                .m5
                .validate(&config.m3.kernel, &UserLayout::M3)
                .is_ok()
        );
        assert_eq!(config.m3.boot.layout, UserLayout::M3);
    }
}
