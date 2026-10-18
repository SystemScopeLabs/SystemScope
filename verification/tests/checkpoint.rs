//! The M4 checkpoint encoding (`docs/m4-design.md` §12.2, §25.3).

use systemscope_contracts::canonical::DecodeError;
use systemscope_verify::checkpoint::{Checkpoint, CheckpointError, StreamMark};

fn mark(stream: &str, covered: u64, byte: u8) -> StreamMark {
    StreamMark {
        stream: stream.to_owned(),
        covered,
        prefix: [byte; 32],
    }
}

fn checkpoint() -> Checkpoint {
    Checkpoint {
        execution: [7; 32],
        event_index: 0x0102_0304_0506_0708,
        streams: vec![mark("arch", 3, 1), mark("effect", 0, 0), mark("os", 9, 2)],
        state: b"SSSNAP".to_vec(),
    }
}

#[test]
fn a_checkpoint_round_trips_in_its_canonical_layout() {
    let c = checkpoint();
    let bytes = c.to_bytes();
    assert_eq!(Checkpoint::from_bytes(&bytes), Ok(c.clone()));
    let mut want = vec![7; 32];
    want.extend(0x0102_0304_0506_0708u64.to_le_bytes());
    want.extend(3u32.to_le_bytes());
    for (name, covered, byte) in [("arch", 3u64, 1u8), ("effect", 0, 0), ("os", 9, 2)] {
        want.extend((name.len() as u32).to_le_bytes());
        want.extend(name.as_bytes());
        want.extend(covered.to_le_bytes());
        want.extend([byte; 32]);
    }
    want.extend(6u32.to_le_bytes());
    want.extend(b"SSSNAP");
    assert_eq!(bytes, want);
    assert_eq!(c.mark("effect"), Some(&mark("effect", 0, 0)));
    assert_eq!(c.mark("bus"), None);
}

#[test]
fn broken_checkpoint_bytes_are_refused() {
    let bytes = checkpoint().to_bytes();
    for n in 0..bytes.len() {
        assert_eq!(
            Checkpoint::from_bytes(&bytes[..n]),
            Err(CheckpointError::Decode(DecodeError::Truncated)),
            "{n}"
        );
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert_eq!(
        Checkpoint::from_bytes(&padded),
        Err(CheckpointError::Decode(DecodeError::TrailingBytes))
    );
    // A forged mark count is refused before allocating; one mark takes 44 bytes at least.
    let mut forged = checkpoint();
    forged.streams.clear();
    forged.state = vec![0; 40];
    let mut bytes = forged.to_bytes();
    bytes[40..44].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(
        Checkpoint::from_bytes(&bytes),
        Err(CheckpointError::Decode(DecodeError::Truncated))
    );
}

#[test]
fn stream_marks_must_be_in_strictly_increasing_name_order() {
    for streams in [
        vec![mark("os", 0, 0), mark("arch", 0, 0)],
        vec![mark("arch", 0, 0), mark("arch", 1, 0)],
    ] {
        let c = Checkpoint {
            streams,
            ..checkpoint()
        };
        assert_eq!(
            Checkpoint::from_bytes(&c.to_bytes()),
            Err(CheckpointError::StreamOrder)
        );
    }
    assert_eq!(
        CheckpointError::StreamOrder.to_string(),
        "checkpoint: streams out of order"
    );
    assert_eq!(
        CheckpointError::Decode(DecodeError::Truncated).to_string(),
        "checkpoint: input ends inside a value"
    );
}
