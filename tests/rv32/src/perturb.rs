//! Perturbations A and B on the reference platforms (`docs/m5-design.md` §7).
//!
//! A replaces constant latencies, which only the platform builder knows: the links' in
//! the topology and the targets' response latencies in their configurations. B is the
//! runtime's per-message jitter, passed through to the session. [`Perturbation`] is both,
//! with its canonical encoding and identity.

use std::collections::BTreeMap;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_runtime::jitter::Jitter;
use systemscope_runtime::ties::Ties;

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

/// A run's whole perturbation (`docs/m5-design.md` §7.4): A, B, and C. The empty value
/// is the identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Perturbation {
    /// Perturbation A.
    pub latencies: Latencies,
    /// Perturbation A on an M5 kernel: its time slice in timer cycles, replacing the
    /// platform's. A preemption point moves by the difference at every slice, so the
    /// points spread over the programs' loops.
    pub slice: Option<u32>,
    /// Perturbation B.
    pub jitter: Option<Jitter>,
    /// Perturbation C, the tie order.
    pub ties: Option<Ties>,
}

impl Perturbation {
    /// Whether it is the identity.
    pub fn is_empty(&self) -> bool {
        *self == Perturbation::default()
    }

    /// The canonical encoding, a reproducer's `perturbation.bin`: the link overrides in
    /// index order, the response overrides in path order, then the slice, the jitter
    /// setting, and the tie setting, each after a tag. A jitter setting with a list of
    /// messages has tag 2 and the list after the setting.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.len(self.latencies.links.len());
        for (&index, &cycles) in &self.latencies.links {
            e.u32(index);
            e.u64(cycles);
        }
        e.len(self.latencies.respond.len());
        for (path, &cycles) in &self.latencies.respond {
            e.str(path);
            e.u64(cycles);
        }
        match self.slice {
            None => e.u8(0),
            Some(slice) => {
                e.u8(1);
                e.u32(slice);
            }
        }
        match &self.jitter {
            None => e.u8(0),
            Some(j) if j.only.is_none() => {
                e.u8(1);
                j.encode(&mut e);
            }
            Some(j) => {
                e.u8(2);
                j.encode(&mut e);
                j.encode_only(&mut e);
            }
        }
        match &self.ties {
            None => e.u8(0),
            Some(t) => {
                e.u8(1);
                t.encode(&mut e);
            }
        }
        e.into_bytes()
    }

    /// Reads exactly one perturbation, refusing overrides out of order.
    pub fn from_bytes(bytes: &[u8]) -> Result<Perturbation, DecodeError> {
        let mut d = Decoder::new(bytes);
        let mut p = Perturbation::default();
        let out_of_order = DecodeError::InvalidTag {
            what: "override order",
            tag: 0,
        };
        for _ in 0..d.len()? {
            let index = d.u32()?;
            if p.latencies
                .links
                .last_key_value()
                .is_some_and(|(&i, _)| i >= index)
            {
                return Err(out_of_order);
            }
            p.latencies.links.insert(index, d.u64()?);
        }
        for _ in 0..d.len()? {
            let path = d.str()?.to_owned();
            if p.latencies
                .respond
                .last_key_value()
                .is_some_and(|(q, _)| *q >= path)
            {
                return Err(out_of_order);
            }
            p.latencies.respond.insert(path, d.u64()?);
        }
        p.slice = match d.u8()? {
            0 => None,
            1 => Some(d.u32()?),
            tag => return Err(DecodeError::InvalidTag { what: "slice", tag }),
        };
        p.jitter = match d.u8()? {
            0 => None,
            1 => Some(Jitter::decode(&mut d)?),
            2 => {
                let mut j = Jitter::decode(&mut d)?;
                j.decode_only(&mut d)?;
                Some(j)
            }
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "jitter",
                    tag,
                });
            }
        };
        p.ties = match d.u8()? {
            0 => None,
            1 => Some(Ties::decode(&mut d)?),
            tag => return Err(DecodeError::InvalidTag { what: "ties", tag }),
        };
        d.finish()?;
        Ok(p)
    }

    /// The BLAKE3 of the canonical encoding.
    pub fn identity(&self) -> [u8; 32] {
        *blake3::hash(&self.to_bytes()).as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use systemscope_runtime::jitter::LinkSet;

    fn sample() -> Perturbation {
        Perturbation {
            latencies: Latencies {
                links: [(2, 5), (9, 1)].into(),
                respond: [("soc.ram".to_owned(), 3)].into(),
            },
            slice: Some(7000),
            jitter: Some(Jitter {
                seed: 0x5eed,
                max_cycles: 8,
                links: LinkSet::of(&[2, 4]),
                only: None,
            }),
            ties: Some(Ties { seed: 0x71e5 }),
        }
    }

    #[test]
    fn a_perturbation_round_trips_and_its_identity_follows_its_bytes() {
        for p in [Perturbation::default(), sample()] {
            assert_eq!(Perturbation::from_bytes(&p.to_bytes()), Ok(p.clone()));
            assert_eq!(p.identity(), *blake3::hash(&p.to_bytes()).as_bytes());
        }
        assert!(Perturbation::default().is_empty());
        assert!(!sample().is_empty());
        assert_eq!(Perturbation::default().to_bytes(), [0; 11]);
        let mut seed = sample();
        seed.jitter.as_mut().unwrap().seed += 1;
        assert_ne!(seed.identity(), sample().identity());
        let mut ties = sample();
        ties.ties = None;
        assert_ne!(ties.identity(), sample().identity());
        let mut listed = sample();
        listed.jitter.as_mut().unwrap().only = Some(vec![(2, 0, 3), (4, 1, 0)].into());
        assert_eq!(
            Perturbation::from_bytes(&listed.to_bytes()),
            Ok(listed.clone())
        );
        assert_ne!(listed.identity(), sample().identity());
    }

    #[test]
    fn a_perturbation_refuses_disorder_a_bad_tag_and_trailing_bytes() {
        let encoded = |links: &[(u32, u64)], paths: &[&str], tags: [u8; 3]| {
            let mut e = Encoder::new();
            e.len(links.len());
            for &(i, c) in links {
                e.u32(i);
                e.u64(c);
            }
            e.len(paths.len());
            for path in paths {
                e.str(path);
                e.u64(1);
            }
            for tag in tags {
                e.u8(tag);
            }
            e.into_bytes()
        };
        let order = Err(DecodeError::InvalidTag {
            what: "override order",
            tag: 0,
        });
        assert_eq!(
            Perturbation::from_bytes(&encoded(&[(3, 1), (3, 2)], &[], [0; 3])),
            order
        );
        assert_eq!(
            Perturbation::from_bytes(&encoded(&[(4, 1), (3, 2)], &[], [0; 3])),
            order
        );
        assert_eq!(
            Perturbation::from_bytes(&encoded(&[], &["b", "a"], [0; 3])),
            order
        );
        assert_eq!(
            Perturbation::from_bytes(&encoded(&[], &[], [2, 0, 0])),
            Err(DecodeError::InvalidTag {
                what: "slice",
                tag: 2
            })
        );
        assert_eq!(
            Perturbation::from_bytes(&encoded(&[], &[], [0, 3, 0])),
            Err(DecodeError::InvalidTag {
                what: "jitter",
                tag: 3
            })
        );
        assert_eq!(
            Perturbation::from_bytes(&encoded(&[], &[], [0, 0, 2])),
            Err(DecodeError::InvalidTag {
                what: "ties",
                tag: 2
            })
        );
        let mut trailing = sample().to_bytes();
        trailing.push(0);
        assert!(Perturbation::from_bytes(&trailing).is_err());
        let bytes = sample().to_bytes();
        assert!(Perturbation::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    }
}
