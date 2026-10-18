//! The first state divergence of two same-engine sides on the counter
//! (`docs/m4-design.md` §12.4).

mod common;

use common::*;
use systemscope_verify::backend::Backend;
use systemscope_verify::case::InputVariant;
use systemscope_verify::error::EngineError;
use systemscope_verify::latent::{SameSide, StateDivergence, first_state_divergence};
use systemscope_verify::locate::{Index, record_index};
use systemscope_verify::profile::{Plan, Role, plan};
use systemscope_verify::runtime::RuntimeBackend;

fn the_plan() -> Plan {
    let b = backend();
    plan(&profile(), &b.support(), &b.support()).unwrap()
}

fn index(input: &InputVariant, every: u64) -> Index {
    let mut b = backend();
    record_index(
        &the_plan(),
        &scenario(),
        side(&mut b, input.clone()),
        Role::Expected,
        every,
    )
    .unwrap()
}

fn scoped(
    input: &InputVariant,
    every: u64,
    scope: &[&str],
    until: u64,
) -> Result<StateDivergence, EngineError> {
    let (ie, ia) = (index(&InputVariant::None, every), index(input, every));
    let (mut x, mut y): (RuntimeBackend, RuntimeBackend) = (backend(), backend());
    first_state_divergence(
        &the_plan(),
        &scenario(),
        same(&mut x, &InputVariant::None, &ie),
        same(&mut y, input, &ia),
        scope,
        until,
    )
}

fn scan(input: &InputVariant, every: u64, until: u64) -> StateDivergence {
    scoped(input, every, &[], until).unwrap()
}

fn same<'a>(
    backend: &'a mut RuntimeBackend,
    input: &InputVariant,
    index: &'a Index,
) -> SameSide<'a> {
    SameSide {
        backend,
        input: input.clone(),
        config: config(),
        checkpoints: &index.checkpoints,
    }
}

fn found(origin: u64, event: u64) -> StateDivergence {
    StateDivergence {
        origin,
        event: Some(event),
        differing: vec!["counter".to_owned()],
    }
}

#[test]
fn a_state_patch_diverges_in_state_at_its_event() {
    for k in 1..LIMIT - 1 {
        let input = patch_n(k, k, k + 1);
        // Without checkpoints the scan starts from the initial states.
        assert_eq!(scan(&input, 0, LIMIT), found(0, k), "{k}");
        // The checkpoint after event k already holds the patch: the last equal pair is
        // the one before it.
        assert_eq!(scan(&input, 1, LIMIT), found(k - 1, k), "{k}");
        assert_eq!(scan(&input, 3, LIMIT).event, Some(k), "{k}");
    }
}

#[test]
fn equal_sides_and_a_short_scan_find_nothing() {
    let none = |origin| StateDivergence {
        origin,
        event: None,
        differing: Vec::new(),
    };
    assert_eq!(scan(&InputVariant::None, 0, LIMIT - 1), none(0));
    assert_eq!(scan(&InputVariant::None, 2, LIMIT - 1), none(LIMIT - 2));
    // A checkpoint pair after `until` is no origin, and the scan stops at `until`.
    let input = patch_n(6, 6, 7);
    assert_eq!(scan(&input, 2, 5), none(4));
    assert_eq!(scan(&input, 0, 6), found(0, 6));
}

#[test]
fn a_scope_compares_only_its_components() {
    let input = patch_n(5, 5, 6);
    for every in [0, 1, 4] {
        assert_eq!(
            scoped(&input, every, &["counter"], LIMIT).unwrap(),
            scan(&input, every, LIMIT)
        );
    }
    assert_eq!(
        scoped(&input, 1, &["nothing"], LIMIT),
        Err(EngineError::Internal("no component nothing".to_owned()))
    );
}
