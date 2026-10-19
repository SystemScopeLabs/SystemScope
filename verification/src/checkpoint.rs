//! The M4 checkpoint: one side's runtime snapshot with the boundaries it covers
//! (`docs/m4-design.md` §12.1, §12.2).

use std::fmt;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};

/// How far one stream had come at a checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamMark {
    /// The stream.
    pub stream: String,
    /// The boundaries emitted by the events the checkpoint covers.
    pub covered: u64,
    /// The prefix digest `P(covered)`.
    pub prefix: [u8; 32],
}

/// A side's state after `event_index` events, and every compared stream's progress there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The `ExecutionId` of the side it belongs to.
    pub execution: [u8; 32],
    /// The events dispatched.
    pub event_index: u64,
    /// One mark per compared stream, in stream-name order.
    pub streams: Vec<StreamMark>,
    /// The unchanged runtime snapshot.
    pub state: Vec<u8>,
    /// The projection's state across the boundary ([`crate::runtime::Projection::save`]);
    /// empty for a projection that keeps none.
    pub projection: Vec<u8>,
}

/// Why checkpoint bytes were refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    /// The canonical encoding is broken.
    Decode(DecodeError),
    /// The stream marks are not in strictly increasing name order.
    StreamOrder,
    /// An empty projection state is encoded; it must be left out.
    EmptyProjection,
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckpointError::Decode(e) => write!(f, "checkpoint: {e}"),
            CheckpointError::StreamOrder => f.write_str("checkpoint: streams out of order"),
            CheckpointError::EmptyProjection => {
                f.write_str("checkpoint: an empty projection state is encoded")
            }
        }
    }
}

impl std::error::Error for CheckpointError {}

impl From<DecodeError> for CheckpointError {
    fn from(e: DecodeError) -> CheckpointError {
        CheckpointError::Decode(e)
    }
}

impl Checkpoint {
    /// The mark of `stream`, if the checkpoint has one.
    pub fn mark(&self, stream: &str) -> Option<&StreamMark> {
        self.streams.iter().find(|m| m.stream == stream)
    }

    /// The canonical encoding: the M0 primitives, the streams in name order, then the
    /// projection state only when it is not empty, so a checkpoint of a projection that
    /// keeps none encodes as in M4.
    pub fn encode(&self, e: &mut Encoder) {
        e.raw(&self.execution);
        e.u64(self.event_index);
        e.len(self.streams.len());
        for m in &self.streams {
            e.str(&m.stream);
            e.u64(m.covered);
            e.raw(&m.prefix);
        }
        e.bytes(&self.state);
        if !self.projection.is_empty() {
            e.bytes(&self.projection);
        }
    }

    /// The canonical bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        self.encode(&mut e);
        e.into_bytes()
    }

    /// Reads a checkpoint, refusing marks out of name order. The projection state is
    /// read when bytes remain, so a checkpoint is the last item of its decoder.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Checkpoint, CheckpointError> {
        let execution = d.array()?;
        let event_index = d.u64()?;
        let count = d.len()?;
        // A mark takes at least a name length, a count, and a digest.
        if count > d.remaining() / 44 {
            return Err(DecodeError::Truncated.into());
        }
        let mut streams: Vec<StreamMark> = Vec::with_capacity(count);
        for _ in 0..count {
            let stream = d.str()?.to_owned();
            if streams.last().is_some_and(|m| m.stream >= stream) {
                return Err(CheckpointError::StreamOrder);
            }
            streams.push(StreamMark {
                stream,
                covered: d.u64()?,
                prefix: d.array()?,
            });
        }
        let state = d.bytes()?.to_vec();
        let projection = if d.remaining() == 0 {
            Vec::new()
        } else {
            let p = d.bytes()?.to_vec();
            if p.is_empty() {
                return Err(CheckpointError::EmptyProjection);
            }
            p
        };
        Ok(Checkpoint {
            execution,
            event_index,
            streams,
            state,
            projection,
        })
    }

    /// Reads exactly one checkpoint from `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Checkpoint, CheckpointError> {
        let mut d = Decoder::new(bytes);
        let c = Checkpoint::decode(&mut d)?;
        d.finish()?;
        Ok(c)
    }
}
