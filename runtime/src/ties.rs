//! Tie order, perturbation C (`docs/m5-design.md` §7.3).
//!
//! In a tied session the scheduler assigns `sequence = rank << COUNTER_BITS | counter`.
//! `counter` is the scheduling counter, unique and in scheduling order; `rank` is drawn
//! from the seed and the counter. Events with equal `(tick, phase)` then dispatch in rank
//! order, a seeded permutation, instead of in scheduling order. An event scheduled into
//! the `(tick, phase)` being dispatched draws its rank at or above the current one, so its
//! key is always after the current event's.

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};

/// BLAKE3 `derive_key` context for rank draws. Part of the contract: changing it changes
/// every tied run.
pub const TIES_CONTEXT: &str = "SystemScope 2026-10 ties v1";

/// The low bits of a tied sequence that hold the counter.
pub const COUNTER_BITS: u32 = 40;

/// The bits above the counter that hold the rank.
pub const RANK_BITS: u32 = 24;

/// Perturbation C's setting: part of the session configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ties {
    /// Seeds every rank.
    pub seed: u64,
}

impl Ties {
    /// Appends the canonical encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.seed);
    }

    /// Reads one setting.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Ties, DecodeError> {
        Ok(Ties { seed: d.u64()? })
    }

    /// The rank of the event with `counter`, drawn uniformly from `[floor, 2^RANK_BITS)`.
    pub fn rank(&self, counter: u64, floor: u64) -> u64 {
        debug_assert!(floor < 1 << RANK_BITS);
        let mut material = [0u8; 16];
        material[..8].copy_from_slice(&self.seed.to_le_bytes());
        material[8..].copy_from_slice(&counter.to_le_bytes());
        let out = blake3::derive_key(TIES_CONTEXT, &material);
        let word = u64::from_le_bytes(out[..8].try_into().expect("8 bytes"));
        floor + word % ((1 << RANK_BITS) - floor)
    }
}

/// The counter part of a sequence.
pub fn counter(sequence: u64) -> u64 {
    sequence & ((1 << COUNTER_BITS) - 1)
}

/// The rank part of a sequence: 0 in an untied session.
pub fn rank(sequence: u64) -> u64 {
    sequence >> COUNTER_BITS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rank_stays_in_its_range_and_splits_from_the_counter() {
        let ties = Ties { seed: 7 };
        for counter in 0..2_000 {
            for floor in [0, 1, 1 << 23, (1 << RANK_BITS) - 1] {
                let r = ties.rank(counter, floor);
                assert!((floor..1 << RANK_BITS).contains(&r));
                let sequence = r << COUNTER_BITS | counter;
                assert_eq!((rank(sequence), super::counter(sequence)), (r, counter));
            }
        }
        assert_eq!(ties.rank(5, 0), ties.rank(5, 0));
        assert_ne!(ties.rank(5, 0), Ties { seed: 8 }.rank(5, 0));
    }
}
