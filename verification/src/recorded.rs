//! A post-run backend that replays committed observation streams (`docs/m4-design.md`
//! §25.2): the expected side of a source-mutant case, recorded from the unmodified build.
//!
//! Each stream is one `.obs` entry (§16.1) whose first ordinal is 0. The recording is
//! bound to the scenario and the comparison profile it was made under, and replays only
//! with no input variant: it is the unmodified run.

use std::collections::BTreeMap;

use systemscope_contracts::canonical::DecodeError;

use crate::backend::{Backend, Capabilities, Capability};
use crate::bundle::ObservationEvidence;
use crate::case::{BackendId, BackendRunConfig, InputVariant, Scenario};
use crate::error::EngineError;
use crate::observation::Observation;
use crate::profile::{ComparisonProfile, Support};

/// One stream's canonical bytes: a `.obs` entry from ordinal 0.
pub fn stream_bytes(stream: &[Observation]) -> Vec<u8> {
    ObservationEvidence {
        first: 0,
        observations: stream.to_vec(),
    }
    .to_bytes()
}

/// Reads [`stream_bytes`]'s output, refusing a stream that does not start at ordinal 0.
pub fn stream_from_bytes(bytes: &[u8]) -> Result<Vec<Observation>, DecodeError> {
    let evidence = ObservationEvidence::from_bytes(bytes)?;
    if evidence.first != 0 {
        return Err(DecodeError::InvalidTag {
            what: "recorded stream first ordinal",
            tag: evidence.first.min(255) as u8,
        });
    }
    Ok(evidence.observations)
}

/// Committed streams replayed as a backend: record-only, with
/// [`Capability::DeterministicReplay`] and the stream capabilities of what it holds.
#[derive(Clone, Debug)]
pub struct RecordedBackend {
    id: BackendId,
    support: Support,
    scenario: [u8; 32],
    profile: [u8; 32],
    streams: BTreeMap<String, Vec<Observation>>,
    started: bool,
}

impl RecordedBackend {
    /// The streams `streams`, recorded by the backend `id` with `support` on `scenario`
    /// under `profile`.
    pub fn new(
        id: BackendId,
        support: Support,
        scenario: &Scenario,
        profile: &ComparisonProfile,
        streams: BTreeMap<String, Vec<Observation>>,
    ) -> RecordedBackend {
        RecordedBackend {
            id,
            support,
            scenario: scenario.identity(),
            profile: profile.identity(),
            streams,
            started: false,
        }
    }
}

impl Backend for RecordedBackend {
    fn describe(&self) -> BackendId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = Capabilities::from([Capability::DeterministicReplay]);
        for (name, capability) in [
            ("arch", Capability::ArchStream),
            ("effect", Capability::EffectStream),
            ("os", Capability::OsStream),
        ] {
            if self.streams.contains_key(name) {
                c.insert(capability);
            }
        }
        c
    }

    fn support(&self) -> Support {
        self.support.clone()
    }

    /// Accepts only the recording's own scenario and profile, with no input variant.
    fn start(
        &mut self,
        scenario: &Scenario,
        input: &InputVariant,
        _: &BackendRunConfig,
        profile: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        self.started = false;
        if *input != InputVariant::None {
            return Err(EngineError::Unsupported(
                "an input variant on a recording".to_owned(),
            ));
        }
        if scenario.identity() != self.scenario {
            return Err(EngineError::Internal(
                "the recording is of another scenario".to_owned(),
            ));
        }
        if profile.identity() != self.profile {
            return Err(EngineError::Internal(format!(
                "the recording is not under profile {} version {}",
                profile.name, profile.version
            )));
        }
        self.started = true;
        Ok(())
    }

    fn next(&mut self, _: &str) -> Result<Option<Observation>, EngineError> {
        Err(EngineError::Unsupported("step_boundary".to_owned()))
    }

    fn record(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        if !self.started {
            return Err(EngineError::Internal("record before start".to_owned()));
        }
        self.streams
            .get(stream)
            .cloned()
            .ok_or_else(|| EngineError::Internal(format!("no recorded {stream} stream")))
    }
}
