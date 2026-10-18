//! Perturbations A and B on the reference platforms (`docs/m5-design.md` §7).
//!
//! A replaces constant latencies, which only the platform builder knows: the links' in
//! the topology and the targets' response latencies in their configurations. B is the
//! runtime's per-message jitter, passed through to the session.

use std::collections::BTreeMap;

/// The largest cycle count an override may set.
pub const MAX_CYCLES: u64 = 64;

/// Perturbation A: latency overrides. The empty value keeps every frozen latency.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Latencies {
    /// A link's latency in CPU cycles, by declaration index. At least 1: a link never
    /// delivers in the cycle it was sent.
    pub links: BTreeMap<u32, u64>,
    /// A target's response latency in CPU cycles after acceptance, by component path.
    pub respond: BTreeMap<String, u64>,
}

impl Latencies {
    /// Link `index`'s cycles, or `frozen`.
    pub fn link(&self, index: u32, frozen: u64) -> u64 {
        self.links.get(&index).copied().unwrap_or(frozen)
    }

    /// `path`'s response cycles, or `frozen`.
    pub fn respond(&self, path: &str, frozen: u64) -> u64 {
        self.respond.get(path).copied().unwrap_or(frozen)
    }

    /// Checks the overrides against a platform with `links` links whose responding
    /// targets are `targets`.
    pub fn check(&self, links: u32, targets: &[&str]) -> Result<(), &'static str> {
        for (&index, &cycles) in &self.links {
            if index >= links {
                return Err("an overridden link does not exist");
            }
            if !(1..=MAX_CYCLES).contains(&cycles) {
                return Err("a link latency is outside 1 to MAX_CYCLES");
            }
        }
        for (path, &cycles) in &self.respond {
            if !targets.contains(&path.as_str()) {
                return Err("an overridden path is not a responding target");
            }
            if cycles > MAX_CYCLES {
                return Err("a response latency is above MAX_CYCLES");
            }
        }
        Ok(())
    }
}
