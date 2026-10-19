//! Campaigns on `m5-reference` (`docs/m5-design.md` §14): each seed derives a
//! perturbation for one mechanism, and the subject, the unpatched scenario or a planted
//! race, runs under it and is compared with its unperturbed run under `m5.outcome`.

use systemscope_runtime::jitter::{Jitter, LinkSet};
use systemscope_runtime::ties::Ties;
use systemscope_rv32::m5ref::SLICE;
use systemscope_rv32::perturb::{Latencies, Perturbation};
use systemscope_verify::backend::{Backend, Side, run};
use systemscope_verify::case::InputVariant;
use systemscope_verify::compare::{Recording, StreamResult, Verdict, compare_recordings};
use systemscope_verify::profile::{Plan, plan};

use super::Fixture;
use super::races::Race;
use super::verify::{self, JITTER_LINKS, outcome_profile};

/// A perturbation mechanism a campaign samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mechanism {
    /// A: the latency of every link in [`JITTER_LINKS`], drawn from
    /// `1..=`[`A_MAX_CYCLES`], and the kernel's slice, drawn from [`A_SLICES`].
    A,
    /// B: jitter on [`JITTER_LINKS`], its maximum drawn from [`B_MAX_CYCLES`].
    B,
    /// C: the tie order, its seed drawn from the stream.
    C,
}

/// The largest link latency A draws.
pub const A_MAX_CYCLES: u64 = 16;
/// The slices A draws from: shorter ones would spend the event budget on preemptions.
pub const A_SLICES: std::ops::RangeInclusive<u32> = SLICE / 2..=SLICE * 5 / 4;
/// The jitter maxima B draws from.
pub const B_MAX_CYCLES: [u64; 4] = [2, 4, 8, 16];

/// The `i`-th 64-bit word of the stream for `seed` under `mechanism`.
fn draw(mechanism: Mechanism, seed: u64, i: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"systemscope m5 campaign");
    h.update(&[mechanism as u8]);
    h.update(&seed.to_le_bytes());
    h.update(&i.to_le_bytes());
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8 bytes"))
}

/// The perturbation that `seed` derives for `mechanism`.
pub fn perturbation(mechanism: Mechanism, seed: u64) -> Perturbation {
    match mechanism {
        Mechanism::A => Perturbation {
            latencies: Latencies {
                links: JITTER_LINKS
                    .iter()
                    .map(|&i| (i, 1 + draw(mechanism, seed, u64::from(i)) % A_MAX_CYCLES))
                    .collect(),
                ..Latencies::default()
            },
            slice: Some({
                let span = u64::from(A_SLICES.end() - A_SLICES.start()) + 1;
                A_SLICES.start() + (draw(mechanism, seed, u64::from(u32::MAX)) % span) as u32
            }),
            ..Perturbation::default()
        },
        Mechanism::B => Perturbation {
            jitter: Some(Jitter {
                seed: draw(mechanism, seed, 0),
                max_cycles: B_MAX_CYCLES
                    [(draw(mechanism, seed, 1) % B_MAX_CYCLES.len() as u64) as usize],
                links: LinkSet::of(&JITTER_LINKS),
            }),
            ..Perturbation::default()
        },
        Mechanism::C => Perturbation {
            ties: Some(Ties {
                seed: draw(mechanism, seed, 0),
            }),
            ..Perturbation::default()
        },
    }
}

/// What a campaign runs: the unpatched scenario, or a planted race.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Subject(pub Option<Race>);

impl Subject {
    /// The name of the scenario or race.
    pub fn name(self) -> &'static str {
        self.0.map_or("unpatched", Race::name)
    }
}

/// The plan both sides of every comparison share.
pub fn the_plan(fixture: &Fixture) -> Plan {
    let b = verify::backend(fixture, &Perturbation::default());
    plan(&outcome_profile(), &b.support(), &b.support()).expect("m5.outcome plans")
}

/// Records `subject` under `perturbation` up to the stop rule or a failure.
pub fn record(fixture: &Fixture, subject: Subject, perturbation: &Perturbation) -> Recording {
    let (input, defect) = match subject.0 {
        None => (InputVariant::None, None),
        Some(race) => (
            race.variant(&fixture.disk)
                .unwrap_or_else(|e| panic!("{}: {e}", race.name())),
            race.defect(),
        ),
    };
    let mut b = verify::defective_backend(fixture, perturbation, defect);
    let side = Side {
        backend: &mut b,
        input,
        config: verify::config(),
    };
    run(&the_plan(fixture), &verify::scenario(fixture), side)
}

/// How a perturbed run compares with the unperturbed one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judged {
    /// `outcome` agrees; the schedule may differ.
    Agree,
    /// `outcome` diverged at this ordinal: a finding.
    Diverged(u64),
    /// A side failed before any divergence, with the failure: a finding too, since the
    /// unperturbed run of every subject is judged complete.
    Failed(String),
}

/// Judges the perturbed recording against the unperturbed one.
pub fn judge(plan: &Plan, unperturbed: &Recording, perturbed: &Recording) -> Judged {
    match compare_recordings(plan, unperturbed, perturbed) {
        Ok(Verdict::Compared(c)) => match &c.primary {
            StreamResult::Agree { .. } if c.failures.is_empty() => Judged::Agree,
            StreamResult::Agree { .. } => Judged::Failed(format!("{:?}", c.failures)),
            StreamResult::Diverged { ordinal, .. } => Judged::Diverged(*ordinal),
        },
        Ok(Verdict::Incomparable(why)) => Judged::Failed(format!("incomparable: {why:?}")),
        Err(e) => Judged::Failed(format!("{e}")),
    }
}
