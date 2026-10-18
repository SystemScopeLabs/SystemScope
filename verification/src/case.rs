//! Scenarios, input variants, run configurations, and execution identities
//! (`docs/m4-design.md` §6.6).

use systemscope_contracts::canonical::Encoder;
use systemscope_contracts::trace::Value;

/// The common semantic inputs of a comparison case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scenario {
    /// The platform's semantic name, such as `m3-reference`.
    pub platform: String,
    /// The platform's topology hash.
    pub topology_hash: [u8; 32],
    /// The CPU profile: `M1`, `M2`, or `M3`.
    pub cpu_profile: String,
    /// The BLAKE3 of the program or disk bytes.
    pub program: [u8; 32],
    /// A repository-relative path to those bytes: a locator only, never identity.
    pub locator: String,
    /// The session seed.
    pub seed: u64,
}

impl Scenario {
    /// The BLAKE3 of the canonical encoding, without the locator.
    pub fn identity(&self) -> [u8; 32] {
        let mut e = Encoder::new();
        e.str(&self.platform);
        e.raw(&self.topology_hash);
        e.str(&self.cpu_profile);
        e.raw(&self.program);
        e.u64(self.seed);
        *blake3::hash(e.as_bytes()).as_bytes()
    }
}

/// A state patch: one field of the checkpoint after `event_index` events (§25.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePatch {
    /// The events dispatched before the patch.
    pub event_index: u64,
    /// The patched field, as a platform names it, such as `soc.cpu/pc`.
    pub field: String,
    /// The value the checkpoint must hold.
    pub old: Value,
    /// The value written.
    pub new: Value,
}

/// A side's canonical change to the common scenario.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InputVariant {
    /// Both sides run the scenario as it is.
    #[default]
    None,
    /// A field of a checkpoint is patched.
    State(StatePatch),
}

impl InputVariant {
    /// The BLAKE3 of the canonical encoding.
    pub fn identity(&self) -> [u8; 32] {
        let mut e = Encoder::new();
        match self {
            InputVariant::None => e.u8(0),
            InputVariant::State(p) => {
                e.u8(1);
                e.u64(p.event_index);
                e.str(&p.field);
                p.old.encode(&mut e);
                p.new.encode(&mut e);
            }
        }
        *blake3::hash(e.as_bytes()).as_bytes()
    }
}

/// A backend's identity: what it is and which build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendId {
    /// The backend name, such as `systemscope`.
    pub name: String,
    /// Its pinned version.
    pub version: String,
    /// Its build identity, such as an ISA configuration.
    pub build: String,
}

impl BackendId {
    /// The BLAKE3 of the canonical encoding.
    pub fn identity(&self) -> [u8; 32] {
        let mut e = Encoder::new();
        e.str(&self.name);
        e.str(&self.version);
        e.str(&self.build);
        *blake3::hash(e.as_bytes()).as_bytes()
    }
}

/// `ExecutionId = BLAKE3(scenario identity ‖ input variant identity ‖ backend identity)`.
pub fn execution_id(scenario: &Scenario, input: &InputVariant, backend: &BackendId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&scenario.identity());
    h.update(&input.identity());
    h.update(&backend.identity());
    *h.finalize().as_bytes()
}

/// Per-side operational controls that never change the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendRunConfig {
    /// The safety budget, in the backend's own unit: events for SystemScope.
    pub budget: u64,
}
