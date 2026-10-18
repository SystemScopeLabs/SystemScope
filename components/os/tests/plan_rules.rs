//! The plan and disk boot rules at their boundaries (`docs/m3-design.md` §6.4, §8.2,
//! §8.3): each case breaks exactly one rule, or sits exactly on its limit and passes.

mod common;

use common::layout::*;
use common::procs::config;
use common::procs::*;
use common::{restore_into, snapshot_of};
use systemscope_elf::{BLOCK_SIZE, Perms};
use systemscope_os::image::{MAX_PROCESSES, PHYS_LIMIT, valid_perms};
use systemscope_os::{
    DiskBoot, KernelConfig, ModeledKernel, PlanError, ProcessPlan, UserLayout, Window,
};

fn prog() -> Vec<u8> {
    two_segment(&[0x0000_0013, 0x0000_0073], b"DATA", 5000)
}

fn plan() -> ProcessPlan {
    plan_of(&[prog()])
}

fn layout_err(plan: &ProcessPlan, config: KernelConfig) -> Option<PlanError> {
    plan.validate(&config).err()
}

fn image_rule(plan: &ProcessPlan) -> Option<&'static str> {
    match plan.validate(&config()) {
        Ok(()) => None,
        Err(PlanError::Image { index: 0, rule }) => Some(rule),
        Err(e) => panic!("not an image rule: {e}"),
    }
}

const PERMS_RULE: &str = "a segment has no pages or invalid permissions";
const PAGE_RULE: &str = "a page is misplaced, out of order, or shared";

#[test]
fn a_leaf_is_readable_or_executable_and_never_writable_without_readable() {
    for bits in 0..8u8 {
        let perms = Perms {
            read: bits & 1 != 0,
            write: bits & 2 != 0,
            execute: bits & 4 != 0,
        };
        // R, RW, X, RX, RWX; never none, W, or WX.
        let valid = matches!(bits, 1 | 3 | 4 | 5 | 7);
        assert_eq!(valid_perms(perms), valid, "{perms:?}");
    }
}

#[test]
fn the_ram_base_must_be_megapage_aligned() {
    let mut c = config();
    c.ram.base += 0x1000;
    assert_eq!(
        layout_err(&plan(), c),
        Some(PlanError::Layout(
            "the RAM base is not a 4 MiB aligned Sv32 address"
        ))
    );
}

#[test]
fn either_end_of_the_pool_off_a_page_is_rejected() {
    let rule = Some(PlanError::Layout("the frame pool is not page-aligned"));
    let mut base = config();
    base.frame_pool.base += 0x800;
    base.frame_pool.size -= 0x1000;
    assert_eq!(layout_err(&plan(), base), rule);
    let mut size = config();
    size.frame_pool.size -= 0x800;
    assert_eq!(layout_err(&plan(), size), rule);
}

#[test]
fn the_pool_may_end_exactly_at_the_sv32_physical_limit() {
    // The RAM reaches past the limit, so only the reachability rule decides.
    let mut c = config();
    c.ram.size = PHYS_LIMIT + 0x1000 - RAM_BASE;
    c.frame_pool = Window {
        base: PHYS_LIMIT - 0x10_0000,
        size: 0x10_0000,
    };
    assert_eq!(layout_err(&plan(), c), None);
    c.frame_pool.size += 0x1000;
    assert_eq!(
        layout_err(&plan(), c),
        Some(PlanError::Layout("the frame pool is not reachable by Sv32"))
    );
}

#[test]
fn either_user_bound_off_a_page_is_rejected() {
    let rule = Some(PlanError::Layout(
        "USER_BASE or STACK_TOP is not page-aligned",
    ));
    for layout in [
        UserLayout {
            user_base: 0x0001_0004,
            ..UserLayout::M3
        },
        UserLayout {
            stack_top: 0x7FFF_F004,
            ..UserLayout::M3
        },
    ] {
        let p = ProcessPlan { layout, ..plan() };
        assert_eq!(layout_err(&p, config()), rule, "{layout:?}");
    }
}

#[test]
fn a_plan_holds_up_to_max_processes_images() {
    let one = plan();
    let full = ProcessPlan {
        images: vec![one.images[0].clone(); MAX_PROCESSES],
        ..one
    };
    assert_eq!(full.validate(&config()), Ok(()));
}

#[test]
fn a_misaligned_entry_inside_the_range_is_rejected() {
    let mut p = plan();
    p.images[0].image.entry += 2;
    assert_eq!(
        image_rule(&p),
        Some("the entry is misaligned or outside the image range")
    );
}

#[test]
fn a_segment_needs_pages_and_valid_permissions_each_on_its_own() {
    assert_eq!(image_rule(&plan()), None);
    let mut empty = plan();
    empty.images[0].image.segments[1].pages.clear();
    assert_eq!(image_rule(&empty), Some(PERMS_RULE));
    let mut write_only = plan();
    let seg = &mut write_only.images[0].image.segments[1];
    seg.perms = Perms {
        read: false,
        write: true,
        execute: false,
    };
    for p in &mut seg.pages {
        p.perms = seg.perms;
    }
    assert_eq!(image_rule(&write_only), Some(PERMS_RULE));
}

#[test]
fn each_page_rule_rejects_on_its_own() {
    let layout = UserLayout::M3;
    let range = layout.image_range();
    let mut misaligned = plan();
    misaligned.images[0].image.segments[1].pages[0].va += 4;
    let mut below = plan();
    below.images[0].image.segments[0].pages[0].va = range.start - 0x1000;
    let mut at_end = plan();
    let seg = at_end.images[0].image.segments.last_mut().unwrap();
    let mut page = *seg.pages.last().unwrap();
    page.va = range.end;
    page.copy = None;
    seg.pages.push(page);
    let mut other_perms = plan();
    let seg = &mut other_perms.images[0].image.segments[1];
    seg.pages[0].perms = Perms {
        read: true,
        write: false,
        execute: false,
    };
    assert_ne!(seg.pages[0].perms, seg.perms);
    for (why, p) in [
        ("misaligned", misaligned),
        ("below USER_BASE", below),
        ("at the stack base", at_end),
        ("other permissions", other_perms),
    ] {
        assert_eq!(image_rule(&p), Some(PAGE_RULE), "{why}");
    }
}

#[test]
fn an_empty_copy_is_rejected() {
    let mut p = plan();
    let copy = p.images[0].image.segments[0].pages[0]
        .copy
        .as_mut()
        .unwrap();
    copy.len = 0;
    assert_eq!(image_rule(&p), Some("a copy leaves its page or the file"));
}

#[test]
fn a_snapshot_of_another_plan_is_rejected() {
    let a = plan();
    let b = plan_of(&[two_segment(&[0x0000_0013], b"OTHER", 0)]);
    let snap = snapshot_of(&ModeledKernel::with_processes(config(), a.clone()).unwrap());
    let mut same = ModeledKernel::with_processes(config(), a).unwrap();
    assert!(restore_into(&mut same, &snap).is_ok());
    let mut other = ModeledKernel::with_processes(config(), b).unwrap();
    assert!(restore_into(&mut other, &snap).is_err());
}

fn disk() -> DiskBoot {
    DiskBoot {
        layout: UserLayout::M3,
        capacity_blocks: 64,
    }
}

#[test]
fn staging_may_hold_exactly_one_block() {
    let c = KernelConfig {
        staging: Window {
            base: STAGING,
            size: BLOCK_SIZE as u64,
        },
        ..config()
    };
    assert_eq!(disk().validate(&c), Ok(()));
}

#[test]
fn staging_may_end_exactly_at_4_gib() {
    // The kernel megapage is the last one below 4 GiB, with staging at its top and the
    // pool just above it.
    let c = KernelConfig {
        ram: Window {
            base: 0xFFC0_0000,
            size: 8 << 20,
        },
        trap_frame: 0xFFC1_0000,
        staging: Window {
            base: 0xFFF0_0000,
            size: 0x10_0000,
        },
        frame_pool: Window {
            base: 1 << 32,
            size: 4 << 20,
        },
        ..config()
    };
    assert_eq!(disk().validate(&c), Ok(()));
}

#[test]
fn the_block_controller_window_may_end_at_its_last_register() {
    let c = KernelConfig {
        blk: Window {
            base: BLK_BASE,
            size: 0x1C,
        },
        ..config()
    };
    assert_eq!(disk().validate(&c), Ok(()));
}

#[test]
fn the_errors_name_the_broken_rule() {
    assert_eq!(
        systemscope_os::KernelConfigError::Overlap("staging", "the frame pool").to_string(),
        "staging and the frame pool overlap"
    );
    assert_eq!(
        PlanError::Layout("the frame pool is not page-aligned").to_string(),
        "layout: the frame pool is not page-aligned"
    );
}
