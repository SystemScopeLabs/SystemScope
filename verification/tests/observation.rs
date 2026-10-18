//! ObservedValue canonical encoding v1 and the prefix digest (`docs/m4-design.md` §8.3).

use systemscope_contracts::canonical::{DecodeError, Decoder};
use systemscope_contracts::trace::Value;
use systemscope_verify::observation::{Observation, ObservedValue, PrefixDigest};

fn obs(ordinal: u64, kind: &str, fields: Vec<(&str, ObservedValue)>) -> Observation {
    Observation {
        stream: "arch".to_owned(),
        ordinal,
        kind: kind.to_owned(),
        fields: fields.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
    }
}

fn present(v: Value) -> ObservedValue {
    ObservedValue::Present(v)
}

fn every_value() -> Observation {
    obs(
        7,
        "retire",
        vec![
            ("pc", present(Value::U64(0x8000_0000))),
            ("rd", ObservedValue::Absent),
            ("i", present(Value::I64(-2))),
            ("b", present(Value::Bool(true))),
            ("s", present(Value::Str("U".to_owned()))),
            ("x", present(Value::Bytes(vec![0xAB, 0xCD]))),
        ],
    )
}

fn decode(bytes: &[u8]) -> Result<Observation, DecodeError> {
    let mut d = Decoder::new(bytes);
    let obs = Observation::decode(&mut d)?;
    d.finish()?;
    Ok(obs)
}

#[test]
fn encoding_v1_is_pinned_byte_for_byte() {
    let o = obs(
        1,
        "k",
        vec![("a", ObservedValue::Absent), ("b", present(Value::U64(2)))],
    );
    let mut expected = Vec::new();
    expected.extend([4, 0, 0, 0]); // stream length
    expected.extend(b"arch");
    expected.extend(1u64.to_le_bytes()); // ordinal
    expected.extend([1, 0, 0, 0, b'k']); // kind
    expected.extend([2, 0, 0, 0]); // field count
    expected.extend([1, 0, 0, 0, b'a', 0x00]); // Absent
    expected.extend([1, 0, 0, 0, b'b', 0x01, 0]); // Present, Value tag U64
    expected.extend(2u64.to_le_bytes());
    assert_eq!(o.to_bytes(), expected);
}

#[test]
fn every_value_round_trips() {
    let o = every_value();
    assert_eq!(decode(&o.to_bytes()), Ok(o));
    let empty = obs(0, "shutdown", Vec::new());
    assert_eq!(decode(&empty.to_bytes()), Ok(empty));
}

#[test]
fn undefined_tags_are_corrupt() {
    let o = obs(0, "k", vec![("f", present(Value::U64(0)))]);
    let bytes = o.to_bytes();
    // The ObservedValue tag sits after the field name.
    let tag_at = bytes.len() - 8 - 1 - 1;
    assert_eq!(bytes[tag_at], 0x01);
    for tag in 0x02..=0xFF {
        let mut b = bytes.clone();
        b[tag_at] = tag;
        assert_eq!(
            decode(&b),
            Err(DecodeError::InvalidTag {
                what: "observed value",
                tag
            })
        );
    }
    for tag in 5..=0xFF {
        let mut b = bytes.clone();
        b[tag_at + 1] = tag;
        assert_eq!(
            decode(&b),
            Err(DecodeError::InvalidTag { what: "value", tag })
        );
    }
}

#[test]
fn truncated_and_padded_bytes_are_corrupt() {
    let bytes = every_value().to_bytes();
    for n in 0..bytes.len() {
        assert_eq!(decode(&bytes[..n]), Err(DecodeError::Truncated), "{n}");
    }
    let mut padded = bytes;
    padded.push(0);
    assert_eq!(decode(&padded), Err(DecodeError::TrailingBytes));
}

#[test]
fn a_forged_field_count_is_refused_before_allocating() {
    let mut bytes = obs(0, "k", Vec::new()).to_bytes();
    let n = bytes.len();
    bytes[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode(&bytes), Err(DecodeError::Truncated));
    // Three fields cannot fit in ten bytes, so the count is refused before the first
    // field's undefined tag is read.
    let mut bytes = obs(0, "k", Vec::new()).to_bytes();
    let n = bytes.len();
    bytes[n - 4..].copy_from_slice(&3u32.to_le_bytes());
    bytes.extend([0, 0, 0, 0, 0x07, 0, 0, 0, 0, 0]);
    assert_eq!(decode(&bytes), Err(DecodeError::Truncated));
}

#[test]
fn absent_and_empty_values_are_all_distinct() {
    let values = [
        ObservedValue::Absent,
        present(Value::U64(0)),
        present(Value::I64(0)),
        present(Value::Bool(false)),
        present(Value::Str(String::new())),
        present(Value::Bytes(Vec::new())),
    ];
    let digests: Vec<[u8; 32]> = values
        .iter()
        .map(|v| {
            let mut p = PrefixDigest::new();
            p.absorb(&obs(0, "retire", vec![("rd_value", v.clone())]));
            p.digest()
        })
        .collect();
    for i in 0..values.len() {
        for j in i + 1..values.len() {
            assert_ne!(digests[i], digests[j], "{:?} {:?}", values[i], values[j]);
        }
    }
}

#[test]
fn the_prefix_digest_chains_blake3_over_each_encoding() {
    let a = every_value();
    let b = obs(8, "exception", vec![("cause", present(Value::U64(8)))]);
    let mut p = PrefixDigest::new();
    assert_eq!((p.digest(), p.count()), ([0; 32], 0));
    assert_eq!(PrefixDigest::default(), p);
    p.absorb(&a);
    let p1 = *blake3::hash(&[[0; 32].as_slice(), &a.to_bytes()].concat()).as_bytes();
    assert_eq!((p.digest(), p.count()), (p1, 1));
    p.absorb(&b);
    let p2 = *blake3::hash(&[p1.as_slice(), &b.to_bytes()].concat()).as_bytes();
    assert_eq!((p.digest(), p.count()), (p2, 2));
    // Resuming from `P(1)` continues exactly where the uninterrupted digest is.
    let mut r = PrefixDigest::resume(p1, 1);
    assert_eq!((r.digest(), r.count()), (p1, 1));
    r.absorb(&b);
    assert_eq!(r, p);
}

#[test]
fn the_digest_covers_every_part_of_an_observation() {
    let base = obs(3, "retire", vec![("pc", present(Value::U64(4)))]);
    let digest = |o: &Observation| {
        let mut p = PrefixDigest::new();
        p.absorb(o);
        p.digest()
    };
    let mut stream = base.clone();
    stream.stream = "os".to_owned();
    let mut ordinal = base.clone();
    ordinal.ordinal = 4;
    let mut kind = base.clone();
    kind.kind = "exception".to_owned();
    let mut name = base.clone();
    name.fields[0].0 = "insn".to_owned();
    let mut payload = base.clone();
    payload.fields[0].1 = present(Value::U64(5));
    for changed in [stream, ordinal, kind, name, payload] {
        assert_ne!(digest(&changed), digest(&base), "{changed:?}");
    }
}

#[test]
fn field_finds_values_by_name() {
    let o = every_value();
    assert_eq!(o.field("rd"), Some(&ObservedValue::Absent));
    assert_eq!(o.field("i"), Some(&present(Value::I64(-2))));
    assert_eq!(o.field("csr"), None);
}
