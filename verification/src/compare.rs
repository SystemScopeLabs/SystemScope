//! The comparator: first divergence on the primary stream, auxiliary streams as evidence
//! (`docs/m4-design.md` §6.4, §6.7, §10).

use crate::error::EngineError;
use crate::observation::{Observation, ObservedValue, PrefixDigest};
use crate::profile::{Incomparable, Plan, Role, StreamSpec};

/// One field that differs at a divergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDiff {
    /// The field name.
    pub name: String,
    /// The expected side's value.
    pub expected: ObservedValue,
    /// The actual side's value.
    pub actual: ObservedValue,
}

/// How two streams differ at their first divergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// The boundary kinds differ.
    Kind {
        /// The expected side's kind.
        expected: String,
        /// The actual side's kind.
        actual: String,
    },
    /// The kinds agree and these compared fields differ, in profile order.
    Fields {
        /// The common kind.
        kind: String,
        /// The differing fields.
        fields: Vec<FieldDiff>,
    },
    /// One stream ends first; `ended` names the side whose stream ended.
    Ended {
        /// The side with no observation at this ordinal.
        ended: Role,
        /// The other side's kind at this ordinal.
        kind: String,
    },
}

/// One stream's result (§6.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamResult {
    /// Every compared boundary agrees up to the stop rule.
    Agree {
        /// The stream.
        stream: String,
        /// The number of observations compared.
        compared: u64,
        /// The prefix digest `P(compared)`.
        final_prefix: [u8; 32],
    },
    /// The first divergence of the stream.
    Diverged {
        /// The stream.
        stream: String,
        /// The first differing ordinal.
        ordinal: u64,
        /// How the observations differ there.
        divergence: Divergence,
    },
}

impl StreamResult {
    /// Whether the stream agrees.
    pub fn agrees(&self) -> bool {
        matches!(self, StreamResult::Agree { .. })
    }
}

/// The outcome of a comparison (§20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every compared stream agrees.
    Agree,
    /// The primary stream diverged.
    Diverged,
    /// The primary stream agrees and an auxiliary stream diverged.
    AuxiliaryDivergence,
}

/// The result of comparing both sides' recorded streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The primary stream's result, which decides the first divergence.
    pub primary: StreamResult,
    /// Each auxiliary stream's result, in profile order; evidence only.
    pub auxiliary: Vec<StreamResult>,
    /// The optional fields neither compared nor read, from the plan.
    pub not_compared: Vec<String>,
    /// The sides that failed before the stop rule, after the primary divergence they
    /// both recorded; the auxiliary streams are then not compared.
    pub failures: Vec<(Role, EngineError)>,
}

impl Comparison {
    /// The outcome the stream results imply.
    pub fn outcome(&self) -> Outcome {
        if !self.primary.agrees() {
            Outcome::Diverged
        } else if self.auxiliary.iter().all(StreamResult::agrees) {
            Outcome::Agree
        } else {
            Outcome::AuxiliaryDivergence
        }
    }
}

/// What a comparison concluded, when no engine error stopped it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The streams were compared.
    Compared(Comparison),
    /// A side reported something the profile cannot compare.
    Incomparable(Incomparable),
}

/// One side's recorded streams, in the profile's stream order.
pub type Recorded = Vec<Vec<Observation>>;

/// Compares both sides' complete streams under `plan`. `expected` and `actual` hold one
/// stream per profile stream, the primary first.
pub fn compare(
    plan: &Plan,
    expected: &Recorded,
    actual: &Recorded,
) -> Result<Verdict, EngineError> {
    let specs: Vec<&StreamSpec> = plan.profile.streams().collect();
    if expected.len() != specs.len() || actual.len() != specs.len() {
        return Err(EngineError::Internal(
            "recorded streams do not match the profile's streams".to_owned(),
        ));
    }
    let mut results = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        match compare_stream(spec, &expected[i], &actual[i])? {
            Ok(r) => results.push(r),
            Err(why) => return Ok(Verdict::Incomparable(why)),
        }
    }
    let primary = results.remove(0);
    Ok(Verdict::Compared(Comparison {
        primary,
        auxiliary: results,
        not_compared: plan.not_compared.clone(),
        failures: Vec::new(),
    }))
}

/// One side's recording: what it observed, and why it stopped before the stop rule, if it
/// did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    /// The observations, one stream per profile stream, the primary first.
    pub streams: Recorded,
    /// The failure that ended the side early.
    pub failure: Option<EngineError>,
}

impl Recording {
    /// The streams of a side that reached the stop rule, or its failure.
    pub fn complete(self) -> Result<Recorded, EngineError> {
        match self.failure {
            None => Ok(self.streams),
            Some(e) => Err(e),
        }
    }
}

/// Compares two recordings (§20). When both sides reached the stop rule, this is
/// [`compare`]. When a side failed first, only the primary observations both sides
/// recorded are compared: a divergence among them is still the first divergence, never
/// hidden by the failure, and is reported with the failures as evidence. Without one, the
/// first failure is the result, never a divergence: a stream cut short by a failure makes
/// no length divergence.
pub fn compare_recordings(
    plan: &Plan,
    expected: &Recording,
    actual: &Recording,
) -> Result<Verdict, EngineError> {
    let failures: Vec<(Role, EngineError)> = [(Role::Expected, expected), (Role::Actual, actual)]
        .into_iter()
        .filter_map(|(role, r)| r.failure.clone().map(|e| (role, e)))
        .collect();
    let Some((_, first)) = failures.first().cloned() else {
        return compare(plan, &expected.streams, &actual.streams);
    };
    fn primary(r: &Recording) -> &[Observation] {
        r.streams.first().map_or(&[], Vec::as_slice)
    }
    let (e, a) = (primary(expected), primary(actual));
    let common = e.len().min(a.len());
    match compare_stream(&plan.profile.primary, &e[..common], &a[..common])? {
        Err(why) => Ok(Verdict::Incomparable(why)),
        Ok(primary @ StreamResult::Diverged { .. }) => Ok(Verdict::Compared(Comparison {
            primary,
            auxiliary: Vec::new(),
            not_compared: plan.not_compared.clone(),
            failures,
        })),
        Ok(StreamResult::Agree { .. }) => Err(first),
    }
}

/// Compares one stream. An observation that breaks the projection's contract with the
/// profile is an engine error; an `Absent` the profile forbids is incomparable.
fn compare_stream(
    spec: &StreamSpec,
    expected: &[Observation],
    actual: &[Observation],
) -> Result<Result<StreamResult, Incomparable>, EngineError> {
    let mut prefix = PrefixDigest::new();
    for ordinal in 0..expected.len().max(actual.len()) {
        let e = expected.get(ordinal);
        let a = actual.get(ordinal);
        for (side, o) in [(Role::Expected, e), (Role::Actual, a)] {
            if let Some(o) = o
                && let Err(why) = check(spec, ordinal as u64, side, o)?
            {
                return Ok(Err(why));
            }
        }
        let divergence = match (e, a) {
            (Some(e), Some(a)) => match diff(e, a) {
                Some(d) => d,
                None => {
                    // Equal observations extend the common prefix.
                    prefix.absorb(e);
                    continue;
                }
            },
            (Some(e), None) => Divergence::Ended {
                ended: Role::Actual,
                kind: e.kind.clone(),
            },
            (None, Some(a)) => Divergence::Ended {
                ended: Role::Expected,
                kind: a.kind.clone(),
            },
            (None, None) => unreachable!("the loop stops at the longer stream"),
        };
        return Ok(Ok(StreamResult::Diverged {
            stream: spec.stream.clone(),
            ordinal: ordinal as u64,
            divergence,
        }));
    }
    Ok(Ok(StreamResult::Agree {
        stream: spec.stream.clone(),
        compared: prefix.count(),
        final_prefix: prefix.digest(),
    }))
}

/// Checks that `o` is the observation `spec` expects at `ordinal`: the stream, the
/// ordinal, a compared kind, and exactly the compared fields in profile order.
fn check(
    spec: &StreamSpec,
    ordinal: u64,
    side: Role,
    o: &Observation,
) -> Result<Result<(), Incomparable>, EngineError> {
    let broken = |what: &str| {
        Err(EngineError::Internal(format!(
            "{side:?} {}[{ordinal}]: {what}",
            spec.stream
        )))
    };
    if o.stream != spec.stream || o.ordinal != ordinal {
        return broken("observation out of place");
    }
    let Some(kind) = spec.kind(&o.kind) else {
        return broken("kind not in the profile");
    };
    let names = o.fields.iter().map(|(n, _)| n);
    if !names.eq(kind.fields.iter().map(|f| &f.name)) {
        return broken("fields do not match the profile");
    }
    for (f, (_, v)) in kind.fields.iter().zip(&o.fields) {
        if !f.may_be_absent && *v == ObservedValue::Absent {
            return Ok(Err(Incomparable::AbsentRequired {
                side,
                field: format!("{}/{}/{}", spec.stream, kind.kind, f.name),
                ordinal,
            }));
        }
    }
    Ok(Ok(()))
}

/// The divergence between two checked observations at one ordinal, if any.
fn diff(e: &Observation, a: &Observation) -> Option<Divergence> {
    if e.kind != a.kind {
        return Some(Divergence::Kind {
            expected: e.kind.clone(),
            actual: a.kind.clone(),
        });
    }
    let fields: Vec<FieldDiff> = e
        .fields
        .iter()
        .zip(&a.fields)
        .filter(|((_, ev), (_, av))| ev != av)
        .map(|((name, ev), (_, av))| FieldDiff {
            name: name.clone(),
            expected: ev.clone(),
            actual: av.clone(),
        })
        .collect();
    (!fields.is_empty()).then(|| Divergence::Fields {
        kind: e.kind.clone(),
        fields,
    })
}

/// The naive oracle (§25.1): the first index at which two complete streams differ, as
/// whole observations, or where one ends first. Used to check the comparator and the
/// locator, never by them.
pub fn naive_first_divergence(expected: &[Observation], actual: &[Observation]) -> Option<u64> {
    let common = expected.len().min(actual.len());
    (0..common)
        .find(|&i| expected[i] != actual[i])
        .or((expected.len() != actual.len()).then_some(common))
        .map(|i| i as u64)
}
