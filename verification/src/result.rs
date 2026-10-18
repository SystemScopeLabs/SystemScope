//! The verification result, its canonical encoding, and its text rendering
//! (`docs/m4-design.md` §6.7, §13.4, §14).
//!
//! The encoding is the precursor of a reproducer's `result.bin` (§16): the same result
//! always gives the same bytes, and the text rendering is a view derived from it.

use std::fmt::{self, Write as _};

use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};
use systemscope_contracts::trace::{TraceAt, Value};

use crate::compare::{Comparison, Divergence, FieldDiff, Outcome, StreamResult, Verdict};
use crate::error::EngineError;
use crate::latent::StateDivergence;
use crate::locate::Window;
use crate::observation::{Observation, ObservedValue, decode_value};
use crate::profile::{Incomparable, Role};
use crate::slice::Slice;
use crate::state::{Change, StateDifference, decode_role, role_tag};
use crate::trace::{Diagnostic, TraceDivergence, TraceLine};

/// The magic that starts an encoded result.
pub const RESULT_MAGIC: [u8; 4] = *b"SSVR";
/// The version of the encoding.
pub const RESULT_VERSION: u32 = 1;

/// What a case's verification found, with its evidence (§6.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationResult {
    /// The comparison, why the case is incomparable, or why no complete result exists.
    pub verdict: Result<Verdict, EngineError>,
    /// Both sides' observations around the primary divergence (§14.2).
    pub window: Option<Window>,
    /// State evidence (§13), when both sides read state.
    pub state: Option<StateEvidence>,
    /// Trace evidence (§14).
    pub trace: TraceEvidence,
    /// Each side's recorded predecessors of its first differing trace record (§15), for
    /// a primary divergence of same-engine sides.
    pub slices: Vec<Slice>,
}

/// The overall outcome, the engine's failures included (§20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultOutcome {
    /// The streams were compared, with this outcome.
    Compared(Outcome),
    /// The case cannot be compared.
    Incomparable,
    /// No complete result was produced.
    EngineError,
}

/// The structured state diff at the divergence, and the first state divergence of
/// same-engine sides (§12.4, §13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateEvidence {
    /// The events each side had dispatched when its state was read.
    pub read_at: [u64; 2],
    /// The differing fields there.
    pub differences: Vec<StateDifference>,
    /// The first state divergence, for same-engine sides.
    pub first: Option<StateDivergence>,
}

/// The trace windows and diagnostics (§14).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceEvidence {
    /// The window around each auxiliary stream's divergence (§14.2).
    pub auxiliary: Vec<(String, Window)>,
    /// The first differing canonical trace record, for same-engine sides (§14.1).
    pub canonical: Option<TraceDivergence>,
    /// Backend diagnostics, shown and never compared (§14.3).
    pub diagnostics: Vec<Diagnostic>,
}

impl VerificationResult {
    /// The outcome the verdict implies.
    pub fn outcome(&self) -> ResultOutcome {
        match &self.verdict {
            Ok(Verdict::Compared(c)) => ResultOutcome::Compared(c.outcome()),
            Ok(Verdict::Incomparable(_)) => ResultOutcome::Incomparable,
            Err(_) => ResultOutcome::EngineError,
        }
    }

    /// The canonical bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.raw(&RESULT_MAGIC);
        e.u32(RESULT_VERSION);
        match &self.verdict {
            Ok(Verdict::Compared(c)) => {
                e.u8(0);
                comparison(&mut e, c);
            }
            Ok(Verdict::Incomparable(i)) => {
                e.u8(1);
                incomparable(&mut e, i);
            }
            Err(err) => {
                e.u8(2);
                engine_error(&mut e, err);
            }
        }
        option(&mut e, self.window.as_ref(), window);
        option(&mut e, self.state.as_ref(), |e, s| {
            e.u64(s.read_at[0]);
            e.u64(s.read_at[1]);
            list(e, &s.differences, |e, d| d.encode(e));
            option(e, s.first.as_ref(), state_divergence);
        });
        list(&mut e, &self.trace.auxiliary, |e, (stream, w)| {
            e.str(stream);
            window(e, w);
        });
        option(&mut e, self.trace.canonical.as_ref(), trace_divergence);
        list(&mut e, &self.trace.diagnostics, |e, d| d.encode(e));
        list(&mut e, &self.slices, |e, s| s.encode(e));
        e.into_bytes()
    }

    /// Reads a result, refusing trailing bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<VerificationResult, DecodeError> {
        let mut d = Decoder::new(bytes);
        if d.array::<4>()? != RESULT_MAGIC {
            return Err(DecodeError::InvalidTag {
                what: "result magic",
                tag: bytes[0],
            });
        }
        let version = d.u32()?;
        if version != RESULT_VERSION {
            return Err(DecodeError::InvalidTag {
                what: "result version",
                tag: version.min(255) as u8,
            });
        }
        let verdict = match d.u8()? {
            0 => Ok(Verdict::Compared(read_comparison(&mut d)?)),
            1 => Ok(Verdict::Incomparable(read_incomparable(&mut d)?)),
            2 => Err(read_engine_error(&mut d)?),
            tag => {
                return Err(DecodeError::InvalidTag {
                    what: "verdict",
                    tag,
                });
            }
        };
        let window = read_option(&mut d, read_window)?;
        let state = read_option(&mut d, |d| {
            Ok(StateEvidence {
                read_at: [d.u64()?, d.u64()?],
                differences: read_list(d, StateDifference::decode)?,
                first: read_option(d, read_state_divergence)?,
            })
        })?;
        let trace = TraceEvidence {
            auxiliary: read_list(&mut d, |d| Ok((d.str()?.to_owned(), read_window(d)?)))?,
            canonical: read_option(&mut d, read_trace_divergence)?,
            diagnostics: read_list(&mut d, Diagnostic::decode)?,
        };
        let slices = read_list(&mut d, Slice::decode)?;
        d.finish()?;
        Ok(VerificationResult {
            verdict,
            window,
            state,
            trace,
            slices,
        })
    }
}

fn option<T>(e: &mut Encoder, v: Option<&T>, f: impl Fn(&mut Encoder, &T)) {
    match v {
        None => e.u8(0),
        Some(v) => {
            e.u8(1);
            f(e, v);
        }
    }
}

fn read_option<T>(
    d: &mut Decoder<'_>,
    f: impl Fn(&mut Decoder<'_>) -> Result<T, DecodeError>,
) -> Result<Option<T>, DecodeError> {
    match d.u8()? {
        0 => Ok(None),
        1 => Ok(Some(f(d)?)),
        tag => Err(DecodeError::InvalidTag {
            what: "option",
            tag,
        }),
    }
}

fn list<T>(e: &mut Encoder, items: &[T], f: impl Fn(&mut Encoder, &T)) {
    e.len(items.len());
    for i in items {
        f(e, i);
    }
}

fn read_list<T>(
    d: &mut Decoder<'_>,
    f: impl Fn(&mut Decoder<'_>) -> Result<T, DecodeError>,
) -> Result<Vec<T>, DecodeError> {
    let n = d.len()?;
    // Every item takes at least one byte: a longer count is corrupt.
    if n > d.remaining() {
        return Err(DecodeError::Truncated);
    }
    (0..n).map(|_| f(d)).collect()
}

fn observed(e: &mut Encoder, v: &ObservedValue) {
    match v {
        ObservedValue::Absent => e.u8(0),
        ObservedValue::Present(v) => {
            e.u8(1);
            v.encode(e);
        }
    }
}

fn read_observed(d: &mut Decoder<'_>) -> Result<ObservedValue, DecodeError> {
    match d.u8()? {
        0 => Ok(ObservedValue::Absent),
        1 => Ok(ObservedValue::Present(decode_value(d)?)),
        tag => Err(DecodeError::InvalidTag {
            what: "observed value",
            tag,
        }),
    }
}

fn divergence(e: &mut Encoder, d: &Divergence) {
    match d {
        Divergence::Kind { expected, actual } => {
            e.u8(0);
            e.str(expected);
            e.str(actual);
        }
        Divergence::Fields { kind, fields } => {
            e.u8(1);
            e.str(kind);
            list(e, fields, |e, f| {
                e.str(&f.name);
                observed(e, &f.expected);
                observed(e, &f.actual);
            });
        }
        Divergence::Ended { ended, kind } => {
            e.u8(2);
            e.u8(role_tag(*ended));
            e.str(kind);
        }
    }
}

fn read_divergence(d: &mut Decoder<'_>) -> Result<Divergence, DecodeError> {
    Ok(match d.u8()? {
        0 => Divergence::Kind {
            expected: d.str()?.to_owned(),
            actual: d.str()?.to_owned(),
        },
        1 => Divergence::Fields {
            kind: d.str()?.to_owned(),
            fields: read_list(d, |d| {
                Ok(FieldDiff {
                    name: d.str()?.to_owned(),
                    expected: read_observed(d)?,
                    actual: read_observed(d)?,
                })
            })?,
        },
        2 => Divergence::Ended {
            ended: decode_role(d)?,
            kind: d.str()?.to_owned(),
        },
        tag => {
            return Err(DecodeError::InvalidTag {
                what: "divergence",
                tag,
            });
        }
    })
}

fn stream_result(e: &mut Encoder, r: &StreamResult) {
    match r {
        StreamResult::Agree {
            stream,
            compared,
            final_prefix,
        } => {
            e.u8(0);
            e.str(stream);
            e.u64(*compared);
            e.raw(final_prefix);
        }
        StreamResult::Diverged {
            stream,
            ordinal,
            divergence: d,
        } => {
            e.u8(1);
            e.str(stream);
            e.u64(*ordinal);
            divergence(e, d);
        }
    }
}

fn read_stream_result(d: &mut Decoder<'_>) -> Result<StreamResult, DecodeError> {
    Ok(match d.u8()? {
        0 => StreamResult::Agree {
            stream: d.str()?.to_owned(),
            compared: d.u64()?,
            final_prefix: d.array()?,
        },
        1 => StreamResult::Diverged {
            stream: d.str()?.to_owned(),
            ordinal: d.u64()?,
            divergence: read_divergence(d)?,
        },
        tag => {
            return Err(DecodeError::InvalidTag {
                what: "stream result",
                tag,
            });
        }
    })
}

fn failures(e: &mut Encoder, f: &[(Role, EngineError)]) {
    list(e, f, |e, (role, err)| {
        e.u8(role_tag(*role));
        engine_error(e, err);
    });
}

fn read_failures(d: &mut Decoder<'_>) -> Result<Vec<(Role, EngineError)>, DecodeError> {
    read_list(d, |d| Ok((decode_role(d)?, read_engine_error(d)?)))
}

fn comparison(e: &mut Encoder, c: &Comparison) {
    stream_result(e, &c.primary);
    list(e, &c.auxiliary, stream_result);
    list(e, &c.not_compared, |e, s| e.str(s));
    failures(e, &c.failures);
}

fn read_comparison(d: &mut Decoder<'_>) -> Result<Comparison, DecodeError> {
    Ok(Comparison {
        primary: read_stream_result(d)?,
        auxiliary: read_list(d, read_stream_result)?,
        not_compared: read_list(d, |d| Ok(d.str()?.to_owned()))?,
        failures: read_failures(d)?,
    })
}

fn incomparable(e: &mut Encoder, i: &Incomparable) {
    match i {
        Incomparable::Stream { side, stream } => {
            e.u8(0);
            e.u8(role_tag(*side));
            e.str(stream);
        }
        Incomparable::Field { side, field } => {
            e.u8(1);
            e.u8(role_tag(*side));
            e.str(field);
        }
        Incomparable::AbsentRequired {
            side,
            field,
            ordinal,
        } => {
            e.u8(2);
            e.u8(role_tag(*side));
            e.str(field);
            e.u64(*ordinal);
        }
    }
}

fn read_incomparable(d: &mut Decoder<'_>) -> Result<Incomparable, DecodeError> {
    Ok(match d.u8()? {
        0 => Incomparable::Stream {
            side: decode_role(d)?,
            stream: d.str()?.to_owned(),
        },
        1 => Incomparable::Field {
            side: decode_role(d)?,
            field: d.str()?.to_owned(),
        },
        2 => Incomparable::AbsentRequired {
            side: decode_role(d)?,
            field: d.str()?.to_owned(),
            ordinal: d.u64()?,
        },
        tag => {
            return Err(DecodeError::InvalidTag {
                what: "incomparable",
                tag,
            });
        }
    })
}

fn engine_error(e: &mut Encoder, err: &EngineError) {
    let (tag, why) = match err {
        EngineError::BackendFailed(w) => (0, w),
        EngineError::Unsupported(w) => (1, w),
        EngineError::BoundaryDesync(w) => (2, w),
        EngineError::Internal(w) => (3, w),
        EngineError::CorruptReproducer(w) => (4, w),
    };
    e.u8(tag);
    e.str(why);
}

fn read_engine_error(d: &mut Decoder<'_>) -> Result<EngineError, DecodeError> {
    let tag = d.u8()?;
    let why = d.str()?.to_owned();
    Ok(match tag {
        0 => EngineError::BackendFailed(why),
        1 => EngineError::Unsupported(why),
        2 => EngineError::BoundaryDesync(why),
        3 => EngineError::Internal(why),
        4 => EngineError::CorruptReproducer(why),
        tag => {
            return Err(DecodeError::InvalidTag {
                what: "engine error",
                tag,
            });
        }
    })
}

fn window(e: &mut Encoder, w: &Window) {
    e.u64(w.first);
    list(e, &w.expected, |e, o| o.encode(e));
    list(e, &w.actual, |e, o| o.encode(e));
    failures(e, &w.failures);
}

fn read_window(d: &mut Decoder<'_>) -> Result<Window, DecodeError> {
    Ok(Window {
        first: d.u64()?,
        expected: read_list(d, Observation::decode)?,
        actual: read_list(d, Observation::decode)?,
        failures: read_failures(d)?,
    })
}

fn state_divergence(e: &mut Encoder, s: &StateDivergence) {
    e.u64(s.origin);
    option(e, s.event.as_ref(), |e, v| e.u64(*v));
    list(e, &s.differing, |e, c| e.str(c));
}

fn read_state_divergence(d: &mut Decoder<'_>) -> Result<StateDivergence, DecodeError> {
    Ok(StateDivergence {
        origin: d.u64()?,
        event: read_option(d, |d| d.u64())?,
        differing: read_list(d, |d| Ok(d.str()?.to_owned()))?,
    })
}

fn trace_divergence(e: &mut Encoder, t: &TraceDivergence) {
    e.u64(t.origin);
    option(e, t.event.as_ref(), |e, v| e.u64(*v));
    e.u64(t.record);
    e.u64(t.first);
    list(e, &t.expected, |e, l| l.encode(e));
    list(e, &t.actual, |e, l| l.encode(e));
}

fn read_trace_divergence(d: &mut Decoder<'_>) -> Result<TraceDivergence, DecodeError> {
    Ok(TraceDivergence {
        origin: d.u64()?,
        event: read_option(d, |d| d.u64())?,
        record: d.u64()?,
        first: d.u64()?,
        expected: read_list(d, TraceLine::decode)?,
        actual: read_list(d, TraceLine::decode)?,
    })
}

impl fmt::Display for VerificationResult {
    /// The text rendering: a view of the canonical result, in its order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        render(&mut out, self).expect("writing to a String");
        f.write_str(&out)
    }
}

fn value(v: &Value) -> String {
    match v {
        Value::U64(v) => format!("{v:#x}"),
        Value::I64(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Str(s) => format!("{s:?}"),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("bytes[{}] {hex}", b.len())
        }
    }
}

fn observed_text(v: &ObservedValue) -> String {
    match v {
        ObservedValue::Absent => "absent".to_owned(),
        ObservedValue::Present(v) => value(v),
    }
}

fn role(r: Role) -> &'static str {
    match r {
        Role::Expected => "expected",
        Role::Actual => "actual",
    }
}

fn observation(o: &Observation) -> String {
    let fields: Vec<String> = o
        .fields
        .iter()
        .map(|(n, v)| format!("{n}={}", observed_text(v)))
        .collect();
    format!("[{}] {} {}", o.ordinal, o.kind, fields.join(" "))
}

fn divergence_text(d: &Divergence) -> String {
    match d {
        Divergence::Kind { expected, actual } => {
            format!("kind: expected {expected}, actual {actual}")
        }
        Divergence::Fields { kind, fields } => {
            let f: Vec<String> = fields
                .iter()
                .map(|f| {
                    format!(
                        "{}: expected {}, actual {}",
                        f.name,
                        observed_text(&f.expected),
                        observed_text(&f.actual)
                    )
                })
                .collect();
            format!("{kind}: {}", f.join("; "))
        }
        Divergence::Ended { ended, kind } => {
            format!("the {} stream ended; the other has {kind}", role(*ended))
        }
    }
}

fn stream_text(r: &StreamResult) -> String {
    match r {
        StreamResult::Agree {
            stream,
            compared,
            final_prefix,
        } => format!(
            "{stream}: agree, {compared} compared, prefix {}",
            hex(final_prefix)
        ),
        StreamResult::Diverged {
            stream,
            ordinal,
            divergence,
        } => format!(
            "{stream}: diverged at {ordinal}, {}",
            divergence_text(divergence)
        ),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn window_text(out: &mut String, w: &Window) -> fmt::Result {
    for (r, side) in [(Role::Expected, &w.expected), (Role::Actual, &w.actual)] {
        writeln!(out, "    {}:", role(r))?;
        for o in side.iter() {
            writeln!(out, "      {}", observation(o))?;
        }
    }
    for (r, e) in &w.failures {
        writeln!(out, "    {} failed: {e}", role(*r))?;
    }
    Ok(())
}

fn trace_line(l: &TraceLine) -> String {
    let at = match l.at {
        TraceAt::Init => "init".to_owned(),
        TraceAt::Event(k) => format!("{}/{:?}/{}", k.tick.0, k.phase, k.sequence),
    };
    let fields: Vec<String> = l
        .fields
        .iter()
        .map(|(n, v)| format!("{n}={}", value(v)))
        .collect();
    format!("{at} {} {} {}", l.component, l.kind, fields.join(" "))
}

fn render(out: &mut String, r: &VerificationResult) -> fmt::Result {
    let outcome = match r.outcome() {
        ResultOutcome::Compared(Outcome::Agree) => "agree",
        ResultOutcome::Compared(Outcome::Diverged) => "diverged",
        ResultOutcome::Compared(Outcome::AuxiliaryDivergence) => "auxiliary divergence",
        ResultOutcome::Incomparable => "incomparable",
        ResultOutcome::EngineError => "engine error",
    };
    writeln!(out, "outcome: {outcome}")?;
    match &r.verdict {
        Ok(Verdict::Compared(c)) => {
            writeln!(out, "primary: {}", stream_text(&c.primary))?;
            for a in &c.auxiliary {
                writeln!(out, "auxiliary: {}", stream_text(a))?;
            }
            for n in &c.not_compared {
                writeln!(out, "not compared: {n}")?;
            }
            for (side, e) in &c.failures {
                writeln!(out, "failure: {}: {e}", role(*side))?;
            }
        }
        Ok(Verdict::Incomparable(i)) => writeln!(out, "incomparable: {i:?}")?,
        Err(e) => writeln!(out, "error: {e}")?,
    }
    if let Some(w) = &r.window {
        writeln!(out, "window from {}:", w.first)?;
        window_text(out, w)?;
    }
    if let Some(s) = &r.state {
        writeln!(
            out,
            "state at events {} (expected) and {} (actual):",
            s.read_at[0], s.read_at[1]
        )?;
        for d in &s.differences {
            let change = match &d.change {
                Change::Changed { expected, actual } => {
                    format!("expected {}, actual {}", value(expected), value(actual))
                }
                Change::Missing { missing, value: v } => {
                    format!("missing({}), other {}", role(*missing), value(v))
                }
            };
            writeln!(
                out,
                "  {} {} [{}]: {change}",
                d.component, d.field, d.source
            )?;
        }
        if let Some(first) = &s.first {
            match first.event {
                Some(e) => writeln!(
                    out,
                    "first state divergence (evidence): event {e} from origin {}, in {}",
                    first.origin,
                    first.differing.join(", ")
                )?,
                None => writeln!(
                    out,
                    "first state divergence (evidence): none from origin {}",
                    first.origin
                )?,
            }
        }
    }
    for (stream, w) in &r.trace.auxiliary {
        writeln!(out, "auxiliary window {stream} from {}:", w.first)?;
        window_text(out, w)?;
    }
    if let Some(t) = &r.trace.canonical {
        match t.event {
            Some(e) => writeln!(
                out,
                "first trace divergence: event {e}, record {} from origin {}",
                t.record, t.origin
            )?,
            None => writeln!(out, "first trace divergence: none from origin {}", t.origin)?,
        }
        for (r, side) in [(Role::Expected, &t.expected), (Role::Actual, &t.actual)] {
            writeln!(out, "  {}:", role(r))?;
            for (i, l) in side.iter().enumerate() {
                let mark = if t.first + i as u64 == t.record {
                    '>'
                } else {
                    ' '
                };
                writeln!(out, "   {mark}{}", trace_line(l))?;
            }
        }
    }
    for s in &r.slices {
        writeln!(
            out,
            "predecessor slice ({}), window from record {}:",
            role(s.role),
            s.first
        )?;
        for e in &s.entries {
            writeln!(out, "  [{}] {}: {}", e.record, e.link, trace_line(&e.line))?;
        }
        for u in &s.unresolved {
            writeln!(
                out,
                "  [{}] unresolved: {} bytes at {:#x}, no store in the window gives them",
                u.record, u.len, u.addr
            )?;
        }
    }
    for d in &r.trace.diagnostics {
        writeln!(out, "diagnostic ({}) {}:", role(d.role), d.label)?;
        for l in &d.lines {
            writeln!(out, "  {l}")?;
        }
    }
    Ok(())
}
