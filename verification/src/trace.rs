//! Trace evidence (`docs/m4-design.md` §14): the canonical execution trace around the
//! first differing record of two same-engine sides, the observation windows around each
//! auxiliary stream's divergence, and backend diagnostics shown but never compared.

use std::collections::VecDeque;

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::event::EventKey;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};

use crate::case::Scenario;
use crate::compare::{Comparison, Recorded, StreamResult};
use crate::error::EngineError;
use crate::latent::{SameSide, resume, start};
use crate::locate::Window;
use crate::observation::decode_value;
use crate::profile::{Plan, Role};
use crate::slice::{SLICE_RECORDS, Slice, slice};
use crate::state::{decode_role, role_tag};

/// One canonical trace record, with its component named by topology path (§14.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceLine {
    /// The event it was emitted in, or `init`.
    pub at: TraceAt,
    /// Whether a component or the runtime emitted it.
    pub origin: TraceOrigin,
    /// The emitting component's path; for a runtime record, the event's target.
    pub component: String,
    /// The record kind.
    pub kind: String,
    /// The fields, in emission order.
    pub fields: Vec<(String, Value)>,
}

impl TraceLine {
    pub(crate) fn of(r: &TraceRecord, paths: &[String]) -> TraceLine {
        TraceLine {
            at: r.at,
            origin: r.origin,
            component: paths
                .get(r.component.0 as usize)
                .map_or_else(|| format!("#{}", r.component.0), Clone::clone),
            kind: r.kind.to_owned(),
            fields: r
                .fields
                .iter()
                .map(|(n, v)| ((*n).to_owned(), v.clone()))
                .collect(),
        }
    }

    /// Appends the canonical encoding.
    pub fn encode(&self, e: &mut Encoder) {
        match self.at {
            TraceAt::Init => e.u8(0),
            TraceAt::Event(key) => {
                e.u8(1);
                key.encode(e);
            }
        }
        e.u8(match self.origin {
            TraceOrigin::Component => 0,
            TraceOrigin::Runtime => 1,
        });
        e.str(&self.component);
        e.str(&self.kind);
        e.len(self.fields.len());
        for (name, value) in &self.fields {
            e.str(name);
            value.encode(e);
        }
    }

    /// Reads one line.
    pub fn decode(d: &mut Decoder<'_>) -> Result<TraceLine, DecodeError> {
        let at = match d.u8()? {
            0 => TraceAt::Init,
            1 => TraceAt::Event(EventKey::decode(d)?),
            tag => return Err(DecodeError::InvalidTag { what: "at", tag }),
        };
        let origin = match d.u8()? {
            0 => TraceOrigin::Component,
            1 => TraceOrigin::Runtime,
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "origin",
                    tag,
                });
            }
        };
        let component = d.str()?.to_owned();
        let kind = d.str()?.to_owned();
        let n = d.len()?;
        let mut fields = Vec::new();
        for _ in 0..n {
            fields.push((d.str()?.to_owned(), decode_value(d)?));
        }
        Ok(TraceLine {
            at,
            origin,
            component,
            kind,
            fields,
        })
    }
}

/// Where two same-engine sides' canonical traces first differ (§14.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceDivergence {
    /// The events both sides had dispatched where the scan began, as for the first state
    /// divergence.
    pub origin: u64,
    /// The events dispatched once the event that emitted the first differing record was
    /// done, or `None` if the traces agree through the end of the scan.
    pub event: Option<u64>,
    /// The records both sides emitted after `origin` before the first differing one.
    pub record: u64,
    /// The index after `origin` of each window's first record: `record` minus the
    /// window, or 0.
    pub first: u64,
    /// The expected side's records from `first`, up to the window after the first
    /// differing one, or fewer where the scan ended.
    pub expected: Vec<TraceLine>,
    /// The actual side's, likewise.
    pub actual: Vec<TraceLine>,
}

/// Compares two same-engine sides' canonical trace records one by one, from the latest
/// checkpoint pair with equal states at most `until` events, or from the initial states,
/// up to `until` events, and returns the first record that differs with `window` records
/// around it on each side (§14.1). Records emitted during `init` are not compared.
pub fn first_trace_divergence(
    plan: &Plan,
    scenario: &Scenario,
    expected: SameSide<'_>,
    actual: SameSide<'_>,
    window: u64,
    until: u64,
) -> Result<TraceDivergence, EngineError> {
    Ok(scan(plan, scenario, [expected, actual], window, 0, until)?.0)
}

/// The first trace divergence, as [`first_trace_divergence`] finds it, and each side's
/// relevant-predecessor slice of its first differing record (§15), built from the up to
/// [`SLICE_RECORDS`] records before it. A side whose records end before the differing
/// index has no slice.
pub fn trace_and_slices(
    plan: &Plan,
    scenario: &Scenario,
    expected: SameSide<'_>,
    actual: SameSide<'_>,
    window: u64,
    until: u64,
) -> Result<(TraceDivergence, Vec<Slice>), EngineError> {
    scan(
        plan,
        scenario,
        [expected, actual],
        window,
        SLICE_RECORDS,
        until,
    )
}

fn scan(
    plan: &Plan,
    scenario: &Scenario,
    mut sides: [SameSide<'_>; 2],
    window: u64,
    keep: usize,
    until: u64,
) -> Result<(TraceDivergence, Vec<Slice>), EngineError> {
    let paths = start(plan, scenario, &mut sides)?;
    let origin = resume(&mut sides, until, |b| Ok(b.to_vec()))?;
    let [e, a] = sides.map(|s| s.backend);
    e.capture()?;
    a.capture()?;
    let w = usize::try_from(window).unwrap_or(usize::MAX);
    let ring = w.max(keep);
    let mut before: [VecDeque<TraceRecord>; 2] = Default::default();
    let mut record = 0u64;
    loop {
        let at = e.events().expect("started");
        if at >= until {
            let none = TraceDivergence {
                origin,
                event: None,
                record,
                first: record,
                expected: Vec::new(),
                actual: Vec::new(),
            };
            return Ok((none, Vec::new()));
        }
        e.step_event()?;
        a.step_event()?;
        let (re, ra) = (e.captured(), a.captured());
        let differs = re.iter().zip(&ra).position(|(x, y)| x != y);
        let Some(j) = differs.or((re.len() != ra.len()).then(|| re.len().min(ra.len()))) else {
            record += re.len() as u64;
            for (b, r) in before.iter_mut().zip([re, ra]) {
                b.extend(r);
                while b.len() > ring {
                    b.pop_front();
                }
            }
            continue;
        };
        // Each side's dense window, ending at its first differing record.
        let mut slices = Vec::new();
        if keep > 0 {
            for ((role, b), r) in [Role::Expected, Role::Actual]
                .into_iter()
                .zip(&before)
                .zip([&re, &ra])
            {
                let Some(anchor) = r.get(j) else {
                    continue;
                };
                let from = b.len().saturating_sub(keep);
                let mut dense: Vec<TraceRecord> = b.iter().skip(from).cloned().collect();
                dense.extend(r[..j].iter().cloned());
                dense.push(anchor.clone());
                let first = record + (from as u64) - (b.len() as u64);
                slices.push(slice(&dense, first, &paths, role));
            }
        }
        // The records before the first differing one are equal on both sides.
        let mut lines = [re, ra].map(|r| {
            let mut kept: Vec<TraceRecord> = before[0].iter().cloned().collect();
            kept.extend(r);
            kept
        });
        let kept_before = (before[0].len() + j).min(w);
        let skip = before[0].len() + j - kept_before;
        let want = skip + kept_before + 1 + w;
        let mut more = [&mut *e, &mut *a];
        for (side, kept) in more.iter_mut().zip(lines.iter_mut()) {
            while kept.len() < want {
                if side.step_event().is_err() {
                    break;
                }
                kept.extend(side.captured());
            }
            kept.truncate(want);
            kept.drain(..skip);
        }
        let record = record + j as u64;
        let [le, la] = lines.map(|k| k.iter().map(|r| TraceLine::of(r, &paths)).collect());
        let found = TraceDivergence {
            origin,
            event: Some(at + 1),
            record,
            first: record - kept_before as u64,
            expected: le,
            actual: la,
        };
        return Ok((found, slices));
    }
}

/// The observation window around each auxiliary stream's divergence, in that stream's
/// own ordinals (§14.2): `report_window` observations before and after it on each side,
/// or fewer where a stream ends.
pub fn auxiliary_windows(
    plan: &Plan,
    comparison: &Comparison,
    expected: &Recorded,
    actual: &Recorded,
) -> Vec<(String, Window)> {
    let w = u64::from(plan.profile.report_window);
    let mut out = Vec::new();
    for (i, r) in comparison.auxiliary.iter().enumerate() {
        let StreamResult::Diverged {
            stream, ordinal, ..
        } = r
        else {
            continue;
        };
        let first = ordinal.saturating_sub(w);
        let slice = |s: Option<&Vec<_>>| -> Vec<_> {
            let s = s.map_or(&[][..], Vec::as_slice);
            let end = ordinal.saturating_add(w + 1).min(s.len() as u64) as usize;
            s.get(first as usize..end)
                .map(<[_]>::to_vec)
                .unwrap_or_default()
        };
        out.push((
            stream.clone(),
            Window {
                first,
                expected: slice(expected.get(i + 1)),
                actual: slice(actual.get(i + 1)),
                failures: Vec::new(),
            },
        ));
    }
    out
}

/// A backend's own diagnostic output, attached as opaque evidence: shown, never
/// compared (§14.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The side it came from.
    pub role: Role,
    /// What it is, such as `spike log`.
    pub label: String,
    /// Its lines, as the backend wrote them.
    pub lines: Vec<String>,
}

impl Diagnostic {
    /// Appends the canonical encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.u8(role_tag(self.role));
        e.str(&self.label);
        e.len(self.lines.len());
        for l in &self.lines {
            e.str(l);
        }
    }

    /// Reads one diagnostic.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Diagnostic, DecodeError> {
        let role = decode_role(d)?;
        let label = d.str()?.to_owned();
        let n = d.len()?;
        let mut lines = Vec::new();
        for _ in 0..n {
            lines.push(d.str()?.to_owned());
        }
        Ok(Diagnostic { role, label, lines })
    }
}
