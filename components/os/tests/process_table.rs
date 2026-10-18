//! The process table's own invariant checks (`docs/m3-design.md` §6.4, §6.6), driven
//! directly: each case breaks exactly one rule.

use systemscope_elf::Perms;
use systemscope_os::frames::Frames;
use systemscope_os::process::{Context, Pcb, ProcState, Processes};
use systemscope_os::space::Region;

const POOL: u64 = 0x8040_0000;
const RX: Perms = Perms {
    read: true,
    write: false,
    execute: true,
};

/// An 8-frame pool with `n` frames reserved for PID 1.
fn pool_with(n: usize) -> (Frames, Vec<u32>) {
    let mut frames = Frames::new(POOL, 8 * 4096);
    let got = frames.reserve(1, n).unwrap();
    (frames, got)
}

/// PID 1, `Ready`, with its root table in the first frame and one region in the rest.
fn pcb(frames: &[u32]) -> Pcb {
    Pcb {
        pid: 1,
        state: ProcState::Ready,
        context: Some(Context::initial(0x0001_0000, 0x7FFF_F000)),
        root: frames[0],
        tables: vec![frames[0]],
        regions: vec![Region {
            va: 0x0001_0000,
            perms: RX,
            frames: frames[1..].to_vec(),
        }],
    }
}

#[test]
fn admission_needs_ready_and_a_context_each() {
    let (frames, f) = pool_with(2);
    let mut t = Processes::new(frames);
    let mut no_context = pcb(&f);
    no_context.context = None;
    assert!(t.admit(no_context).is_err());
    let mut running = pcb(&f);
    running.state = ProcState::Running;
    assert!(t.admit(running).is_err());
    assert_eq!(t.admit(pcb(&f)), Ok(()));
    assert_eq!(t.check(1, None), Ok(()));
}

#[test]
fn a_live_process_needs_regions_as_well_as_tables() {
    let (frames, f) = pool_with(1);
    let mut t = Processes::new(frames);
    let mut bare = pcb(&f);
    bare.regions.clear();
    t.admit(bare).unwrap();
    assert!(t.check(1, None).is_err());
}

#[test]
fn the_creation_in_flight_needs_a_pid_nonzero_free_and_planned_each() {
    let (frames, f) = pool_with(2);
    let mut t = Processes::new(frames);
    t.admit(pcb(&f)).unwrap();
    assert_eq!(t.check(2, Some((2, &[]))), Ok(()));
    for pid in [0, 1, 3] {
        assert!(t.check(2, Some((pid, &[]))).is_err(), "PID {pid}");
    }
}

#[test]
fn a_creation_frame_listed_twice_is_owned_twice() {
    let (frames, f) = pool_with(1);
    let t = Processes::new(frames);
    assert_eq!(t.check(1, Some((1, &f))), Ok(()));
    assert!(t.check(1, Some((1, &[f[0], f[0]]))).is_err());
}
