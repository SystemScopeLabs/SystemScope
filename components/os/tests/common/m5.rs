//! Test support for the M5 kernel (`docs/m5-design.md` §11). Test-only.
//!
//! - [`Devices`] is the interrupt controller, the timer, and the block controller behind
//!   the harness, written from their register maps (m2-design §9.2 and §6; m5-design
//!   §10), independently of `systemscope-platform`. The test decides when the timer
//!   expires and when a block transfer ends, and can add spurious `PENDING` bits.
//! - [`M5Harness`] drives an M5 kernel through `MockCtx`, one access at a time, answering
//!   RAM accesses from a [`PageMem`] and device accesses from [`Devices`]. With
//!   `roundtrip` set, it snapshots the kernel at every boundary, restores the bytes into a
//!   fresh kernel, requires the same bytes back, and continues with the restored kernel.

#![allow(dead_code)]

use systemscope_contracts::component::Component;
use systemscope_contracts::event::Phase;
use systemscope_contracts::protocol::mem_v1::{MemMsg, ReadOutcome, WriteOutcome};
use systemscope_contracts::trace::Value;
use systemscope_elf::BLOCK_SIZE;
use systemscope_os::kernel::{GATE_PORT, ISSUE, MEM_PORT};
use systemscope_os::{M5Config, ModeledKernel, ProcessPlan, Window};

use super::layout::*;
use super::procs::{HELD, MAX_OP_ACCESSES, PageMem, Seen, Stop, fault_text};
use super::{MockCtx, Traced, restore_into, snapshot_of};

pub const TIMER_BASE: u64 = 0x1000_4000;
pub const TIMER_SIZE: u64 = 0x10;
pub const IRQC_SIZE: u64 = 0x8;
pub const SHARED: u64 = 0x8030_0000;
pub const SHARED_VA: u32 = 0xF000;
pub const BLK_SOURCE: u8 = 0;
pub const TIMER_SOURCE: u8 = 1;
pub const SLICE: u32 = 1000;
pub const FINAL_WORDS: u32 = 4;
pub const INTERRUPT: u32 = 0x8000_0009;
pub const SPP: u32 = 1 << 8;
pub const DISK_BLOCKS: usize = 16;

/// Register offsets (m2-design §6, §9.2; m5-design §10).
pub mod reg {
    pub const PENDING: u64 = 0x0;
    pub const ENABLE: u64 = 0x4;
    pub const COMPARE: u64 = 0x0;
    pub const CONTROL: u64 = 0x4;
    pub const T_STATUS: u64 = 0x8;
    pub const COMMAND: u64 = 0x00;
    pub const STATUS: u64 = 0x04;
    pub const LBA: u64 = 0x08;
    pub const MEM_ADDR: u64 = 0x0C;
    pub const BLOCK_COUNT: u64 = 0x10;
    pub const IRQ_ENABLE: u64 = 0x14;
    pub const ACK: u64 = 0x18;
}

/// The M5 configuration the tests use: the `m3-reference` layout plus the timer at
/// `0x1000_4000`, the block controller on source 0 and the timer on source 1.
pub fn m5_config() -> M5Config {
    M5Config {
        irqc: Window {
            base: IRQC_BASE,
            size: IRQC_SIZE,
        },
        timer: Window {
            base: TIMER_BASE,
            size: TIMER_SIZE,
        },
        blk_source: BLK_SOURCE,
        timer_source: TIMER_SOURCE,
        slice: SLICE,
        shared: SHARED,
        shared_va: SHARED_VA,
        final_words: FINAL_WORDS,
    }
}

/// The block controller's command state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blk {
    Idle,
    Busy,
    Done { error: u8 },
}

/// The devices behind the harness (see the module docs).
#[derive(Clone, Debug)]
pub struct Devices {
    pub sources: (u8, u8),
    pub irq_enable: u32,
    pub compare: u32,
    pub control: u32,
    pub expired: bool,
    pub blk_irq_enable: u32,
    pub lba: u32,
    pub mem_addr: u32,
    pub count: u32,
    pub blk: Blk,
    pub disk: Vec<u8>,
    /// Bits `PENDING` reads besides the lines: spurious candidates.
    pub extra_pending: u32,
    /// Every command: `(lba, count, mem_addr)`.
    pub commands: Vec<(u32, u32, u32)>,
}

impl Devices {
    /// Devices with a disk whose block `b` is filled with byte `b + 1`, bit 7 set.
    pub fn new(sources: (u8, u8)) -> Devices {
        let disk = (0..DISK_BLOCKS * BLOCK_SIZE)
            .map(|i| ((i / BLOCK_SIZE) as u8 + 1) | 0x80)
            .collect();
        Devices {
            sources,
            irq_enable: 0,
            compare: 0,
            control: 0,
            expired: false,
            blk_irq_enable: 0,
            lba: 0,
            mem_addr: 0,
            count: 0,
            blk: Blk::Idle,
            disk,
            extra_pending: 0,
            commands: Vec::new(),
        }
    }

    /// The source lines: the block controller's `DONE && IRQ_ENABLE`, the timer's
    /// `EXPIRED`.
    pub fn lines(&self) -> u32 {
        let blk = matches!(self.blk, Blk::Done { .. }) && self.blk_irq_enable & 1 != 0;
        u32::from(blk) << self.sources.0 | u32::from(self.expired) << self.sources.1
    }

    /// Ends the transfer in flight with `error`; without one, its block lands in `mem`.
    pub fn finish(&mut self, error: u8, mem: &mut PageMem) {
        assert_eq!(self.blk, Blk::Busy, "no transfer in flight");
        if error == 0 {
            let at = self.lba as usize * BLOCK_SIZE;
            mem.write(u64::from(self.mem_addr), &self.disk[at..at + BLOCK_SIZE]);
        }
        self.blk = Blk::Done { error };
    }

    /// The block `lba` as the disk holds it.
    pub fn block(&self, lba: u32) -> Vec<u8> {
        let at = lba as usize * BLOCK_SIZE;
        self.disk[at..at + BLOCK_SIZE].to_vec()
    }

    fn in_window(addr: u64, base: u64, size: u64) -> Option<u64> {
        (addr >= base && addr < base + size).then(|| addr - base)
    }

    /// A read the devices answer, or `None` for RAM.
    fn read(&mut self, addr: u64, len: u32) -> Option<Vec<u8>> {
        let value = if let Some(off) = Self::in_window(addr, IRQC_BASE, IRQC_SIZE) {
            assert_eq!(
                (off, len),
                (reg::PENDING, 4),
                "the kernel reads only PENDING"
            );
            self.lines() | self.extra_pending
        } else if let Some(off) = Self::in_window(addr, TIMER_BASE, TIMER_SIZE) {
            assert_eq!(
                (off, len),
                (reg::T_STATUS, 4),
                "the kernel reads only STATUS"
            );
            u32::from(self.expired)
        } else {
            let off = Self::in_window(addr, BLK_BASE, BLK_SIZE)?;
            assert_eq!((off, len), (reg::STATUS, 4), "the kernel reads only STATUS");
            match self.blk {
                Blk::Idle => 0,
                Blk::Busy => 1,
                Blk::Done { error } => 2 | (u32::from(error) << 8),
            }
        };
        Some(value.to_le_bytes().to_vec())
    }

    /// A write the devices take, or `false` for RAM.
    fn write(&mut self, addr: u64, data: &[u8]) -> bool {
        let word = || {
            assert_eq!(data.len(), 4, "the kernel writes whole registers");
            u32::from_le_bytes(data.try_into().unwrap())
        };
        if let Some(off) = Self::in_window(addr, IRQC_BASE, IRQC_SIZE) {
            assert_eq!(off, reg::ENABLE, "the kernel writes only ENABLE");
            self.irq_enable = word();
        } else if let Some(off) = Self::in_window(addr, TIMER_BASE, TIMER_SIZE) {
            let v = word();
            match off {
                reg::COMPARE => {
                    assert!(v >= 1, "COMPARE is at least 1");
                    self.compare = v;
                }
                reg::CONTROL => self.control = v,
                reg::T_STATUS => {
                    assert_eq!(v, 1, "STATUS is write-1-to-clear");
                    self.expired = false;
                }
                other => panic!("a timer write to {other:#x}"),
            }
        } else if let Some(off) = Self::in_window(addr, BLK_BASE, BLK_SIZE) {
            let v = word();
            match off {
                reg::LBA => self.lba = v,
                reg::MEM_ADDR => self.mem_addr = v,
                reg::BLOCK_COUNT => self.count = v,
                reg::IRQ_ENABLE => self.blk_irq_enable = v,
                reg::COMMAND => {
                    assert_eq!(v, 1, "the kernel only reads");
                    assert_eq!(self.blk, Blk::Idle, "a command while one is in flight");
                    self.commands.push((self.lba, self.count, self.mem_addr));
                    self.blk = Blk::Busy;
                }
                reg::ACK => {
                    assert_eq!(v, 1);
                    assert!(matches!(self.blk, Blk::Done { .. }), "ACK before DONE");
                    self.blk = Blk::Idle;
                }
                other => panic!("a block controller write to {other:#x}"),
            }
        } else {
            return false;
        }
        true
    }
}

/// An M5 kernel, a memory, its devices, and a `MockCtx`, stepped one handler at a time.
pub struct M5Harness {
    pub k: ModeledKernel,
    pub make: Box<dyn Fn() -> ModeledKernel>,
    pub ctx: MockCtx,
    pub mem: PageMem,
    pub dev: Devices,
    pub next_txn: u64,
    pub seen: Vec<Seen>,
    pub pending: Option<MemMsg>,
    /// Snapshot, restore into a fresh kernel, and continue with it at every boundary.
    pub roundtrip: bool,
    /// The number of round trips made.
    pub roundtrips: usize,
    /// The stage name inspect showed at each issued access.
    pub ops: Vec<String>,
}

impl M5Harness {
    /// A harness for `files` with `plan` and `m5`, the files staged.
    pub fn with(files: &[Vec<u8>], plan: ProcessPlan, m5: M5Config) -> M5Harness {
        let mut mem = PageMem::default();
        mem.stage(files, &plan);
        let config = super::procs::config();
        let make =
            Box::new(move || ModeledKernel::with_processes_m5(config, plan.clone(), m5).unwrap());
        M5Harness {
            k: make(),
            make,
            ctx: MockCtx::new(),
            mem,
            dev: Devices::new((m5.blk_source, m5.timer_source)),
            next_txn: 0,
            seen: Vec::new(),
            pending: None,
            roundtrip: false,
            roundtrips: 0,
            ops: Vec::new(),
        }
    }

    /// A harness for `files` with the M3 layout and [`m5_config`].
    pub fn new(files: &[Vec<u8>]) -> M5Harness {
        M5Harness::with(files, super::procs::plan_of(files), m5_config())
    }

    fn roundtrip_now(&mut self) {
        if !self.roundtrip {
            return;
        }
        let bytes = snapshot_of(&self.k);
        let mut fresh = (self.make)();
        restore_into(&mut fresh, &bytes).unwrap_or_else(|e| panic!("restore: {e:?}"));
        assert_eq!(snapshot_of(&fresh), bytes, "restore gives the same bytes");
        self.k = fresh;
        self.roundtrips += 1;
    }

    /// Delivers `ENTER` of `value`.
    pub fn enter(&mut self, value: u32) -> Result<(), &'static str> {
        let msg = MemMsg::WriteReq {
            txn: HELD,
            addr: 0,
            data: value.to_le_bytes().to_vec(),
        };
        self.ctx
            .deliver(&mut self.k, GATE_PORT, msg, Phase::Request)
            .map_err(fault_text)?;
        assert!(self.ctx.take_sent().is_empty());
        assert_eq!(self.ctx.take_woke().len(), 1);
        Ok(())
    }

    fn issue(&mut self) -> Result<(), &'static str> {
        self.ctx
            .wake(&mut self.k, ISSUE, Phase::Request)
            .map_err(fault_text)?;
        match self.k.inspect().get("op") {
            Some(Value::Str(op)) => self.ops.push(op.clone()),
            other => panic!("{other:?}"),
        }
        let sent = self.ctx.take_sent();
        assert_eq!(sent.len(), 1, "one request per wake");
        assert_eq!(sent[0].port, MEM_PORT);
        let txn = match &sent[0].msg {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => txn.0,
            other => panic!("{other:?}"),
        };
        assert_eq!(txn, self.next_txn, "fresh, consecutive kernel txns");
        self.next_txn += 1;
        self.pending = Some(sent[0].msg.clone());
        Ok(())
    }

    fn respond(&mut self) -> Result<bool, &'static str> {
        let resp = match self.pending.take().expect("a request is pending") {
            MemMsg::ReadReq { txn, addr, len } => {
                let data = self
                    .dev
                    .read(addr, len)
                    .unwrap_or_else(|| self.mem.read(addr, len as usize));
                self.seen.push((false, addr, data.clone()));
                MemMsg::ReadResp {
                    txn,
                    outcome: ReadOutcome::Data { data },
                }
            }
            MemMsg::WriteReq { txn, addr, data } => {
                self.seen.push((true, addr, data.clone()));
                if !self.dev.write(addr, &data) {
                    self.mem.write(addr, &data);
                }
                MemMsg::WriteResp {
                    txn,
                    outcome: WriteOutcome::Done,
                }
            }
            other => panic!("{other:?}"),
        };
        self.ctx
            .deliver(&mut self.k, MEM_PORT, resp, Phase::Complete)
            .map_err(fault_text)?;
        let sent = self.ctx.take_sent();
        let woke = self.ctx.take_woke();
        if sent.is_empty() {
            assert_eq!(woke.len(), 1);
            Ok(false)
        } else {
            assert_eq!(sent[0].port, GATE_PORT);
            assert_eq!(
                sent[0].msg,
                MemMsg::WriteResp {
                    txn: HELD,
                    outcome: WriteOutcome::Done
                }
            );
            Ok(true)
        }
    }

    /// Runs an operation from `ENTER` of `value` to its release.
    pub fn op(&mut self, value: u32) -> Stop {
        if let Err(e) = self.enter(value) {
            return Stop::Fault(e);
        }
        self.roundtrip_now();
        for _ in 0..MAX_OP_ACCESSES {
            if let Err(e) = self.issue() {
                return Stop::Fault(e);
            }
            self.roundtrip_now();
            match self.respond() {
                Err(e) => return Stop::Fault(e),
                Ok(true) => {
                    self.roundtrip_now();
                    return Stop::Released;
                }
                Ok(false) => self.roundtrip_now(),
            }
        }
        panic!("the operation did not end")
    }

    /// Boots: the first `ENTER`, to its release.
    pub fn boot(&mut self) {
        assert_eq!(self.op(TRAP_FRAME), Stop::Released);
    }

    /// Writes a trap frame: `scause`, `sepc`, `stval`, `sstatus`, and `x_i = 0x100 · i +
    /// tag`.
    pub fn write_frame(&mut self, scause: u32, sepc: u32, stval: u32, sstatus: u32, tag: u32) {
        let f = u64::from(TRAP_FRAME);
        for i in 1..=31u32 {
            self.mem.set_word(f + 4 * u64::from(i - 1), 0x100 * i + tag);
        }
        self.mem.set_word(f + 0x7C, sepc);
        self.mem.set_word(f + 0x80, sstatus);
        self.mem.set_word(f + 0x84, scause);
        self.mem.set_word(f + 0x88, stval);
    }

    /// Runs a trap with the frame [`M5Harness::write_frame`] writes.
    pub fn trap(&mut self, scause: u32, sepc: u32, sstatus: u32, tag: u32) -> Stop {
        self.write_frame(scause, sepc, 0, sstatus, tag);
        self.op(TRAP_FRAME)
    }

    /// Runs an interrupt trap from U at `sepc`.
    pub fn interrupt(&mut self, sepc: u32, tag: u32) -> Stop {
        self.trap(INTERRUPT, sepc, 0, tag)
    }

    /// Runs an interrupt trap from the idle spin (`SPP = S`).
    pub fn interrupt_idle(&mut self) -> Stop {
        self.trap(INTERRUPT, 0x8000_0100, SPP, 0x55)
    }

    /// Runs syscall `nr` with `args` from `sepc`.
    pub fn syscall(&mut self, nr: u32, args: [u32; 3], sepc: u32, tag: u32) -> Stop {
        self.write_frame(8, sepc, 0, 0, tag);
        let f = u64::from(TRAP_FRAME);
        for (i, a) in args.into_iter().enumerate() {
            self.mem.set_word(f + 0x24 + 4 * i as u64, a);
        }
        self.mem.set_word(f + 0x40, nr);
        self.op(TRAP_FRAME)
    }

    /// Runs `blk_read(lba, buf)` from `sepc`.
    pub fn blk_read(&mut self, lba: u32, buf: u32, sepc: u32) -> Stop {
        self.syscall(244, [lba, buf, 0], sepc, 0x30)
    }

    /// The trap frame's word at `offset`.
    pub fn frame(&self, offset: u64) -> u32 {
        self.mem.word(u64::from(TRAP_FRAME) + offset)
    }

    /// The whole trap frame.
    pub fn frame_bytes(&self) -> Vec<u8> {
        self.mem.read(u64::from(TRAP_FRAME), 0x98)
    }

    /// The traces of `kind`, in order.
    pub fn traced(&self, kind: &str) -> Vec<&Traced> {
        self.ctx.traced.iter().filter(|t| t.0 == kind).collect()
    }

    /// The running PID.
    pub fn running(&self) -> Option<u32> {
        self.k.processes().unwrap().current()
    }

    /// The run queue.
    pub fn queue(&self) -> Vec<u32> {
        self.k
            .processes()
            .unwrap()
            .queue()
            .iter()
            .copied()
            .collect()
    }

    /// The root table PPN of `pid`.
    pub fn root(&self, pid: u32) -> u32 {
        self.k.processes().unwrap().pcb(pid).unwrap().root
    }

    /// The accesses since `from` that were writes, as `(addr, bytes)`.
    pub fn writes_since(&self, from: usize) -> Vec<(u64, Vec<u8>)> {
        self.seen[from..]
            .iter()
            .filter(|s| s.0)
            .map(|s| (s.1, s.2.clone()))
            .collect()
    }
}
