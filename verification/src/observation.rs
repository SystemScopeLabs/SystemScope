//! Observations, their canonical encoding v1, and the prefix digest (`docs/m4-design.md`
//! §8).

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::trace::Value;

/// A field's value at one boundary (§8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedValue {
    /// The field is in the profile, and the backend reports no value for it here.
    Absent,
    /// The reported value.
    Present(Value),
}

/// One comparison boundary of one stream (§8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The stream: `arch`, `effect`, or `os`.
    pub stream: String,
    /// The position in its stream, from 0.
    pub ordinal: u64,
    /// The boundary kind, such as `retire` or `exception`.
    pub kind: String,
    /// The fields, in the profile's order for this kind.
    pub fields: Vec<(String, ObservedValue)>,
}

impl Observation {
    /// The value of the field `name`, if the observation has it.
    pub fn field(&self, name: &str) -> Option<&ObservedValue> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Writes ObservedValue canonical encoding v1 (§8.3).
    pub fn encode(&self, e: &mut Encoder) {
        e.str(&self.stream);
        e.u64(self.ordinal);
        e.str(&self.kind);
        e.len(self.fields.len());
        for (name, value) in &self.fields {
            e.str(name);
            match value {
                ObservedValue::Absent => e.u8(0x00),
                ObservedValue::Present(v) => {
                    e.u8(0x01);
                    v.encode(e);
                }
            }
        }
    }

    /// The canonical bytes of this observation alone.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        self.encode(&mut e);
        e.into_bytes()
    }

    /// Reads one observation, refusing every tag v1 does not define.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Observation, DecodeError> {
        let stream = d.str()?.to_owned();
        let ordinal = d.u64()?;
        let kind = d.str()?.to_owned();
        let count = d.len()?;
        // Each field takes at least a name length and a tag, so a count beyond the input
        // is corrupt; refusing it here keeps a forged count from reserving memory.
        if count > d.remaining() / 5 {
            return Err(DecodeError::Truncated);
        }
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let name = d.str()?.to_owned();
            let value = match d.u8()? {
                0x00 => ObservedValue::Absent,
                0x01 => ObservedValue::Present(decode_value(d)?),
                tag => {
                    return Err(DecodeError::InvalidTag {
                        what: "observed value",
                        tag,
                    });
                }
            };
            fields.push((name, value));
        }
        Ok(Observation {
            stream,
            ordinal,
            kind,
            fields,
        })
    }
}

/// Reads a contracts `Value` in the encoding traces use.
fn decode_value(d: &mut Decoder<'_>) -> Result<Value, DecodeError> {
    Ok(match d.u8()? {
        0 => Value::U64(d.u64()?),
        1 => Value::I64(d.i64()?),
        2 => Value::Bool(d.bool()?),
        3 => Value::Str(d.str()?.to_owned()),
        4 => Value::Bytes(d.bytes()?.to_vec()),
        tag => return Err(DecodeError::InvalidTag { what: "value", tag }),
    })
}

/// A stream's prefix digest: `P(0) = [0; 32]`, `P(i+1) = BLAKE3(P(i) ‖ encode(obs_i))`
/// (§8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixDigest {
    digest: [u8; 32],
    count: u64,
}

impl Default for PrefixDigest {
    fn default() -> PrefixDigest {
        PrefixDigest::new()
    }
}

impl PrefixDigest {
    /// `P(0)`.
    pub fn new() -> PrefixDigest {
        PrefixDigest {
            digest: [0; 32],
            count: 0,
        }
    }

    /// Absorbs the next observation of the stream.
    pub fn absorb(&mut self, obs: &Observation) {
        let mut h = blake3::Hasher::new();
        h.update(&self.digest);
        h.update(&obs.to_bytes());
        self.digest = *h.finalize().as_bytes();
        self.count += 1;
    }

    /// `P(count)`.
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// The number of observations absorbed.
    pub fn count(&self) -> u64 {
        self.count
    }
}
