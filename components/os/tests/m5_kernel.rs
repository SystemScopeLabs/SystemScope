//! The M5 kernel driven through `MockCtx` (`docs/m5-design.md` §11): boot's Arm and the
//! shared mapping, timer preemption, spurious interrupts, the blocking `blk_read` with its
//! Fill and wake, two sources in one trap, idle, `-EIO`, the request FIFO, every
//! `-EFAULT`, the shutdown's Disarm and Final, `M5Config` validation, and restore at
//! every access boundary of each scenario.
//!
//! The devices behind the kernel are `common::m5::Devices`, written from the register
//! maps; the test decides when the timer expires and when a transfer ends.

mod common;

use common::layout::*;
use common::m5::*;
use common::procs::{self, Harness, Kind, Stop, tu, two_segment, walk};
use common::{restore_into, snapshot_of};
use systemscope_os::interrupts::{fill_chunk, is_fill_start};
use systemscope_os::kernel::SHUTDOWN_KIND;
use systemscope_os::process::ProcState;
use systemscope_os::procop::{SWITCH_KIND, SYSCALL_EXIT_KIND};
use systemscope_os::{KernelConfig, M5Config, ModeledKernel, PlanError, UserLayout, Window};

const TEXT: u32 = 0x0001_0000;
const DATA: u32 = 0x0001_1000;
/// A buffer that straddles the first two data pages.
const BUF: u32 = 0x0001_1F00;
const BLK_READ: u32 = 244;
const EXIT: u32 = 93;
const EIO: u32 = 5u32.wrapping_neg();
const EFAULT: u32 = 14u32.wrapping_neg();
const ENOSYS: u32 = 38u32.wrapping_neg();
const A0: u64 = 0x24;
const SEPC: u64 = 0x7C;
const SATP: u64 = 0x8C;
const ACTION: u64 = 0x90;

/// A program whose data spans three writable pages: 0x11000–0x13FFF.
fn prog() -> Vec<u8> {
    two_segment(&[0x13, 0x13, 0x13], b"data", 8192)
}

fn booted(n: usize) -> M5Harness {
    booted_with(n, false)
}

fn booted_with(n: usize, roundtrip: bool) -> M5Harness {
    let files = vec![prog(); n];
    let mut h = M5Harness::new(&files);
    h.roundtrip = roundtrip;
    h.boot();
    h
}

fn state(h: &M5Harness, pid: u32) -> ProcState {
    h.k.processes().unwrap().pcb(pid).unwrap().state
}

fn user_bytes(h: &M5Harness, pid: u32, va: u32, len: u32) -> Vec<u8> {
    (va..va + len)
        .map(|v| {
            h.mem
                .read(walk(&h.mem, h.root(pid), v, Kind::Load).unwrap(), 1)[0]
        })
        .collect()
}

fn requests(h: &M5Harness) -> Vec<(u32, u32)> {
    h.k.m5()
        .unwrap()
        .requests
        .iter()
        .map(|r| (r.pid, r.lba))
        .collect()
}

fn idle(h: &M5Harness) -> bool {
    h.k.m5().unwrap().idle
}

fn sources(h: &M5Harness) -> Vec<u64> {
    h.traced("os.interrupt")
        .iter()
        .map(|t| tu(t, "sources"))
        .collect()
}

fn pids(h: &M5Harness, kind: &str) -> Vec<u64> {
    h.traced(kind).iter().map(|t| tu(t, "pid")).collect()
}

fn shutdown_reason(h: &M5Harness) -> u64 {
    let t = h.traced(SHUTDOWN_KIND);
    assert_eq!(t.len(), 1);
    tu(t[0], "reason")
}

/// The index in `seen` of the first access to `addr` at or after `from`.
fn first_at(h: &M5Harness, from: usize, addr: u64) -> usize {
    from + h.seen[from..].iter().position(|s| s.1 == addr).unwrap()
}

// Boot and the shared page.

#[test]
fn boot_arms_once_before_the_first_dispatch() {
    let h = booted(2);
    let writes = h.writes_since(0);
    let zeroes: Vec<_> = writes
        .iter()
        .filter(|w| w.0 >> 12 == SHARED >> 12)
        .collect();
    assert_eq!(zeroes.len(), 256);
    assert!(zeroes.iter().all(|w| w.1 == [0; 16]));
    let addrs: Vec<u64> = zeroes.iter().map(|w| w.0).collect();
    assert_eq!(addrs, (0..256).map(|i| SHARED + 16 * i).collect::<Vec<_>>());
    let arm = [
        (IRQC_BASE + reg::ENABLE, 0b11),
        (BLK_BASE + reg::IRQ_ENABLE, 1),
        (TIMER_BASE + reg::COMPARE, SLICE),
        (TIMER_BASE + reg::CONTROL, 3),
    ];
    let at = writes
        .iter()
        .position(|w| w.0 == IRQC_BASE + reg::ENABLE)
        .unwrap();
    for (i, (addr, value)) in arm.into_iter().enumerate() {
        assert_eq!(writes[at + i], (addr, value.to_le_bytes().to_vec()));
    }
    let zeroed_last = writes
        .iter()
        .rposition(|w| w.0 >> 12 == SHARED >> 12)
        .unwrap();
    assert_eq!(zeroed_last + 1, at, "Arm zeroes the page first");
    let satp = writes
        .iter()
        .position(|w| w.0 == u64::from(TRAP_FRAME) + SATP)
        .unwrap();
    assert!(satp > at + 3, "Arm precedes the first dispatch");
    assert_eq!(
        writes
            .iter()
            .filter(|w| w.0 == TIMER_BASE + reg::CONTROL)
            .count(),
        1
    );
    assert_eq!(
        (
            h.dev.irq_enable,
            h.dev.blk_irq_enable,
            h.dev.compare,
            h.dev.control
        ),
        (0b11, 1, SLICE, 3)
    );
    assert_eq!(h.running(), Some(1));
    assert_eq!(h.frame(SATP), (1 << 31) | h.root(1));
}

#[test]
fn every_process_maps_the_shared_page_read_write() {
    let mut h = booted(2);
    for pid in [1, 2] {
        let pa = walk(&h.mem, h.root(pid), SHARED_VA + 0x10, Kind::Store);
        assert_eq!(pa, Some(SHARED + 0x10));
        assert_eq!(walk(&h.mem, h.root(pid), SHARED_VA, Kind::Fetch), None);
    }
    // A blk_read into the shared page is a store the walk allows.
    assert_eq!(h.blk_read(1, SHARED_VA, TEXT), Stop::Released);
    assert_eq!(requests(&h), [(1, 1)]);
}

#[test]
fn an_m3_kernel_maps_no_shared_page() {
    let files = vec![prog()];
    let mut h = Harness::new(procs::config(), &files, procs::plan_of(&files));
    h.boot();
    let root = h.k.processes().unwrap().pcb(1).unwrap().root;
    assert_eq!(walk(&h.mem, root, SHARED_VA, Kind::Load), None);
    assert!(h.k.m5().is_none());
}

// Interrupts from a running process.

#[test]
fn the_timer_preempts_and_the_preempted_context_resumes_exactly() {
    let mut h = booted(2);
    h.dev.expired = true;
    assert_eq!(h.interrupt(TEXT + 8, 0x11), Stop::Released);
    assert!(!h.dev.expired, "the kernel cleared EXPIRED");
    assert_eq!(sources(&h), [1 << TIMER_SOURCE]);
    assert_eq!(pids(&h, "os.process.preempt"), [1]);
    assert_eq!((h.running(), h.queue()), (Some(2), vec![1]));
    assert_eq!(h.frame(SEPC), TEXT);

    h.dev.expired = true;
    assert_eq!(h.interrupt(TEXT + 4, 0x22), Stop::Released);
    assert_eq!((h.running(), h.queue()), (Some(1), vec![2]));
    assert_eq!(h.frame(SEPC), TEXT + 8, "sepc is not advanced");
    for i in 1..=31u64 {
        assert_eq!(h.frame(4 * (i - 1)), 0x100 * i as u32 + 0x11);
    }
    assert_eq!(h.frame(SATP), (1 << 31) | h.root(1));
    assert_eq!(pids(&h, "os.process.preempt"), [1, 2]);
}

#[test]
fn an_interrupt_with_nothing_to_do_resumes_with_no_write() {
    let mut h = booted(2);
    // A spurious timer candidate: PENDING says so, STATUS does not.
    h.dev.extra_pending = 1 << TIMER_SOURCE;
    let from = h.seen.len();
    h.write_frame(INTERRUPT, TEXT + 4, 0, 0, 0x33);
    let before = h.frame_bytes();
    assert_eq!(h.op(TRAP_FRAME), Stop::Released);
    let after = h.frame_bytes();
    assert_eq!(h.writes_since(from), vec![], "no write at all");
    assert_eq!(before, after);
    assert_eq!(sources(&h), [1 << TIMER_SOURCE]);
    assert!(h.traced("os.process.preempt").is_empty());
    assert_eq!(h.running(), Some(1));

    // Bits outside the two sources are masked away: only PENDING is read.
    h.dev.extra_pending = 0xFFFF_FFFC;
    let from = h.seen.len();
    assert_eq!(h.interrupt(TEXT + 4, 0x34), Stop::Released);
    assert_eq!(sources(&h), [2, 0]);
    let devices: Vec<_> = h.seen[from..]
        .iter()
        .filter(|s| s.1 >> 12 != u64::from(TRAP_FRAME) >> 12)
        .map(|s| s.1)
        .collect();
    assert_eq!(devices, [IRQC_BASE + reg::PENDING]);

    // A spurious block candidate with no request in flight, and one while busy.
    h.dev.extra_pending = 1 << BLK_SOURCE;
    assert_eq!(h.interrupt(TEXT + 4, 0x35), Stop::Released);
    assert_eq!(h.dev.blk, Blk::Idle);
    assert_eq!(h.blk_read(2, BUF, TEXT + 4), Stop::Released);
    assert_eq!(h.dev.blk, Blk::Busy);
    let from = h.seen.len();
    assert_eq!(h.interrupt(TEXT, 0x36), Stop::Released);
    assert_eq!(h.writes_since(from), vec![]);
    assert_eq!(requests(&h), [(1, 2)]);
}

// blk_read.

#[test]
fn blk_read_blocks_fills_the_buffer_and_wakes_the_caller() {
    let mut h = booted(2);
    assert_eq!(h.blk_read(3, BUF, TEXT + 4), Stop::Released);
    let block = h.traced("os.process.block");
    assert_eq!((tu(block[0], "pid"), tu(block[0], "lba")), (1, 3));
    assert_eq!(h.dev.commands, [(3, 1, STAGING as u32)]);
    assert_eq!(state(&h, 1), ProcState::Blocked);
    assert_eq!((h.running(), h.queue()), (Some(2), vec![]));
    assert_eq!(requests(&h), [(1, 3)]);
    assert!(h.traced(SYSCALL_EXIT_KIND).is_empty(), "no return yet");

    h.dev.finish(0, &mut h.mem);
    let from = h.seen.len();
    let frame = {
        h.write_frame(INTERRUPT, TEXT + 8, 0, 0, 0x44);
        h.frame_bytes()
    };
    assert_eq!(h.op(TRAP_FRAME), Stop::Released);
    assert_eq!(h.frame_bytes(), frame, "process 2 resumes untouched");
    assert_eq!(sources(&h), [1 << BLK_SOURCE]);
    assert_eq!(h.dev.blk, Blk::Idle, "acknowledged");
    assert_eq!(pids(&h, "os.process.wake"), [1]);
    let exit = h.traced(SYSCALL_EXIT_KIND);
    assert_eq!(
        (tu(exit[0], "pid"), tu(exit[0], "nr"), tu(exit[0], "ret")),
        (1, 244, 0)
    );
    assert_eq!((state(&h, 1), h.queue()), (ProcState::Ready, vec![1]));
    assert_eq!(user_bytes(&h, 1, BUF, 512), h.dev.block(3));
    // Every Fill write is at most 16 bytes and crosses no page.
    let fills: Vec<_> = h
        .writes_since(from)
        .into_iter()
        .filter(|w| w.0 >= POOL)
        .collect();
    assert!(fills.len() >= 32);
    assert!(
        fills
            .iter()
            .all(|w| w.1.len() <= 16 && (w.0 & 0xFFF) + w.1.len() as u64 <= 0x1000)
    );
    assert_eq!(fills.iter().map(|w| w.1.len()).sum::<usize>(), 512);

    // At its next dispatch it returns 0 past the ecall, the rest of its frame intact.
    h.dev.expired = true;
    assert_eq!(h.interrupt(TEXT + 8, 0x45), Stop::Released);
    assert_eq!(h.running(), Some(1));
    assert_eq!((h.frame(A0), h.frame(SEPC)), (0, TEXT + 8));
    assert_eq!(h.frame(A0 + 4), BUF);
    for i in (1..=31u64).filter(|i| ![10, 11, 12, 17].contains(i)) {
        assert_eq!(h.frame(4 * (i - 1)), 0x100 * i as u32 + 0x30);
    }
}

#[test]
fn two_sources_in_one_trap_wake_then_preempt() {
    let mut h = booted(2);
    assert_eq!(h.blk_read(4, BUF, TEXT), Stop::Released);
    h.dev.finish(0, &mut h.mem);
    h.dev.expired = true;
    let from = h.seen.len();
    assert_eq!(h.interrupt(TEXT + 4, 0x50), Stop::Released);
    assert_eq!(sources(&h), [0b11]);
    let blk = first_at(&h, from, BLK_BASE + reg::STATUS);
    let timer = first_at(&h, from, TIMER_BASE + reg::T_STATUS);
    assert!(blk < timer, "the lower source first");
    let kinds: Vec<_> = h
        .ctx
        .traced
        .iter()
        .map(|t| t.0)
        .filter(|k| ["os.process.wake", "os.process.preempt"].contains(k))
        .collect();
    assert_eq!(
        kinds[kinds.len() - 2..],
        ["os.process.wake", "os.process.preempt"]
    );
    // The woken process went to the queue first, so it runs before the preempted one.
    assert_eq!((h.running(), h.queue()), (Some(1), vec![2]));
    assert_eq!((h.frame(A0), h.frame(SEPC)), (0, TEXT + 4));
    assert_eq!(user_bytes(&h, 1, BUF, 512), h.dev.block(4));
}

#[test]
fn a_controller_error_wakes_the_caller_with_eio_and_copies_nothing() {
    let mut h = booted(2);
    assert_eq!(h.blk_read(5, BUF, TEXT), Stop::Released);
    h.dev.finish(3, &mut h.mem);
    assert_eq!(h.interrupt(TEXT, 0x60), Stop::Released);
    assert_eq!(h.dev.blk, Blk::Idle);
    assert_eq!(tu(h.traced(SYSCALL_EXIT_KIND)[0], "ret"), u64::from(EIO));
    assert_eq!(user_bytes(&h, 1, BUF, 512), vec![0; 512]);
    h.dev.expired = true;
    assert_eq!(h.interrupt(TEXT, 0x61), Stop::Released);
    assert_eq!((h.running(), h.frame(A0)), (Some(1), EIO));
}

#[test]
fn requests_are_served_in_order_one_at_a_time() {
    let mut h = booted(2);
    assert_eq!(h.blk_read(2, BUF, TEXT), Stop::Released);
    assert_eq!(h.blk_read(6, DATA, TEXT + 4), Stop::Released);
    assert_eq!(requests(&h), [(1, 2), (2, 6)]);
    assert_eq!(h.dev.commands.len(), 1, "the second waits");
    assert!(idle(&h));
    assert_eq!(h.frame(SATP), (1 << 31) | h.root(1), "the head's satp");

    h.dev.finish(0, &mut h.mem);
    assert_eq!(h.interrupt_idle(), Stop::Released);
    assert_eq!(
        h.dev.commands,
        [(2, 1, STAGING as u32), (6, 1, STAGING as u32)]
    );
    assert_eq!(requests(&h), [(2, 6)]);
    assert_eq!((idle(&h), h.running()), (false, Some(1)));

    h.dev.finish(0, &mut h.mem);
    assert_eq!(h.interrupt(TEXT + 4, 0x70), Stop::Released);
    assert_eq!(requests(&h), []);
    assert_eq!(h.dev.commands.len(), 2);
    assert_eq!(pids(&h, "os.process.wake"), [1, 2]);
    assert_eq!(user_bytes(&h, 1, BUF, 512), h.dev.block(2));
    assert_eq!(user_bytes(&h, 2, DATA, 512), h.dev.block(6));
}

#[test]
fn a_bad_buffer_returns_efault_without_blocking() {
    for buf in [
        TEXT,        // read-only
        0x0003_0000, // unmapped
        0x0001_3F00, // runs into an unmapped page
        0x8000_0000, // not a user address
        0xFFFF_FF00, // wraps
    ] {
        let mut h = booted(2);
        assert_eq!(h.blk_read(1, buf, TEXT + 4), Stop::Released, "{buf:#x}");
        assert_eq!((h.frame(A0), h.frame(SEPC)), (EFAULT, TEXT + 8), "{buf:#x}");
        assert_eq!(h.running(), Some(1));
        assert!(h.traced("os.process.block").is_empty());
        assert!(h.dev.commands.is_empty());
        assert_eq!(requests(&h), []);
    }
}

#[test]
fn blk_read_without_m5_is_enosys() {
    let files = vec![prog()];
    let mut h = Harness::new(procs::config(), &files, procs::plan_of(&files));
    h.boot();
    assert_eq!(h.syscall(BLK_READ, [1, BUF, 0], TEXT, 0), Stop::Released);
    assert_eq!(h.frame(A0), ENOSYS);
}

// Idle.

#[test]
fn the_kernel_idles_until_a_wake_and_ignores_other_interrupts() {
    let mut h = booted(1);
    assert_eq!(h.blk_read(7, BUF, TEXT + 4), Stop::Released);
    assert!(idle(&h));
    assert_eq!(h.running(), None);
    assert_eq!((h.frame(SATP), h.frame(ACTION)), ((1 << 31) | h.root(1), 2));
    assert!(h.traced(SWITCH_KIND).len() == 1, "no dispatch to idle");

    // A spurious candidate while idle: nothing written, still idle.
    h.dev.extra_pending = 1 << BLK_SOURCE;
    let from = h.seen.len();
    assert_eq!(h.interrupt_idle(), Stop::Released);
    assert_eq!(h.writes_since(from), vec![]);
    assert!(idle(&h));
    h.dev.extra_pending = 0;

    // A timer expiry while idle: acknowledged, nothing to preempt, still idle.
    h.dev.expired = true;
    assert_eq!(h.interrupt_idle(), Stop::Released);
    assert!(!h.dev.expired);
    assert!(idle(&h));
    assert!(h.traced("os.process.preempt").is_empty());

    h.dev.finish(0, &mut h.mem);
    assert_eq!(h.interrupt_idle(), Stop::Released);
    assert!(!idle(&h));
    assert_eq!(h.running(), Some(1));
    assert_eq!(
        (h.frame(A0), h.frame(SEPC), h.frame(ACTION)),
        (0, TEXT + 8, 0)
    );
    assert_eq!(user_bytes(&h, 1, BUF, 512), h.dev.block(7));
}

#[test]
fn any_other_trap_while_idle_shuts_down() {
    for (scause, sstatus) in [(8, SPP), (INTERRUPT, 0), (13, SPP)] {
        let mut h = booted(1);
        assert_eq!(h.blk_read(7, BUF, TEXT), Stop::Released);
        assert_eq!(h.trap(scause, TEXT, sstatus, 0x80), Stop::Released);
        assert_eq!(shutdown_reason(&h), 1);
        assert_eq!(h.dev.control, 0, "disarmed");
        assert_eq!(h.traced("os.final").len(), FINAL_WORDS as usize);
    }
}

// Shutdown.

#[test]
fn every_shutdown_disarms_then_dumps_the_shared_words() {
    let mut h = booted(1);
    for i in 0..FINAL_WORDS + 2 {
        h.mem.set_word(SHARED + 4 * u64::from(i), 0xA000 + i);
    }
    let from = h.seen.len();
    assert_eq!(h.syscall(EXIT, [0, 0, 0], TEXT, 0), Stop::Released);
    assert_eq!(shutdown_reason(&h), 0);
    assert_eq!(h.dev.control, 0);
    let finals: Vec<_> = h
        .traced("os.final")
        .iter()
        .map(|t| (tu(t, "word"), tu(t, "value")))
        .collect();
    let expected: Vec<_> = (0..u64::from(FINAL_WORDS))
        .map(|i| (i, 0xA000 + i))
        .collect();
    assert_eq!(finals, expected);
    let disarm = first_at(&h, from, TIMER_BASE + reg::CONTROL);
    let dump = first_at(&h, from, SHARED);
    let reason = from
        + h.seen[from..]
            .iter()
            .position(|s| s.0 && s.1 == u64::from(TRAP_FRAME) + ACTION)
            .unwrap();
    assert!(disarm < dump && dump < reason);
}

/// Other source numbers, the timer below the block controller: the kernel enables, masks,
/// and orders by the configured sources.
#[test]
fn the_configured_sources_are_the_ones_used() {
    let files = vec![prog(); 2];
    let m5 = M5Config {
        blk_source: 6,
        timer_source: 3,
        ..m5_config()
    };
    let mut h = M5Harness::with(&files, procs::plan_of(&files), m5);
    h.boot();
    assert_eq!(h.dev.irq_enable, (1 << 6) | (1 << 3));
    assert_eq!(h.blk_read(4, BUF, TEXT), Stop::Released);
    h.dev.finish(0, &mut h.mem);
    h.dev.expired = true;
    h.dev.extra_pending = 1 << BLK_SOURCE | 1 << TIMER_SOURCE;
    let from = h.seen.len();
    assert_eq!(h.interrupt(TEXT, 0x90), Stop::Released);
    assert_eq!(sources(&h), [(1 << 6) | (1 << 3)]);
    let timer = first_at(&h, from, TIMER_BASE + reg::T_STATUS);
    let blk = first_at(&h, from, BLK_BASE + reg::STATUS);
    assert!(timer < blk, "the lower source first");
    assert_eq!(pids(&h, "os.process.wake"), [1]);
    assert_eq!(pids(&h, "os.process.preempt"), [2]);
}

#[test]
fn no_final_words_skip_the_dump() {
    let files = vec![prog()];
    let m5 = M5Config {
        final_words: 0,
        ..m5_config()
    };
    let mut h = M5Harness::with(&files, procs::plan_of(&files), m5);
    h.boot();
    let from = h.seen.len();
    assert_eq!(h.syscall(EXIT, [0, 0, 0], TEXT, 0), Stop::Released);
    assert!(h.traced("os.final").is_empty());
    assert_eq!(h.dev.control, 0);
    assert!(h.seen[from..].iter().all(|s| s.1 >> 12 != SHARED >> 12));
}

// Restore.

/// Every scenario above, restored into a fresh kernel at every access boundary.
#[test]
fn every_access_boundary_restores_and_continues() {
    let mut h = booted_with(2, true);
    h.dev.expired = true;
    assert_eq!(h.interrupt(TEXT, 1), Stop::Released);
    assert_eq!(h.blk_read(2, BUF, TEXT), Stop::Released);
    assert_eq!(h.blk_read(3, DATA, TEXT), Stop::Released);
    assert!(idle(&h));
    h.dev.expired = true;
    assert_eq!(h.interrupt_idle(), Stop::Released);
    h.dev.finish(0, &mut h.mem);
    h.dev.expired = true;
    assert_eq!(h.interrupt_idle(), Stop::Released);
    h.dev.finish(3, &mut h.mem);
    h.dev.expired = true;
    assert_eq!(h.interrupt(TEXT, 2), Stop::Released);
    assert_eq!(h.blk_read(1, 0x0003_0000, TEXT), Stop::Released);
    assert_eq!(h.syscall(EXIT, [0, 0, 0], TEXT, 0), Stop::Released);
    assert_eq!(h.syscall(EXIT, [0, 0, 0], TEXT, 0), Stop::Released);
    assert_eq!(shutdown_reason(&h), 0);
    assert!(h.roundtrips > 1000, "{}", h.roundtrips);
    assert_eq!(pids(&h, "os.process.wake"), [2, 1]);
    let mut ops = h.ops.clone();
    ops.sort();
    ops.dedup();
    for name in [
        "arm",
        "irq_pending",
        "timer_status",
        "timer_ack",
        "blk_status",
        "blk_irq_ack",
        "fill",
        "blk_read_command",
        "blk_walk",
        "idle",
        "disarm",
        "final",
    ] {
        assert!(ops.iter().any(|o| o == name), "{name}: {ops:?}");
    }
}

/// A dump of several reads, restored between them.
#[test]
fn a_long_dump_restores_between_its_reads() {
    let files = vec![prog()];
    let m5 = M5Config {
        final_words: 9,
        ..m5_config()
    };
    let mut h = M5Harness::with(&files, procs::plan_of(&files), m5);
    h.roundtrip = true;
    h.boot();
    for i in 0..9 {
        h.mem.set_word(SHARED + 4 * i, 0xB000 + i as u32);
    }
    assert_eq!(h.syscall(EXIT, [0, 0, 0], TEXT, 0), Stop::Released);
    let finals: Vec<_> = h
        .traced("os.final")
        .iter()
        .map(|t| tu(t, "value"))
        .collect();
    assert_eq!(finals, (0..9).map(|i| 0xB000 + i).collect::<Vec<u64>>());
}

#[test]
fn fill_chunks_stop_at_either_page_end() {
    const ST: u64 = STAGING;
    // The buffer's page ends first.
    assert_eq!(fill_chunk(ST, 0x11F08, 0xF0), (ST + 0xF0, 0x11FF8, 8));
    // Staging's page ends first.
    assert_eq!(
        fill_chunk(ST + 0xF00, 0x11000, 0xF8),
        (ST + 0xFF8, 0x110F8, 8)
    );
    // The block ends first.
    assert_eq!(fill_chunk(ST, 0x11000, 0x1F8), (ST + 0x1F8, 0x111F8, 8));
    assert_eq!(fill_chunk(ST, 0x11000, 0), (ST, 0x11000, 16));
    for done in [0, 0x10, 0xF0, 0xF8, 0x108] {
        assert!(is_fill_start(ST, 0x11F08, done), "{done:#x}");
    }
    for done in [3, 0x100, 0x1FF, 512, 600] {
        assert!(!is_fill_start(ST, 0x11F08, done), "{done:#x}");
    }
}

#[test]
fn m3_and_m5_snapshots_do_not_cross() {
    let files = vec![prog()];
    let mut m3 = Harness::new(procs::config(), &files, procs::plan_of(&files));
    m3.boot();
    let m5 = booted(1);
    let make_m3 =
        || ModeledKernel::with_processes(procs::config(), procs::plan_of(&files)).unwrap();
    let make_m5 = || {
        ModeledKernel::with_processes_m5(procs::config(), procs::plan_of(&files), m5_config())
            .unwrap()
    };
    assert!(restore_into(&mut make_m3(), &snapshot_of(&m5.k)).is_err());
    assert!(restore_into(&mut make_m5(), &snapshot_of(&m3.k)).is_err());
    let other = || {
        let m5 = M5Config {
            slice: SLICE + 1,
            ..m5_config()
        };
        ModeledKernel::with_processes_m5(procs::config(), procs::plan_of(&files), m5).unwrap()
    };
    assert!(restore_into(&mut other(), &snapshot_of(&m5.k)).is_err());
    assert!(restore_into(&mut make_m5(), &snapshot_of(&m5.k)).is_ok());
}

#[test]
fn a_corrupted_m5_section_is_rejected() {
    let mut h = booted(1);
    let bytes = snapshot_of(&h.k);
    let files = vec![prog()];
    let fresh = || {
        ModeledKernel::with_processes_m5(procs::config(), procs::plan_of(&files), m5_config())
            .unwrap()
    };
    // The section ends the kernel's bytes: the idle flag, then a zero request count.
    let n = bytes.len();
    assert_eq!(bytes[n - 5..], [0, 0, 0, 0, 0]);
    for flag in [1, 2] {
        let mut bad = bytes.clone();
        bad[n - 5] = flag;
        assert!(restore_into(&mut fresh(), &bad).is_err(), "idle = {flag}");
    }
    let mut bad = bytes.clone();
    bad[n - 4] = 1;
    assert!(
        restore_into(&mut fresh(), &bad).is_err(),
        "a request count with none"
    );

    // Idle with its request: the flag cleared, or the request's PID changed.
    assert_eq!(h.blk_read(1, BUF, TEXT), Stop::Released);
    let bytes = snapshot_of(&h.k);
    assert!(restore_into(&mut fresh(), &bytes).is_ok());
    // The flag, the count, then the request: PID, LBA, buffer, and its two pages.
    let flag = bytes.len() - 29;
    assert_eq!(bytes[flag], 1);
    let mut bad = bytes.clone();
    bad[flag] = 0;
    assert!(
        restore_into(&mut fresh(), &bad).is_err(),
        "a request with idle unset and nothing running"
    );
    let mut bad = bytes.clone();
    bad[flag + 5] = 2;
    assert!(
        restore_into(&mut fresh(), &bad).is_err(),
        "a request for a PID that is not blocked"
    );
}

// Validation.

#[test]
fn every_m5_config_rule_is_checked() {
    let files = vec![prog()];
    let check = |m5: M5Config| {
        ModeledKernel::with_processes_m5(procs::config(), procs::plan_of(&files), m5).err()
    };
    assert_eq!(check(m5_config()), None);
    let base = m5_config();
    let bad = [
        M5Config {
            irqc: Window {
                base: IRQC_BASE,
                size: 0,
            },
            ..base
        },
        M5Config {
            timer: Window {
                base: u64::MAX,
                size: 0x10,
            },
            ..base
        },
        M5Config {
            irqc: Window {
                base: KGATE_BASE,
                size: 8,
            },
            ..base
        },
        M5Config {
            irqc: Window {
                base: IRQC_BASE,
                size: 4,
            },
            ..base
        },
        M5Config {
            timer: Window {
                base: TIMER_BASE,
                size: 8,
            },
            ..base
        },
        M5Config {
            blk_source: 32,
            ..base
        },
        M5Config {
            timer_source: 32,
            ..base
        },
        M5Config {
            timer_source: BLK_SOURCE,
            ..base
        },
        M5Config { slice: 0, ..base },
        M5Config {
            shared: SHARED + 4,
            ..base
        },
        M5Config {
            shared: 0x9000_0000,
            ..base
        },
        M5Config {
            shared: u64::from(TRAP_FRAME),
            ..base
        },
        M5Config {
            shared: STAGING,
            ..base
        },
        M5Config {
            shared: POOL,
            ..base
        },
        M5Config {
            shared_va: 0,
            ..base
        },
        M5Config {
            shared_va: SHARED_VA + 4,
            ..base
        },
        M5Config {
            shared_va: TEXT,
            ..base
        },
        M5Config {
            final_words: 1025,
            ..base
        },
    ];
    for m5 in bad {
        assert!(
            matches!(check(m5), Some(PlanError::M5(_))),
            "{m5:?}: {:?}",
            check(m5)
        );
    }
    assert_eq!(
        check(M5Config {
            final_words: 1024,
            ..base
        }),
        None
    );
    // SHARED_VA must be below USER_BASE of the plan's layout.
    let layout = UserLayout::M3;
    assert!(SHARED_VA < layout.user_base);
}

/// The rules that depend on the kernel configuration and the layout, at their bounds.
#[test]
fn m5_config_rules_against_the_kernel_config_and_layout() {
    let ok = |m5: M5Config, config: &KernelConfig, layout: &UserLayout| match m5
        .validate(config, layout)
    {
        Ok(()) => true,
        Err(PlanError::M5(_)) => false,
        Err(e) => panic!("{e:?}"),
    };
    let base = m5_config();
    let m3 = UserLayout::M3;
    let c = procs::config();
    assert!(ok(base, &c, &m3));
    let timer = |size| M5Config {
        timer: Window {
            base: TIMER_BASE,
            size,
        },
        ..base
    };
    assert!(ok(timer(0xC), &c, &m3));
    assert!(!ok(timer(0xB), &c, &m3));

    let with = |f: &dyn Fn(&mut KernelConfig)| {
        let mut c = procs::config();
        f(&mut c);
        c
    };
    assert!(ok(base, &with(&|c| c.blk.size = 0x1C), &m3));
    assert!(!ok(base, &with(&|c| c.blk.size = 0x1B), &m3));
    assert!(ok(base, &with(&|c| c.staging.size = 512), &m3));
    assert!(!ok(base, &with(&|c| c.staging.size = 511), &m3));
    assert!(ok(base, &with(&|c| c.staging.base = STAGING + 0x10), &m3));
    assert!(!ok(base, &with(&|c| c.staging.base = STAGING + 0x8), &m3));

    // The shared page's end at Sv32's physical limit.
    const LIMIT: u64 = 1 << 34;
    let big = with(&|c| c.ram.size = LIMIT + 0x1000 - RAM_BASE);
    let shared = |shared| M5Config { shared, ..base };
    assert!(ok(shared(LIMIT - 0x1000), &big, &m3));
    assert!(!ok(shared(LIMIT), &big, &m3));

    // SHARED_VA in a megapage slot, with a layout that leaves room below USER_BASE.
    let high = UserLayout {
        user_base: 0x9000_0000,
        ..m3
    };
    let va = |shared_va| M5Config { shared_va, ..base };
    assert!(ok(va(0x1040_0000), &c, &high));
    assert!(!ok(va(0x1000_0000), &c, &high), "the MMIO slot");
    assert!(!ok(va(0x8000_0000), &c, &high), "the kernel slot");
}
