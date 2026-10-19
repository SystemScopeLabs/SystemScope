//! The M5 kernel additions (`docs/m5-design.md` §11): interrupt traps, preemption, the
//! blocking `blk_read`, the idle state, the shared page, and the shutdown's dump.
//!
//! An M5 kernel is a kernel with processes and an [`M5Config`]. Without one, nothing here
//! runs, `blk_read` returns `-ENOSYS`, and the kernel is M3's, byte for byte.
//!
//! # Operations
//!
//! ```text
//! Boot:       … last creation → Arm → Dispatch(head) | Idle | Disarm …
//! Interrupt:  ReadFrame → Pending → [TimerStatus → TimerAck?] / [BlkStatus → BlkAck →
//!             Fill … → BlkCommand?] per pending source, in source order
//!             → Dispatch (preempted, or woken from idle) | release (resume unchanged)
//! blk_read:   ReadFrame → BlkWalk … → BlkCommand? → Dispatch(head) | Idle
//!                                  → Return (-EFAULT)
//! Shutdown:   Disarm → Final → Shutdown
//! ```
//!
//! - **Arm** zeroes the shared page, enables both sources at the interrupt controller,
//!   sets the block controller's `IRQ_ENABLE`, and starts the timer periodic with `SLICE`
//!   cycles. It runs once, after boot's last creation and before its first dispatch.
//! - **Pending** reads the controller's `PENDING`. Its sources, masked to the two the
//!   kernel enabled, are traced as `os.interrupt` and handled lowest source first. The bits
//!   are only candidates: under jitter an interrupt can be taken after the condition that
//!   raised it is gone, so each source is checked at its device before it is handled.
//! - **TimerStatus** reads the timer's `STATUS`. Only `EXPIRED` leads to **TimerAck**,
//!   which clears it and marks the interrupted process for preemption.
//! - **BlkStatus** reads the block controller's `STATUS`. Only `DONE` with a request in
//!   flight leads to **BlkAck**, which writes `ACK`. Then, without an error, **Fill**
//!   copies the block from staging to the requester's buffer, one chunk of at most 16
//!   bytes (crossing no page on either side) per read and write. The requester wakes with
//!   0, or `-EIO` after a controller error, and the next request, if any, is commanded
//!   (**BlkCommand**).
//! - After the last source: a preempted process goes to the queue's tail and the head is
//!   dispatched; a process that was not preempted resumes with no write at all; from idle,
//!   the head is dispatched if a process woke, else the kernel stays idle.
//! - **BlkWalk** checks a `blk_read` buffer, 512 bytes, through the caller's page table as
//!   a U-mode store would ([`walk_step_for`]). The caller then blocks with its context
//!   (`a0` set at the wake, `sepc + 4`), its request joins the FIFO, and if the controller
//!   was idle the request is commanded at once.
//! - **Idle** is the empty run queue with requests in flight: it writes a blocked
//!   process's `satp` and `action = Idle`, and the trampoline spins in S with `SIE` set
//!   until an interrupt. An interrupt from that spin (`SPP = S`) is the only trap the
//!   kernel accepts while idle.
//! - **Disarm** stops the timer (`CONTROL = 0`), so a finished run's event queue drains;
//!   **Final** reads the declared shared words and traces each as `os.final`. Every M5
//!   shutdown runs both before M3's Shutdown.
//!
//! # Kernel state
//!
//! Across operations an M5 kernel keeps, besides M3's: the block request FIFO, head in
//! flight (each request's PID, LBA, buffer, and the buffer's physical pages), and whether
//! it is idle. An interrupt in progress is its stage: where it came from (the interrupted
//! context, or idle), the sources left, and whether the timer asked for preemption.

use std::collections::VecDeque;

use systemscope_contracts::snapshot::SnapshotWriter;
use systemscope_contracts::trace::Value;

use crate::boot::{
    ACK, ACK_DONE, BLOCK_COUNT, COMMAND, IRQ_ENABLE, LBA, MEM_ADDR, OP_READ, STATUS, STATUS_DONE,
    STATUS_ERROR_SHIFT,
};
use crate::config::{FRAME_ACTION, KernelConfig, Window};
use crate::core::{Access, MAX_ACCESS, PAGE, chunks};
use crate::image::{
    BLK_REGISTERS, MMIO_MEGAPAGE, PHYS_LIMIT, PlanError, UserLayout, kernel_megapage, vpn1,
};
use crate::process::{Context, Violation};
use crate::procop::{
    FRAME_SATP, FRAME_SCAUSE, FRAME_SSTATUS, Model, Note, ProcOp, SSTATUS_SPP, Stage, cause_name,
    exit_note, note, read_len, word,
};
use crate::pte;
use crate::syscall::{
    Buffer, EFAULT, EIO, SYS_BLK_READ, WalkStep, neg, pte_address, walk_step_for,
};

/// `scause` of a supervisor external interrupt.
pub const INTERRUPT_CAUSE: u32 = 0x8000_0009;
/// The trap frame's `action` for idle: spin in S with `SIE` set until an interrupt.
pub const ACTION_IDLE: u32 = 2;

/// Offset of the interrupt controller's `PENDING`.
pub const IRQC_PENDING: u64 = 0x0;
/// Offset of the interrupt controller's `ENABLE`.
pub const IRQC_ENABLE: u64 = 0x4;
const IRQC_REGISTERS: u64 = 0x8;
/// Offset of the timer's `COMPARE`.
pub const TIMER_COMPARE: u64 = 0x0;
/// Offset of the timer's `CONTROL`.
pub const TIMER_CONTROL: u64 = 0x4;
/// Offset of the timer's `STATUS`.
pub const TIMER_STATUS: u64 = 0x8;
const TIMER_REGISTERS: u64 = 0xC;
/// `CONTROL.ENABLE`.
pub const TIMER_ENABLE: u32 = 1;
/// `CONTROL.PERIODIC`.
pub const TIMER_PERIODIC: u32 = 2;
/// `STATUS.EXPIRED`, write 1 to clear.
pub const TIMER_EXPIRED: u32 = 1;

/// The bytes one `blk_read` transfers: one block.
pub const BLK_BYTES: u32 = 512;
/// The most shared words a shutdown dumps: the whole page.
pub const MAX_FINAL_WORDS: u32 = (PAGE / 4) as u32;
/// The zeroing writes of the shared page.
const SHARED_ZERO_STEPS: usize = (PAGE / MAX_ACCESS) as usize;

/// Trace kind of a preemption (§11.5): `pid`.
pub const PREEMPT_KIND: &str = "os.process.preempt";
/// Trace kind of a `blk_read` that blocks (§11.5): `pid`, `lba`.
pub const BLOCK_KIND: &str = "os.process.block";
/// Trace kind of a wake (§11.5): `pid`.
pub const WAKE_KIND: &str = "os.process.wake";
/// Trace kind of an interrupt trap (§11.5): `sources`, the pending bits the kernel enabled.
pub const INTERRUPT_KIND: &str = "os.interrupt";
/// Trace kind of a shared word at shutdown (§11.4): `word` (its index), `value`.
pub const FINAL_KIND: &str = "os.final";

/// The snapshot marker of an M5 configuration, after the process source.
const M5_MARKER: u8 = 0xE5;
/// Marks a planted defect after the M5 configuration; absent without one.
const DEFECT_MARKER: u8 = 0xD5;

/// A defect planted in the kernel for M5's campaigns to find (`docs/m5-design.md`
/// §12.3). Only tests set one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Defect {
    /// R5 `kernel-wake-lost`: an interrupt trap taken while idle in which the timer
    /// expired goes back to idle, even when the same trap completed a block request.
    /// The woken process stays `Ready` and is never dispatched: every later idle trap is
    /// a timer tick too.
    IdleTickDropsWake,
}

/// The M5 part of a kernel's configuration (`docs/m5-design.md` §11, §12.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct M5Config {
    /// The interrupt controller's register window.
    pub irqc: Window,
    /// The timer's register window.
    pub timer: Window,
    /// The block controller's interrupt source.
    pub blk_source: u8,
    /// The timer's interrupt source.
    pub timer_source: u8,
    /// The time slice in timer cycles, at least 1.
    pub slice: u32,
    /// The shared page's physical address.
    pub shared: u64,
    /// The shared page's VA in every process: `SHARED_VA`.
    pub shared_va: u32,
    /// The number of shared words, from the first, a shutdown dumps.
    pub final_words: u32,
    /// A planted defect, `None` in every kernel but a campaign's planted race.
    pub defect: Option<Defect>,
}

impl M5Config {
    /// The shared page as a range.
    pub fn shared_page(&self) -> Window {
        Window {
            base: self.shared,
            size: PAGE,
        }
    }

    /// The ranges an M5 kernel grants beyond M3's: the interrupt controller, the timer,
    /// and the shared page.
    pub fn grants(&self) -> [Window; 3] {
        [self.irqc, self.timer, self.shared_page()]
    }

    /// The interrupt sources the kernel enables.
    pub fn sources(&self) -> u32 {
        1 << self.blk_source | 1 << self.timer_source
    }

    /// The shared page as every process maps it: `(SHARED_VA, PPN)`.
    pub fn shared_mapping(&self) -> (u32, u32) {
        (self.shared_va, (self.shared / PAGE) as u32)
    }

    /// Checks the M5 rules beyond the process source's: both device windows non-empty,
    /// inside `u64`, holding their registers, and outside `kgate`; two distinct sources
    /// below 32; a slice of at least 1; a block controller window that holds its
    /// registers, and staging 16-byte aligned and able to hold a block (`blk_read`'s DMA);
    /// the shared page a page-aligned RAM page reachable by Sv32 and outside the trap
    /// frame, staging, and the frame pool; `SHARED_VA` a nonzero page below `USER_BASE`,
    /// outside both megapage slots; and at most a page of final words.
    pub fn validate(&self, config: &KernelConfig, layout: &UserLayout) -> Result<(), PlanError> {
        let err = |rule| Err(PlanError::M5(rule));
        for w in [self.irqc, self.timer] {
            if w.size == 0 || w.base.checked_add(w.size).is_none() {
                return err("a device window is empty or wraps");
            }
            if config.gate.overlaps(w.base, w.size) {
                return err("a device window overlaps kgate");
            }
        }
        if self.irqc.size < IRQC_REGISTERS || self.timer.size < TIMER_REGISTERS {
            return err("a device window does not hold its registers");
        }
        if self.blk_source >= 32 || self.timer_source >= 32 || self.blk_source == self.timer_source
        {
            return err("the sources are not two distinct sources below 32");
        }
        if self.slice == 0 {
            return err("the time slice is zero");
        }
        if config.blk.size < BLK_REGISTERS {
            return err("the block controller window does not hold its registers");
        }
        let staging = config.staging;
        if !staging.base.is_multiple_of(16) || staging.size < u64::from(BLK_BYTES) {
            return err("staging cannot take a block read");
        }
        let shared = self.shared;
        if !shared.is_multiple_of(PAGE)
            || !config.ram.contains(shared, PAGE)
            || shared + PAGE > PHYS_LIMIT
        {
            return err("the shared page is not a page-aligned RAM page reachable by Sv32");
        }
        if [config.frame(), staging, config.frame_pool]
            .iter()
            .any(|w| w.overlaps(shared, PAGE))
        {
            return err("the shared page overlaps the trap frame, staging, or the frame pool");
        }
        let va = self.shared_va;
        if va == 0 || !va.is_multiple_of(PAGE as u32) || va >= layout.user_base {
            return err("SHARED_VA is not a nonzero page below USER_BASE");
        }
        let slot = vpn1(u64::from(va));
        if slot == vpn1(kernel_megapage(config)) || slot == vpn1(MMIO_MEGAPAGE) {
            return err("SHARED_VA is in a megapage slot");
        }
        if self.final_words > MAX_FINAL_WORDS {
            return err("more final words than the shared page holds");
        }
        Ok(())
    }

    /// The canonical encoding, as the snapshot stores it after the process source.
    pub(crate) fn encode(&self, w: &mut SnapshotWriter) {
        w.u8(M5_MARKER);
        for window in [self.irqc, self.timer] {
            w.u64(window.base);
            w.u64(window.size);
        }
        w.u8(self.blk_source);
        w.u8(self.timer_source);
        w.u32(self.slice);
        w.u64(self.shared);
        w.u32(self.shared_va);
        w.u32(self.final_words);
        match self.defect {
            None => {}
            Some(Defect::IdleTickDropsWake) => {
                w.u8(DEFECT_MARKER);
                w.u8(1);
            }
        }
    }
}

/// One `blk_read` request (§11.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Request {
    /// The blocked caller.
    pub pid: u32,
    /// The block.
    pub lba: u32,
    /// The buffer's VA.
    pub buf: u32,
    /// The physical page of each buffer page.
    pub pages: Vec<u32>,
}

/// The M5 kernel state: the configuration, the request FIFO, and the idle flag.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct M5 {
    /// The configuration.
    pub config: M5Config,
    /// The block requests, head in flight at the controller.
    pub requests: VecDeque<Request>,
    /// Whether the run queue emptied with requests in flight and the hart spins in S.
    pub idle: bool,
}

impl M5 {
    /// No request, not idle.
    pub fn new(config: M5Config) -> M5 {
        M5 {
            config,
            requests: VecDeque::new(),
            idle: false,
        }
    }
}

/// What an interrupt trap interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// The running process, with its context from the frame (`sepc` unchanged).
    Running(Context),
    /// The idle spin.
    Idle,
}

/// An interrupt trap in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Irq {
    /// What it interrupted.
    pub from: Origin,
    /// The sources left to handle.
    pub todo: u32,
    /// Whether the timer asked for preemption.
    pub preempt: bool,
}

impl Irq {
    fn new(from: Origin) -> Irq {
        Irq {
            from,
            todo: 0,
            preempt: false,
        }
    }
}

/// What follows a block command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Then {
    /// The `blk_read` of `from`, which just blocked: dispatch the head.
    Dispatch {
        /// The blocked caller.
        from: u32,
    },
    /// The interrupt trap that completed the previous request.
    Irq(Irq),
}

/// One M5 stage.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum M5Stage {
    /// Zeroing the shared page and arming the devices, after boot.
    Arm,
    /// Reading `PENDING`.
    Pending {
        /// The trap.
        irq: Irq,
    },
    /// Reading the timer's `STATUS`.
    TimerStatus {
        /// The trap.
        irq: Irq,
    },
    /// Clearing `EXPIRED`.
    TimerAck {
        /// The trap.
        irq: Irq,
    },
    /// Reading the block controller's `STATUS`.
    BlkStatus {
        /// The trap.
        irq: Irq,
    },
    /// Writing `ACK` for the head request, which ended with `error`.
    BlkAck {
        /// The trap.
        irq: Irq,
        /// `STATUS.ERROR`: 0 for success.
        error: u8,
    },
    /// Copying the head request's block to its buffer, the chunk at `done`.
    Fill {
        /// The trap.
        irq: Irq,
        /// The buffer's VA.
        buf: u32,
        /// The buffer's physical pages.
        pages: Vec<u32>,
        /// The bytes copied.
        done: u32,
    },
    /// Commanding the head request: `LBA`, `MEM_ADDR`, `BLOCK_COUNT`, `COMMAND = READ`.
    BlkCommand {
        /// The head's block.
        lba: u32,
        /// What follows.
        then: Then,
    },
    /// Reading the level-`level` PTE for page `pages.len()` of a `blk_read` buffer.
    BlkWalk {
        /// The block.
        lba: u32,
        /// The buffer, 512 bytes.
        buffer: Buffer,
        /// The physical pages found so far.
        pages: Vec<u32>,
        /// The table the PTE is in.
        table: u32,
        /// 1 or 0.
        level: u8,
        /// The caller's context to block with: `sepc + 4`.
        context: Context,
    },
    /// Writing `satp` and `action = Idle`.
    Idle {
        /// A blocked process's `satp`.
        satp: u32,
    },
    /// Stopping the timer before a shutdown with `reason`.
    Disarm {
        /// The shutdown reason.
        reason: u32,
    },
    /// Reading the final shared words before a shutdown with `reason`.
    Final {
        /// The shutdown reason.
        reason: u32,
    },
}

/// A 4-byte register write.
fn write32(addr: u64, value: u32) -> Access {
    Access::Write {
        addr,
        data: value.to_le_bytes().to_vec(),
    }
}

/// A 4-byte register read.
fn read32(addr: u64) -> Access {
    Access::Read { addr, len: 4 }
}

/// The Fill chunk at `done`: the staging address, the buffer VA, and the length, at most
/// 16 bytes that cross no page on either side.
pub fn fill_chunk(staging: u64, buf: u32, done: u32) -> (u64, u32, u32) {
    let src = staging + u64::from(done);
    let va = buf + done;
    let page = PAGE as u32;
    let len = (BLK_BYTES - done)
        .min(MAX_ACCESS as u32)
        .min(page - va % page)
        .min((PAGE - src % PAGE) as u32);
    (src, va, len)
}

/// Whether `done` is where a Fill chunk starts.
pub fn is_fill_start(staging: u64, buf: u32, done: u32) -> bool {
    let mut at = 0;
    while at < BLK_BYTES {
        if at == done {
            return true;
        }
        at += fill_chunk(staging, buf, at).2;
    }
    false
}

fn idle_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    chunks(u64::from(config.trap_frame) + FRAME_SATP, 8)
}

fn final_chunks(m5: &M5Config) -> Vec<(u64, u64)> {
    chunks(m5.shared, 4 * u64::from(m5.final_words))
}

impl M5Stage {
    /// The name inspect uses.
    pub fn name(&self) -> &'static str {
        match self {
            M5Stage::Arm => "arm",
            M5Stage::Pending { .. } => "irq_pending",
            M5Stage::TimerStatus { .. } => "timer_status",
            M5Stage::TimerAck { .. } => "timer_ack",
            M5Stage::BlkStatus { .. } => "blk_status",
            M5Stage::BlkAck { .. } => "blk_irq_ack",
            M5Stage::Fill { .. } => "fill",
            M5Stage::BlkCommand { .. } => "blk_read_command",
            M5Stage::BlkWalk { .. } => "blk_walk",
            M5Stage::Idle { .. } => "idle",
            M5Stage::Disarm { .. } => "disarm",
            M5Stage::Final { .. } => "final",
        }
    }

    /// The number of accesses of the stage.
    pub(crate) fn steps(&self, config: &KernelConfig, m5: &M5Config) -> usize {
        match self {
            M5Stage::Arm => SHARED_ZERO_STEPS + 4,
            M5Stage::Fill { .. } => 2,
            M5Stage::BlkCommand { .. } => 4,
            M5Stage::Idle { .. } => idle_chunks(config).len(),
            M5Stage::Final { .. } => final_chunks(m5).len(),
            M5Stage::Pending { .. }
            | M5Stage::TimerStatus { .. }
            | M5Stage::TimerAck { .. }
            | M5Stage::BlkStatus { .. }
            | M5Stage::BlkAck { .. }
            | M5Stage::BlkWalk { .. }
            | M5Stage::Disarm { .. } => 1,
        }
    }

    /// The working data length at `step`.
    pub(crate) fn data_len(&self, config: &KernelConfig, m5: &M5Config, step: u32) -> usize {
        match self {
            M5Stage::Fill { buf, done, .. } if step == 1 => {
                fill_chunk(config.staging.base, *buf, *done).2 as usize
            }
            M5Stage::Final { .. } => read_len(&final_chunks(m5), step),
            _ => 0,
        }
    }

    /// The access due at `step`, with `data` the working data.
    pub(crate) fn access(
        &self,
        config: &KernelConfig,
        m5: &M5Config,
        step: usize,
        data: &[u8],
    ) -> Option<Access> {
        let once = |access: Access| (step == 0).then_some(access);
        match self {
            M5Stage::Arm => {
                if step < SHARED_ZERO_STEPS {
                    return Some(Access::Write {
                        addr: m5.shared + step as u64 * MAX_ACCESS,
                        data: vec![0; MAX_ACCESS as usize],
                    });
                }
                let (addr, value) = [
                    (m5.irqc.base + IRQC_ENABLE, m5.sources()),
                    (config.blk.base + IRQ_ENABLE, 1),
                    (m5.timer.base + TIMER_COMPARE, m5.slice),
                    (m5.timer.base + TIMER_CONTROL, TIMER_ENABLE | TIMER_PERIODIC),
                ]
                .get(step - SHARED_ZERO_STEPS)
                .copied()?;
                Some(write32(addr, value))
            }
            M5Stage::Pending { .. } => once(read32(m5.irqc.base + IRQC_PENDING)),
            M5Stage::TimerStatus { .. } => once(read32(m5.timer.base + TIMER_STATUS)),
            M5Stage::TimerAck { .. } => once(write32(m5.timer.base + TIMER_STATUS, TIMER_EXPIRED)),
            M5Stage::BlkStatus { .. } => once(read32(config.blk.base + STATUS)),
            M5Stage::BlkAck { .. } => once(write32(config.blk.base + ACK, ACK_DONE)),
            M5Stage::Fill {
                buf, pages, done, ..
            } => {
                let (src, va, len) = fill_chunk(config.staging.base, *buf, *done);
                match step {
                    0 => Some(Access::Read { addr: src, len }),
                    1 => {
                        let page = ((va >> 12) - (buf >> 12)) as usize;
                        let addr = u64::from(pages[page]) * PAGE + u64::from(va & 0xFFF);
                        Some(Access::Write {
                            addr,
                            data: data.to_vec(),
                        })
                    }
                    _ => None,
                }
            }
            M5Stage::BlkCommand { lba, .. } => {
                let (offset, value) = [
                    (LBA, *lba),
                    (MEM_ADDR, config.staging.base as u32),
                    (BLOCK_COUNT, 1),
                    (COMMAND, OP_READ),
                ]
                .get(step)
                .copied()?;
                Some(write32(config.blk.base + offset, value))
            }
            M5Stage::BlkWalk {
                buffer,
                pages,
                table,
                level,
                ..
            } => once(read32(pte_address(
                *table,
                buffer.page_va(pages.len()),
                *level,
            ))),
            M5Stage::Idle { satp } => {
                let (addr, len) = *idle_chunks(config).get(step)?;
                let mut bytes = [0u8; 8];
                bytes[..4].copy_from_slice(&satp.to_le_bytes());
                bytes[4..].copy_from_slice(&ACTION_IDLE.to_le_bytes());
                let at = (addr - u64::from(config.trap_frame) - FRAME_SATP) as usize;
                Some(Access::Write {
                    addr,
                    data: bytes[at..at + len as usize].to_vec(),
                })
            }
            M5Stage::Disarm { .. } => once(write32(m5.timer.base + TIMER_CONTROL, 0)),
            M5Stage::Final { .. } => {
                let (addr, len) = *final_chunks(m5).get(step)?;
                Some(Access::Read {
                    addr,
                    len: len as u32,
                })
            }
        }
    }
}

// FRAME_ACTION follows FRAME_SATP: the Idle stage writes both in one range.
const _: () = assert!(FRAME_ACTION == FRAME_SATP + 4);

fn pid_note(kind: &'static str, pid: u32) -> Note {
    note(kind, vec![("pid", Value::U64(u64::from(pid)))])
}

impl Model {
    /// The M5 configuration, if this is an M5 kernel.
    pub fn m5_config(&self) -> Option<M5Config> {
        self.m5.as_ref().map(|m| m.config)
    }

    /// Whether the kernel is idle.
    pub fn is_idle(&self) -> bool {
        self.m5.as_ref().is_some_and(|m| m.idle)
    }

    /// The shared page as every process maps it, in an M5 kernel.
    pub fn shared(&self) -> Option<(u32, u32)> {
        self.m5_config().map(|m| m.shared_mapping())
    }

    fn m5_mut(&mut self) -> Result<&mut M5, Violation> {
        self.m5.as_mut().ok_or(Violation("an M5 stage without M5"))
    }

    fn m5_op(&self, stage: M5Stage) -> Result<ProcOp, Violation> {
        let m5 = self
            .m5_config()
            .ok_or(Violation("an M5 stage without M5"))?;
        Ok(ProcOp::new(Stage::M5 { m5, stage }))
    }

    /// After boot's last creation: Arm in an M5 kernel, else the first dispatch.
    pub(crate) fn boot_done(&mut self, notes: &mut Vec<Note>) -> Result<ProcOp, Violation> {
        if self.m5.is_some() {
            return self.m5_op(M5Stage::Arm);
        }
        self.dispatch_next(0, notes)
    }

    /// The first stage of a decided shutdown with `reason`: Disarm in an M5 kernel, else
    /// Shutdown.
    pub(crate) fn shutdown_op(&self, reason: u32) -> ProcOp {
        let stage = match self.m5_config() {
            Some(m5) => Stage::M5 {
                m5,
                stage: M5Stage::Disarm { reason },
            },
            None => Stage::Shutdown { reason },
        };
        ProcOp::new(stage)
    }

    /// With an empty run queue: Idle if a request is in flight (M5), else `None`.
    pub(crate) fn idle_op(&mut self) -> Result<Option<ProcOp>, Violation> {
        let Some(pid) = self
            .m5
            .as_ref()
            .and_then(|m| m.requests.front())
            .map(|r| r.pid)
        else {
            return Ok(None);
        };
        let root = self
            .procs
            .pcb(pid)
            .ok_or(Violation("a request's PID has no PCB"))?
            .root;
        self.m5_mut()?.idle = true;
        self.m5_op(M5Stage::Idle {
            satp: pte::satp(root),
        })
        .map(Some)
    }

    /// The M5 part of the decision after a trap: an interrupt, or any trap while idle.
    /// `None` leaves the trap to M3's rules.
    pub(crate) fn decide_m5(
        &mut self,
        frame: &[u8],
        notes: &mut Vec<Note>,
    ) -> Option<Result<ProcOp, Violation>> {
        let m5 = self.m5.as_ref()?;
        let scause = word(frame, FRAME_SCAUSE);
        let from_s = word(frame, FRAME_SSTATUS) & SSTATUS_SPP != 0;
        if m5.idle {
            if scause == INTERRUPT_CAUSE && from_s {
                let irq = Irq::new(Origin::Idle);
                return Some(self.m5_op(M5Stage::Pending { irq }));
            }
            let detail = format!("trap while idle: {}", cause_name(scause));
            return Some(Ok(self.fail(detail, notes)));
        }
        if scause == INTERRUPT_CAUSE && !from_s {
            let irq = Irq::new(Origin::Running(Context::from_frame(frame)));
            return Some(self.m5_op(M5Stage::Pending { irq }));
        }
        None
    }

    /// `blk_read(lba, buf)` from the running process at `sepc` (§11.3): the buffer walk.
    pub(crate) fn blk_read(
        &mut self,
        frame: &[u8],
        sepc: u32,
        lba: u32,
        buf: u32,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        let Some(buffer) = Buffer::new(sepc, buf, BLK_BYTES) else {
            return self.ret(SYS_BLK_READ, sepc, neg(EFAULT), notes);
        };
        let mut context = Context::from_frame(frame);
        context.pc = sepc.wrapping_add(4);
        let table = self.running_root()?;
        self.m5_op(M5Stage::BlkWalk {
            lba,
            buffer,
            pages: Vec::new(),
            table,
            level: 1,
            context,
        })
    }

    /// Moves past the finished M5 stage `stage`, whose reads returned `data`.
    pub(crate) fn advance_m5(
        &mut self,
        config: &KernelConfig,
        stage: M5Stage,
        data: &[u8],
        notes: &mut Vec<Note>,
    ) -> Result<Option<ProcOp>, Violation> {
        let m5 = self
            .m5_config()
            .ok_or(Violation("an M5 stage without M5"))?;
        let stage = match stage {
            M5Stage::Arm => return self.dispatch_next(0, notes).map(Some),
            M5Stage::Pending { mut irq } => {
                let sources = word(data, 0) & m5.sources();
                notes.push(note(
                    INTERRUPT_KIND,
                    vec![("sources", Value::U64(u64::from(sources)))],
                ));
                irq.todo = sources;
                return self.next_source(irq, notes);
            }
            M5Stage::TimerStatus { mut irq } => {
                irq.todo &= !(1 << m5.timer_source);
                if word(data, 0) & TIMER_EXPIRED == 0 {
                    return self.next_source(irq, notes);
                }
                M5Stage::TimerAck { irq }
            }
            M5Stage::TimerAck { mut irq } => {
                irq.preempt = true;
                return self.next_source(irq, notes);
            }
            M5Stage::BlkStatus { mut irq } => {
                irq.todo &= !(1 << m5.blk_source);
                let status = word(data, 0);
                let in_flight = self.m5.as_ref().is_some_and(|m| !m.requests.is_empty());
                if status & STATUS_DONE == 0 || !in_flight {
                    return self.next_source(irq, notes);
                }
                M5Stage::BlkAck {
                    irq,
                    error: (status >> STATUS_ERROR_SHIFT) as u8,
                }
            }
            M5Stage::BlkAck { irq, error } => {
                if error != 0 {
                    return self.complete_request(neg(EIO), irq, notes);
                }
                let head = self
                    .m5_mut()?
                    .requests
                    .front()
                    .ok_or(Violation("a block completion with no request"))?;
                M5Stage::Fill {
                    irq,
                    buf: head.buf,
                    pages: head.pages.clone(),
                    done: 0,
                }
            }
            M5Stage::Fill {
                irq,
                buf,
                pages,
                done,
            } => {
                let done = done + fill_chunk(config.staging.base, buf, done).2;
                if done == BLK_BYTES {
                    return self.complete_request(0, irq, notes);
                }
                M5Stage::Fill {
                    irq,
                    buf,
                    pages,
                    done,
                }
            }
            M5Stage::BlkCommand { then, .. } => {
                return match then {
                    Then::Dispatch { from } => self.dispatch_next(from, notes).map(Some),
                    Then::Irq(irq) => self.next_source(irq, notes),
                };
            }
            M5Stage::BlkWalk {
                lba,
                buffer,
                mut pages,
                level,
                context,
                ..
            } => {
                let va = buffer.page_va(pages.len());
                match walk_step_for(word(data, 0), level, va, true) {
                    WalkStep::Next(table) => M5Stage::BlkWalk {
                        lba,
                        buffer,
                        pages,
                        table,
                        level: 0,
                        context,
                    },
                    WalkStep::Page(ppn) => {
                        pages.push(ppn);
                        if pages.len() == buffer.pages() {
                            return self.submit(lba, buffer, pages, context, notes).map(Some);
                        }
                        M5Stage::BlkWalk {
                            lba,
                            buffer,
                            pages,
                            table: self.running_root()?,
                            level: 1,
                            context,
                        }
                    }
                    WalkStep::Fault => {
                        return self
                            .ret(SYS_BLK_READ, buffer.sepc, neg(EFAULT), notes)
                            .map(Some);
                    }
                }
            }
            M5Stage::Idle { .. } => return Ok(None),
            M5Stage::Disarm { reason } => {
                if m5.final_words == 0 {
                    return Ok(Some(ProcOp::new(Stage::Shutdown { reason })));
                }
                M5Stage::Final { reason }
            }
            M5Stage::Final { reason } => {
                for (i, w) in data.as_chunks::<4>().0.iter().enumerate() {
                    let value = u32::from_le_bytes(*w);
                    notes.push(note(
                        FINAL_KIND,
                        vec![
                            ("word", Value::U64(i as u64)),
                            ("value", Value::U64(u64::from(value))),
                        ],
                    ));
                }
                return Ok(Some(ProcOp::new(Stage::Shutdown { reason })));
            }
        };
        self.m5_op(stage).map(Some)
    }

    /// The next pending source of `irq`, lowest first, or the end of the trap.
    fn next_source(
        &mut self,
        irq: Irq,
        notes: &mut Vec<Note>,
    ) -> Result<Option<ProcOp>, Violation> {
        let m5 = self
            .m5_config()
            .ok_or(Violation("an M5 stage without M5"))?;
        if irq.todo == 0 {
            return self.finish(irq, notes);
        }
        let stage = if irq.todo.trailing_zeros() == u32::from(m5.timer_source) {
            M5Stage::TimerStatus { irq }
        } else {
            M5Stage::BlkStatus { irq }
        };
        self.m5_op(stage).map(Some)
    }

    /// The end of an interrupt trap: preempt, wake from idle, or release with nothing
    /// written, so the interrupted context resumes unchanged.
    fn finish(&mut self, irq: Irq, notes: &mut Vec<Note>) -> Result<Option<ProcOp>, Violation> {
        match irq.from {
            Origin::Running(context) if irq.preempt => {
                let pid = self.procs.yield_current(context)?;
                notes.push(pid_note(PREEMPT_KIND, pid));
                self.dispatch_next(pid, notes).map(Some)
            }
            Origin::Running(_) => Ok(None),
            Origin::Idle if self.procs.queue().is_empty() => Ok(None),
            Origin::Idle
                if irq.preempt
                    && self.m5_config().and_then(|m| m.defect)
                        == Some(Defect::IdleTickDropsWake) =>
            {
                Ok(None)
            }
            Origin::Idle => {
                self.m5_mut()?.idle = false;
                self.dispatch_next(0, notes).map(Some)
            }
        }
    }

    /// The head request ends with `value`: its caller wakes, and the next request, if
    /// any, is commanded before the trap goes on.
    fn complete_request(
        &mut self,
        value: u32,
        irq: Irq,
        notes: &mut Vec<Note>,
    ) -> Result<Option<ProcOp>, Violation> {
        let m5 = self.m5_mut()?;
        let done = m5
            .requests
            .pop_front()
            .ok_or(Violation("a block completion with no request"))?;
        let next = m5.requests.front().map(|r| r.lba);
        self.procs.wake(done.pid, value)?;
        notes.push(exit_note(done.pid, SYS_BLK_READ, value));
        notes.push(pid_note(WAKE_KIND, done.pid));
        match next {
            Some(lba) => self
                .m5_op(M5Stage::BlkCommand {
                    lba,
                    then: Then::Irq(irq),
                })
                .map(Some),
            None => self.next_source(irq, notes),
        }
    }

    /// The checked `blk_read`: the caller blocks, its request joins the FIFO, and the
    /// controller gets it at once if it was idle.
    fn submit(
        &mut self,
        lba: u32,
        buffer: Buffer,
        pages: Vec<u32>,
        context: Context,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        let pid = self.procs.block_current(context)?;
        notes.push(note(
            BLOCK_KIND,
            vec![
                ("pid", Value::U64(u64::from(pid))),
                ("lba", Value::U64(u64::from(lba))),
            ],
        ));
        let requests = &mut self.m5_mut()?.requests;
        requests.push_back(Request {
            pid,
            lba,
            buf: buffer.buf,
            pages,
        });
        if requests.len() == 1 {
            return self.m5_op(M5Stage::BlkCommand {
                lba,
                then: Then::Dispatch { from: pid },
            });
        }
        self.dispatch_next(pid, notes)
    }
}
