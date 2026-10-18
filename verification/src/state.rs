//! Structured state and its diff (`docs/m4-design.md` §13).
//!
//! A side's state is a list of named fields, each labeled with the source that read it.
//! No source claims completeness: a diff reports what both sides' sources returned.

use std::collections::BTreeMap;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::trace::Value;

use crate::observation::decode_value;
use crate::profile::Role;

/// Where a state field came from (§13.2).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The component's own `inspect()`.
    Inspect,
    /// A schema decoder of the component's snapshot bytes, named by its schema.
    Decoder(String),
    /// Computed from other evidence, named by how.
    Derived(String),
}

impl Source {
    fn encode(&self, e: &mut Encoder) {
        match self {
            Source::Inspect => e.u8(0),
            Source::Decoder(s) => {
                e.u8(1);
                e.str(s);
            }
            Source::Derived(s) => {
                e.u8(2);
                e.str(s);
            }
        }
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Source, DecodeError> {
        Ok(match d.u8()? {
            0 => Source::Inspect,
            1 => Source::Decoder(d.str()?.to_owned()),
            2 => Source::Derived(d.str()?.to_owned()),
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "source",
                    tag,
                });
            }
        })
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Inspect => f.write_str("inspect"),
            Source::Decoder(s) => write!(f, "decoder:{s}"),
            Source::Derived(s) => write!(f, "derived:{s}"),
        }
    }
}

/// One named value of a side's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateField {
    /// The component's topology path, such as `soc.cpu0`.
    pub component: String,
    /// The field's stable name within its component and source, such as `scause` or
    /// `procs[1].state`.
    pub field: String,
    /// The value.
    pub value: Value,
    /// The source that read it.
    pub source: Source,
}

impl StateField {
    /// Appends the canonical encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.str(&self.component);
        e.str(&self.field);
        self.value.encode(e);
        self.source.encode(e);
    }

    /// Reads one field.
    pub fn decode(d: &mut Decoder<'_>) -> Result<StateField, DecodeError> {
        Ok(StateField {
            component: d.str()?.to_owned(),
            field: d.str()?.to_owned(),
            value: decode_value(d)?,
            source: Source::decode(d)?,
        })
    }
}

/// How one field differs (§13.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Both sides report it, with different values.
    Changed {
        /// The expected side's value.
        expected: Value,
        /// The actual side's.
        actual: Value,
    },
    /// Only one side reports it; the role is the side it is missing from.
    Missing {
        /// The side without the field.
        missing: Role,
        /// The other side's value.
        value: Value,
    },
}

/// One differing field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDifference {
    /// The component's topology path.
    pub component: String,
    /// The field.
    pub field: String,
    /// The source both sides read it with.
    pub source: Source,
    /// How it differs.
    pub change: Change,
}

impl StateDifference {
    /// Appends the canonical encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.str(&self.component);
        e.str(&self.field);
        self.source.encode(e);
        match &self.change {
            Change::Changed { expected, actual } => {
                e.u8(0);
                expected.encode(e);
                actual.encode(e);
            }
            Change::Missing { missing, value } => {
                e.u8(1);
                e.u8(role_tag(*missing));
                value.encode(e);
            }
        }
    }

    /// Reads one difference.
    pub fn decode(d: &mut Decoder<'_>) -> Result<StateDifference, DecodeError> {
        let component = d.str()?.to_owned();
        let field = d.str()?.to_owned();
        let source = Source::decode(d)?;
        let change = match d.u8()? {
            0 => Change::Changed {
                expected: decode_value(d)?,
                actual: decode_value(d)?,
            },
            1 => Change::Missing {
                missing: decode_role(d)?,
                value: decode_value(d)?,
            },
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "change",
                    tag,
                });
            }
        };
        Ok(StateDifference {
            component,
            field,
            source,
            change,
        })
    }
}

pub(crate) fn role_tag(r: Role) -> u8 {
    match r {
        Role::Expected => 0,
        Role::Actual => 1,
    }
}

pub(crate) fn decode_role(d: &mut Decoder<'_>) -> Result<Role, DecodeError> {
    match d.u8()? {
        0 => Ok(Role::Expected),
        1 => Ok(Role::Actual),
        tag => Err(DecodeError::InvalidTag { what: "role", tag }),
    }
}

/// The fields that differ between two sides' states, each list in its sources' order.
///
/// A field is identified by its component, source, and name; a name repeated within one
/// component and source is matched by occurrence. Differences are ordered by component,
/// in the order components first appear in `expected` and then in `actual`, and within a
/// component by the expected side's field order, then the actual side's fields that the
/// expected side lacks.
pub fn diff_state(expected: &[StateField], actual: &[StateField]) -> Vec<StateDifference> {
    type Key<'a> = (&'a str, &'a Source, &'a str, usize);
    fn keys(fields: &[StateField]) -> Vec<Key<'_>> {
        let mut seen: BTreeMap<(&str, &Source, &str), usize> = BTreeMap::new();
        fields
            .iter()
            .map(|f| {
                let n = seen.entry((&f.component, &f.source, &f.field)).or_default();
                *n += 1;
                (f.component.as_str(), &f.source, f.field.as_str(), *n - 1)
            })
            .collect()
    }
    let ke = keys(expected);
    let ka: BTreeMap<Key<'_>, usize> = keys(actual)
        .into_iter()
        .enumerate()
        .map(|(i, k)| (k, i))
        .collect();
    let mut used = vec![false; actual.len()];
    let mut out: Vec<StateDifference> = Vec::new();
    let difference = |f: &StateField, change| StateDifference {
        component: f.component.clone(),
        field: f.field.clone(),
        source: f.source.clone(),
        change,
    };
    for (f, k) in expected.iter().zip(&ke) {
        match ka.get(k).copied() {
            Some(i) => {
                used[i] = true;
                if actual[i].value != f.value {
                    out.push(difference(
                        f,
                        Change::Changed {
                            expected: f.value.clone(),
                            actual: actual[i].value.clone(),
                        },
                    ));
                }
            }
            None => out.push(difference(
                f,
                Change::Missing {
                    missing: Role::Actual,
                    value: f.value.clone(),
                },
            )),
        }
    }
    for (f, _) in actual.iter().zip(&used).filter(|(_, u)| !**u) {
        out.push(difference(
            f,
            Change::Missing {
                missing: Role::Expected,
                value: f.value.clone(),
            },
        ));
    }
    let mut rank: BTreeMap<&str, usize> = BTreeMap::new();
    for f in expected.iter().chain(actual) {
        let next = rank.len();
        rank.entry(&f.component).or_insert(next);
    }
    out.sort_by_key(|d| rank[d.component.as_str()]);
    out
}
