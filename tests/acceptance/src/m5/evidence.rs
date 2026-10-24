//! Schedule evidence on `m5-reference` (`docs/m5-design.md` §7.4, §15): the first
//! differing scheduling decision of two runs, the perturbation elements that applied up
//! to it, the shared-page words a process read since its last dispatch, and a state
//! decoder for the M4 engine's state reads.
//!
//! Nothing here adds to a trace. A jitter draw is a pure function of the seed, the
//! directed link, and the message's ordinal on it, and a link delivers in the order it
//! sent, so the `n`-th delivery on a directed link is its message `n`. The jitter of
//! every delivered message is therefore recomputed from the dispatch records with the
//! runtime's own [`Jitter::draw`].

use std::collections::BTreeMap;

use systemscope_contracts::canonical::Decoder;
use systemscope_contracts::component::{ComponentId, PortId};
use systemscope_contracts::trace::{DISPATCH_KIND, TraceHeader, TraceOrigin, TraceRecord, Value};
use systemscope_os::procop::SWITCH_KIND;
use systemscope_runtime::jitter::Jitter;
use systemscope_runtime::snapshot::component_states;
use systemscope_runtime::trace::Trace;
use systemscope_rv32::m5ref::{self, FINAL_WORDS, SHARED};
use systemscope_rv32::perturb::{Latencies, Perturbation};
use systemscope_rv32::{RAM_BASE, m5};
use systemscope_rv32i::cpu::COMMIT_KIND;
use systemscope_verify::runtime::{Projected, Projection};
use systemscope_verify::state::{Source, StateField};

use super::Fixture;
use super::campaign::Subject;
use super::verify::{M5Projection, SCHEDULE, defective_config, outcome_profile};
use systemscope_verify::case::InputVariant;

/// A page's bytes.
const PAGE: u64 = 4096;

/// One jittered delivery: event `event` delivered message `ordinal` sent on `link` from
/// end `dir`, `cycles` later than the link's latency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The event, counted from 1, that delivered it.
    pub event: u64,
    /// The link's declaration index.
    pub link: u32,
    /// 0 if sent from the link's `a` end, 1 if from its `b` end.
    pub dir: u8,
    /// The message's ordinal on the directed link.
    pub ordinal: u64,
    /// The extra cycles, at least 1.
    pub cycles: u64,
}

/// The directed link that delivers to `(component, port)`: its index and the end it was
/// sent from.
fn directed(header: &TraceHeader, component: u32, port: u64) -> Option<(u32, u8)> {
    header.links.iter().enumerate().find_map(|(i, l)| {
        let at = |(c, p): (ComponentId, PortId)| c.0 == component && u64::from(p.0) == port;
        let i = u32::try_from(i).expect("fewer than 2^32 links");
        if at(l.b) {
            Some((i, 0))
        } else if at(l.a) {
            Some((i, 1))
        } else {
            None
        }
    })
}

/// Every dispatch record's event index, counted from 1, with the record.
fn dispatches(records: &[TraceRecord]) -> impl Iterator<Item = (u64, &TraceRecord)> {
    records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Runtime && r.kind == DISPATCH_KIND)
        .zip(1..)
        .map(|(r, k)| (k, r))
}

/// The messages delivered on each directed link, keyed by link and direction.
pub type Delivered = BTreeMap<(u32, u8), u64>;

/// The messages delivered on each directed link by the end of event `upto`, and the
/// jittered ones among them, recomputed with `jitter` (§7.2).
pub fn applied_jitter(
    trace: &Trace,
    jitter: &Jitter,
    upto: u64,
) -> Result<(Delivered, Vec<Applied>), String> {
    let mut delivered = Delivered::new();
    let mut applied = Vec::new();
    for (event, r) in dispatches(&trace.records) {
        if event > upto {
            break;
        }
        let Some((_, Value::U64(port))) = r.fields.iter().find(|(n, _)| *n == "port") else {
            continue;
        };
        let (link, dir) = directed(&trace.header, r.component.0, *port)
            .ok_or_else(|| format!("event {event}: no link delivers to port {port}"))?;
        let ordinal = delivered.entry((link, dir)).or_default();
        if jitter.links.contains(link) {
            let cycles = jitter.draw(link, dir, *ordinal);
            if cycles > 0 {
                applied.push(Applied {
                    event,
                    link,
                    dir,
                    ordinal: *ordinal,
                    cycles,
                });
            }
        }
        *ordinal += 1;
    }
    Ok((delivered, applied))
}

/// One scheduling decision: the event, counted from 1, that made it, and its boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The event.
    pub event: u64,
    /// The `schedule` boundary.
    pub boundary: Projected,
}

/// Every `schedule` boundary of `trace`, read by the `outcome` profile's projection.
pub fn decisions(trace: &Trace) -> Result<Vec<Decision>, String> {
    let mut p = M5Projection::default();
    p.begin(&outcome_profile())?;
    let mut event = 0;
    let mut out = Vec::new();
    let mut found = Vec::new();
    for r in &trace.records {
        if r.origin == TraceOrigin::Runtime && r.kind == DISPATCH_KIND {
            event += 1;
        }
        p.project(r, &mut found)
            .map_err(|why| format!("event {event}: {why}"))?;
        out.extend(
            found
                .drain(..)
                .filter(|b| b.stream == SCHEDULE)
                .map(|boundary| Decision { event, boundary }),
        );
    }
    Ok(out)
}

/// What one side of a schedule divergence holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideEvidence {
    /// The identity of the side's perturbation.
    pub identity: [u8; 32],
    /// Its latency overrides.
    pub latencies: Latencies,
    /// The decisions around the first differing one, at most `window` on each side.
    pub around: Vec<Decision>,
    /// The event of its first differing decision, if it made one.
    pub event: Option<u64>,
    /// The jittered deliveries up to that event, or to the end.
    pub jitter: Vec<Applied>,
}

/// The first differing scheduling decision of two runs (§15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleEvidence {
    /// The ordinal of the first decision that differs.
    pub ordinal: usize,
    /// The expected side, then the actual one.
    pub sides: [SideEvidence; 2],
}

/// The first decision whose kind or fields differ, or where one run ends first.
pub fn first_difference(a: &[Decision], b: &[Decision]) -> Option<usize> {
    let differs = a.iter().zip(b).position(|(x, y)| x.boundary != y.boundary);
    differs.or((a.len() != b.len()).then(|| a.len().min(b.len())))
}

/// The schedule evidence of two traced runs under their perturbations, or `None` when
/// they make the same decisions.
pub fn schedule_evidence(
    sides: [(&Perturbation, &Trace); 2],
    window: usize,
) -> Result<Option<ScheduleEvidence>, String> {
    let made = [decisions(sides[0].1)?, decisions(sides[1].1)?];
    let Some(ordinal) = first_difference(&made[0], &made[1]) else {
        return Ok(None);
    };
    let side = |(p, trace): (&Perturbation, &Trace), made: &[Decision]| {
        let event = made.get(ordinal).map(|d| d.event);
        let jitter = match &p.jitter {
            Some(j) => applied_jitter(trace, j, event.unwrap_or(u64::MAX))?.1,
            None => Vec::new(),
        };
        let from = ordinal.saturating_sub(window);
        let to = (ordinal + window + 1).min(made.len());
        Ok::<_, String>(SideEvidence {
            identity: p.identity(),
            latencies: p.latencies.clone(),
            around: made.get(from..to).unwrap_or_default().to_vec(),
            event,
            jitter,
        })
    };
    Ok(Some(ScheduleEvidence {
        ordinal,
        sides: [side(sides[0], &made[0])?, side(sides[1], &made[1])?],
    }))
}

/// A load from the shared page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedRead {
    /// The event, counted from 1, that retired the load.
    pub event: u64,
    /// The virtual address.
    pub va: u64,
    /// The physical address, in the shared page.
    pub paddr: u64,
    /// The value loaded.
    pub value: u64,
}

/// The shared-page words process `pid` loaded during its last dispatch up to event
/// `upto`: from the last switch to it until `upto` or the switch away from it (§15).
pub fn shared_reads(trace: &Trace, pid: u64, upto: u64) -> Result<Vec<SharedRead>, String> {
    let field = |r: &TraceRecord, name: &str| {
        r.fields
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
    };
    let u = |r: &TraceRecord, name: &str| match field(r, name) {
        Some(Value::U64(v)) => Some(v),
        _ => None,
    };
    let shared = u64::from(SHARED)..u64::from(SHARED) + PAGE;
    let mut event = 0;
    let mut reads: Option<Vec<SharedRead>> = None;
    let mut running = false;
    for r in &trace.records {
        if r.origin == TraceOrigin::Runtime && r.kind == DISPATCH_KIND {
            event += 1;
            if event > upto {
                break;
            }
        }
        match r.kind {
            k if k == SWITCH_KIND => {
                if u(r, "to") == Some(pid) {
                    reads = Some(Vec::new());
                    running = true;
                } else if u(r, "from") == Some(pid) {
                    running = false;
                }
            }
            k if k == COMMIT_KIND => {
                let load = running && field(r, "addr").is_some() && field(r, "width").is_none();
                let (Some(list), true) = (reads.as_mut(), load) else {
                    continue;
                };
                let Some(paddr) = u(r, "paddr").filter(|a| shared.contains(a)) else {
                    continue;
                };
                list.push(SharedRead {
                    event,
                    va: u(r, "addr").ok_or("a load without a U64 addr")?,
                    paddr,
                    value: u(r, "rd_value").unwrap_or(0),
                });
            }
            _ => {}
        }
    }
    reads.ok_or_else(|| format!("pid {pid} is never dispatched by event {upto}"))
}

/// Reads `m5-reference` state fields from snapshot bytes (§15): the shared page's
/// [`FINAL_WORDS`] words and the BLAKE3 of every RAM page, labeled derived, read from
/// RAM's snapshot (`platform.ram` schema 1) without restoring the platform.
pub fn decode_state(bytes: &[u8]) -> Result<Vec<StateField>, String> {
    let ram_id = m5ref::PATHS
        .iter()
        .position(|p| *p == "soc.ram")
        .expect("m5-reference has RAM") as u32;
    let states = component_states(bytes).map_err(|e| format!("snapshot: {e:?}"))?;
    let (_, schema, state) = states
        .into_iter()
        .find(|(id, _, _)| *id == ram_id)
        .ok_or("the snapshot holds no RAM")?;
    if schema != 1 {
        return Err(format!("RAM schema {schema}"));
    }
    let err = |e| format!("RAM: {e:?}");
    let mut d = Decoder::new(state);
    d.u64().map_err(err)?;
    d.raw(32).map_err(err)?;
    match d.u8().map_err(err)? {
        0 => d.raw(16).map(drop).map_err(err)?,
        1 => d.raw(12).map(drop).map_err(err)?,
        tag => return Err(format!("RAM latency tag {tag}")),
    }
    let mut pages = BTreeMap::new();
    for _ in 0..d.len().map_err(err)? {
        let index = d.u32().map_err(err)?;
        let page = d.bytes().map_err(err)?;
        if page.len() as u64 != PAGE {
            return Err(format!("RAM page {index} holds {} bytes", page.len()));
        }
        pages.insert(index, page);
    }
    d.finish().map_err(err)?;
    let component = "soc.ram".to_owned();
    let shared = u32::try_from((u64::from(SHARED) - u64::from(RAM_BASE)) / PAGE).expect("small");
    let mut out: Vec<StateField> = (0..FINAL_WORDS as usize)
        .map(|i| {
            let word = pages.get(&shared).map_or(0, |p| {
                u32::from_le_bytes(p[4 * i..4 * i + 4].try_into().expect("four bytes"))
            });
            StateField {
                component: component.clone(),
                field: format!("shared[{i}]"),
                value: Value::U64(word.into()),
                source: Source::Decoder("m5-ram/1".to_owned()),
            }
        })
        .collect();
    out.extend(pages.iter().map(|(index, page)| StateField {
        component: component.clone(),
        field: format!(
            "page[{:#010x}]",
            u64::from(RAM_BASE) + u64::from(*index) * PAGE
        ),
        value: Value::Bytes(blake3::hash(page).as_bytes().to_vec()),
        source: Source::Derived("page blake3".to_owned()),
    }));
    Ok(out)
}

/// Runs the workload traced on `m5-reference` under `perturbation`: its trace and its
/// final snapshot.
pub fn traced(fixture: &Fixture, perturbation: &Perturbation) -> Result<(Trace, Vec<u8>), String> {
    traced_subject(fixture, Subject(None), perturbation)
}

/// [`traced`] for `subject`: a planted race's media patch is applied to the disk and its
/// kernel defect planted.
pub fn traced_subject(
    fixture: &Fixture,
    subject: Subject,
    perturbation: &Perturbation,
) -> Result<(Trace, Vec<u8>), String> {
    let mut disk = fixture.disk.clone();
    if let Some(race) = subject.0
        && let InputVariant::Media(p) = race.variant(&fixture.disk)?
    {
        p.apply(&mut disk)?;
    }
    let defect = subject.0.and_then(|r| r.defect());
    let run = m5::run_with(
        &fixture.firmware,
        &disk,
        &defective_config(perturbation, defect),
        systemscope_rv32::runner::Start::Init { traced: true },
        Vec::new(),
    );
    let snapshot = run.snapshot.ok_or("the run faulted")?;
    let trace = run.finished.trace.ok_or("the run kept no trace")?;
    Ok((trace, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use systemscope_verify::observation::ObservedValue;

    fn decision(event: u64, to: u64) -> Decision {
        Decision {
            event,
            boundary: Projected {
                stream: SCHEDULE.to_owned(),
                kind: "switch".to_owned(),
                fields: vec![("to".to_owned(), ObservedValue::Present(Value::U64(to)))],
            },
        }
    }

    #[test]
    fn the_first_difference_is_by_boundary_not_by_event() {
        let a = [decision(1, 1), decision(5, 2), decision(9, 3)];
        // Later events, same decisions: no difference.
        let later = [decision(2, 1), decision(6, 2), decision(10, 3)];
        assert_eq!(first_difference(&a, &later), None);
        assert_eq!(
            first_difference(&a, &[decision(1, 1), decision(5, 3)]),
            Some(1)
        );
        // One run makes fewer decisions: the first one it lacks.
        assert_eq!(first_difference(&a, &a[..2]), Some(2));
        assert_eq!(first_difference(&a[..0], &a), Some(0));
    }
}
