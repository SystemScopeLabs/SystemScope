//! Comparison profiles and the field-support check made before either side runs
//! (`docs/m4-design.md` §6.5, §10).

use std::collections::BTreeSet;

use systemscope_contracts::canonical::Encoder;

/// Whether a profile needs a field from every side (§10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// A side that cannot report it makes the case incomparable.
    Required,
    /// A side that cannot report it drops it from the case's comparison.
    Optional,
}

/// One compared field of a kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSpec {
    /// The stable field name.
    pub name: String,
    /// Whether every side must report it.
    pub requirement: Requirement,
    /// Whether `Absent` is a legal value, as for `rd_value` with no register write.
    /// Where it is not, an `Absent` makes the case incomparable.
    pub may_be_absent: bool,
}

/// The compared fields of one boundary kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindSpec {
    /// The boundary kind.
    pub kind: String,
    /// The compared fields, in observation order.
    pub fields: Vec<FieldSpec>,
    /// Fields the profile never reads, in name order.
    pub excluded: Vec<String>,
}

/// One compared stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    /// The stream name.
    pub stream: String,
    /// The compared kinds, in name order.
    pub kinds: Vec<KindSpec>,
    /// Where comparison starts, for example `elf-entry`; empty for the first boundary.
    pub window: String,
}

impl StreamSpec {
    /// The spec of `kind`, if the stream compares it.
    pub fn kind(&self, kind: &str) -> Option<&KindSpec> {
        self.kinds.iter().find(|k| k.kind == kind)
    }
}

/// A comparison profile (§6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComparisonProfile {
    /// The profile name.
    pub name: String,
    /// The profile version; a changed rule is a new version.
    pub version: u32,
    /// The stream that decides the first divergence.
    pub primary: StreamSpec,
    /// The other compared streams, in name order.
    pub auxiliary: Vec<StreamSpec>,
    /// The semantic end of the comparison, which every adapter applies.
    pub stop_rule: String,
    /// The observations kept before and after a divergence for diagnostics.
    pub report_window: u32,
}

impl ComparisonProfile {
    /// The canonical encoding the identity is taken over.
    pub fn encode(&self, e: &mut Encoder) {
        e.str(&self.name);
        e.u32(self.version);
        encode_stream(e, &self.primary);
        e.len(self.auxiliary.len());
        for s in &self.auxiliary {
            encode_stream(e, s);
        }
        e.str(&self.stop_rule);
        e.u32(self.report_window);
    }

    /// The BLAKE3 of the canonical encoding.
    pub fn identity(&self) -> [u8; 32] {
        let mut e = Encoder::new();
        self.encode(&mut e);
        *blake3::hash(e.as_bytes()).as_bytes()
    }

    /// Every compared stream, the primary first.
    pub fn streams(&self) -> impl Iterator<Item = &StreamSpec> {
        std::iter::once(&self.primary).chain(&self.auxiliary)
    }
}

fn encode_stream(e: &mut Encoder, s: &StreamSpec) {
    e.str(&s.stream);
    e.len(s.kinds.len());
    for k in &s.kinds {
        e.str(&k.kind);
        e.len(k.fields.len());
        for f in &k.fields {
            e.str(&f.name);
            e.bool(f.requirement == Requirement::Required);
            e.bool(f.may_be_absent);
        }
        e.len(k.excluded.len());
        for x in &k.excluded {
            e.str(x);
        }
    }
    e.str(&s.window);
}

/// What a side can report: its streams and each one's `stream/kind/field` names (§7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Support {
    /// The streams the side reports.
    pub streams: BTreeSet<String>,
    /// The fields the side reports, as `stream/kind/field`.
    pub fields: BTreeSet<String>,
}

impl Support {
    fn has(&self, stream: &str, kind: &str, field: &str) -> bool {
        self.fields.contains(&format!("{stream}/{kind}/{field}"))
    }
}

/// Which side a statement is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// The expected side.
    Expected,
    /// The actual side.
    Actual,
}

/// Why a case cannot be compared (§20).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incomparable {
    /// A side cannot report a compared stream.
    Stream {
        /// The side.
        side: Role,
        /// The stream.
        stream: String,
    },
    /// A side cannot report a required field.
    Field {
        /// The side.
        side: Role,
        /// The field, as `stream/kind/field`.
        field: String,
    },
    /// A side reported `Absent` where the profile requires a value.
    AbsentRequired {
        /// The side.
        side: Role,
        /// The field, as `stream/kind/field`.
        field: String,
        /// The ordinal of the observation.
        ordinal: u64,
    },
}

/// A profile narrowed to what both sides can report: the plan of one case's comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The profile with every unsupported optional field removed.
    pub profile: ComparisonProfile,
    /// The optional fields dropped, as `stream/kind/field`, in profile order.
    pub not_compared: Vec<String>,
}

/// Checks both sides' support against `profile` before either side runs (§10).
pub fn plan(
    profile: &ComparisonProfile,
    expected: &Support,
    actual: &Support,
) -> Result<Plan, Incomparable> {
    let sides = [(Role::Expected, expected), (Role::Actual, actual)];
    let mut narrowed = profile.clone();
    let mut not_compared = Vec::new();
    let streams = std::iter::once(&mut narrowed.primary).chain(&mut narrowed.auxiliary);
    for s in streams {
        for (side, support) in sides {
            if !support.streams.contains(&s.stream) {
                return Err(Incomparable::Stream {
                    side,
                    stream: s.stream.clone(),
                });
            }
        }
        for k in &mut s.kinds {
            let mut kept = Vec::with_capacity(k.fields.len());
            for f in k.fields.drain(..) {
                let id = format!("{}/{}/{}", s.stream, k.kind, f.name);
                let missing = sides
                    .iter()
                    .find(|(_, sup)| !sup.has(&s.stream, &k.kind, &f.name));
                match (missing, f.requirement) {
                    (None, _) => kept.push(f),
                    (Some(&(side, _)), Requirement::Required) => {
                        return Err(Incomparable::Field { side, field: id });
                    }
                    (Some(_), Requirement::Optional) => not_compared.push(id),
                }
            }
            k.fields = kept;
        }
    }
    Ok(Plan {
        profile: narrowed,
        not_compared,
    })
}
