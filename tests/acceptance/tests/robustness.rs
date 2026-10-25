//! Robustness of every decoder that reads bytes from outside the process: snapshots,
//! reproducer bundles, and perturbations. Corrupted or arbitrary bytes must be refused
//! with an error, never a panic, and a binary encoding or snapshot that decodes must be
//! the canonical one. The committed truncation tests (`at2`, `m1_a6`) cover every prefix of a snapshot;
//! these flip bytes anywhere in a valid encoding and feed arbitrary bytes.

use std::fs;
use std::sync::LazyLock;

use proptest::prelude::*;
use systemscope_acceptance::m5::minimize::PINNED;
use systemscope_acceptance::m5::{Fixture, MID_SNAPSHOT_PATH};
use systemscope_reference::{ReferenceConfig, build};
use systemscope_rv32::perturb::Perturbation;
use systemscope_rv32::workspace_root;
use systemscope_verify::bundle::{MANIFEST, Manifest, ObservationEvidence, RESULT};
use systemscope_verify::result::VerificationResult;

const M0_MID: &[u8] = include_bytes!("../../golden/m0-reference.mid.snap");
const CORPUS_ENTRY: &str = "tests/verification/corpus/sp2";

static M5: LazyLock<(Fixture, Vec<u8>)> = LazyLock::new(|| {
    let root = workspace_root();
    let fixture = Fixture::read(&root).unwrap();
    (fixture, fs::read(root.join(MID_SNAPSHOT_PATH)).unwrap())
});

fn corpus(name: &str) -> Vec<u8> {
    fs::read(workspace_root().join(CORPUS_ENTRY).join(name)).unwrap()
}

/// `bytes` with each `(position, mask)` XORed in, positions taken modulo the length.
fn flipped(bytes: &[u8], flips: &[(usize, u8)]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for &(at, mask) in flips {
        let at = at % out.len();
        out[at] ^= mask;
    }
    out
}

fn flips() -> impl Strategy<Value = Vec<(usize, u8)>> {
    prop::collection::vec((any::<usize>(), 1..=u8::MAX), 1..4)
}

fn bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..512)
}

/// A binary decoder either refuses `bytes` or decodes the canonical encoding of what it
/// returns.
fn strict<T>(bytes: &[u8], decode: impl Fn(&[u8]) -> Option<T>, encode: impl Fn(&T) -> Vec<u8>) {
    if let Some(value) = decode(bytes) {
        assert_eq!(encode(&value), bytes, "a non-canonical encoding decoded");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn a_flipped_m0_snapshot_restores_or_is_refused(flips in flips()) {
        let seed = systemscope_acceptance::FIXED_SEEDS[0];
        let bytes = flipped(M0_MID, &flips);
        let mut rt = build(ReferenceConfig::full(seed));
        if rt.restore(&bytes).is_ok() {
            prop_assert_eq!(rt.snapshot().unwrap(), bytes, "a non-canonical snapshot restored");
        }
    }

    #[test]
    fn a_flipped_m5_snapshot_restores_or_is_refused(flips in flips()) {
        let (fixture, mid) = &*M5;
        let bytes = flipped(mid, &flips);
        let mut rt = fixture.platform();
        if rt.restore(&bytes).is_ok() {
            prop_assert_eq!(rt.snapshot().unwrap(), bytes, "a non-canonical snapshot restored");
        }
    }

    #[test]
    fn arbitrary_bytes_never_restore_a_platform(bytes in bytes()) {
        let (fixture, _) = &*M5;
        prop_assert!(fixture.platform().restore(&bytes).is_err());
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn a_verification_result_decodes_strictly(flips in flips(), raw in bytes()) {
        let decode = |b: &[u8]| VerificationResult::from_bytes(b).ok();
        let valid = corpus(RESULT);
        strict(&valid, decode, VerificationResult::to_bytes);
        strict(&flipped(&valid, &flips), decode, VerificationResult::to_bytes);
        strict(&raw, decode, VerificationResult::to_bytes);
    }

    #[test]
    fn observation_evidence_decodes_strictly(flips in flips(), raw in bytes()) {
        let decode = |b: &[u8]| ObservationEvidence::from_bytes(b).ok();
        let valid = corpus("expected.obs");
        strict(&valid, decode, ObservationEvidence::to_bytes);
        strict(&flipped(&valid, &flips), decode, ObservationEvidence::to_bytes);
        strict(&raw, decode, ObservationEvidence::to_bytes);
    }

    #[test]
    fn a_perturbation_decodes_strictly(which in 0..PINNED.len(), flips in flips(), raw in bytes()) {
        let decode = |b: &[u8]| Perturbation::from_bytes(b).ok();
        let valid = PINNED[which].perturbation().to_bytes();
        strict(&valid, decode, Perturbation::to_bytes);
        strict(&flipped(&valid, &flips), decode, Perturbation::to_bytes);
        strict(&raw, decode, Perturbation::to_bytes);
    }

    #[test]
    fn a_manifest_parses_or_is_refused(flips in flips(), raw in bytes()) {
        let valid = corpus(MANIFEST);
        prop_assert!(Manifest::parse(&valid).is_ok());
        let _ = Manifest::parse(&flipped(&valid, &flips));
        let _ = Manifest::parse(&raw);
    }
}
