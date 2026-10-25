//! The reproducer bundle (`docs/m4-design.md` §16): a canonical directory of replay
//! material, observation evidence, and the verification result, which a replay
//! recomputes byte for byte.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io;
use std::path::Path;

use serde_json::Value as Json;
use systemscope_contracts::canonical::{DecodeError, Decoder, Encoder};

use crate::backend::{Backend, Capabilities, Capability};
use crate::case::{BackendId, BackendRunConfig, InputVariant, Scenario, execution_id};
use crate::checkpoint::Checkpoint;
use crate::compare::{Comparison, Divergence, Outcome, StreamResult, Verdict};
use crate::error::EngineError;
use crate::observation::Observation;
use crate::profile::{ComparisonProfile, Plan, Role};
use crate::report::{Builder, Make, from_origins};
use crate::result::VerificationResult;

/// The manifest's `format`.
pub const FORMAT: &str = "systemscope-reproducer";

/// The manifest's `version`: the bundle layout.
pub const BUNDLE_VERSION: u32 = 1;

/// The manifest's path in a bundle.
pub const MANIFEST: &str = "manifest.json";

/// The verification result's path in a bundle.
pub const RESULT: &str = "result.bin";

/// The magic of a `.obs` entry.
pub const OBS_MAGIC: [u8; 4] = *b"SSVO";

/// The layout of a `.obs` entry.
pub const OBS_VERSION: u32 = 1;

const ROLES: [Role; 2] = [Role::Expected, Role::Actual];

const CAPABILITIES: [(Capability, &str); 8] = [
    (Capability::ArchStream, "arch_stream"),
    (Capability::EffectStream, "effect_stream"),
    (Capability::OsStream, "os_stream"),
    (Capability::StepBoundary, "step_boundary"),
    (Capability::DeterministicReplay, "deterministic_replay"),
    (Capability::Snapshot, "snapshot"),
    (Capability::Restore, "restore"),
    (Capability::State, "state"),
];

const OUTCOMES: [(Outcome, &str); 3] = [
    (Outcome::Agree, "agree"),
    (Outcome::Diverged, "diverged"),
    (Outcome::AuxiliaryDivergence, "auxiliary_divergence"),
];

/// The comparison profile as a manifest records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileEntry {
    /// The profile name.
    pub name: String,
    /// The profile version.
    pub version: u32,
    /// The profile identity.
    pub identity: [u8; 32],
    /// The primary stream.
    pub primary: String,
    /// The auxiliary streams, in name order.
    pub auxiliary: Vec<String>,
    /// The stop rule.
    pub stop_rule: String,
    /// The report window.
    pub report_window: u32,
}

impl ProfileEntry {
    /// The entry for `profile`.
    pub fn of(profile: &ComparisonProfile) -> ProfileEntry {
        ProfileEntry {
            name: profile.name.clone(),
            version: profile.version,
            identity: profile.identity(),
            primary: profile.primary.stream.clone(),
            auxiliary: profile.auxiliary.iter().map(|s| s.stream.clone()).collect(),
            stop_rule: profile.stop_rule.clone(),
            report_window: profile.report_window,
        }
    }
}

/// One side as a manifest records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideEntry {
    /// The side's role.
    pub role: Role,
    /// Its backend identity.
    pub backend: BackendId,
    /// Its capability set.
    pub capabilities: Capabilities,
    /// Its input variant identity.
    pub input: [u8; 32],
    /// Its canonical replay configuration.
    pub config: BackendRunConfig,
    /// Its replay origin: the primary boundaries its checkpoint covers, or `None` for
    /// the initial state.
    pub origin: Option<u64>,
}

/// How the primary stream first diverged, as a manifest records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergenceKind {
    /// The observations' kinds differ.
    Kind,
    /// Fields of one kind differ.
    Fields,
    /// One side's stream ended first.
    Ended,
}

impl DivergenceKind {
    fn of(d: &Divergence) -> DivergenceKind {
        match d {
            Divergence::Kind { .. } => DivergenceKind::Kind,
            Divergence::Fields { .. } => DivergenceKind::Fields,
            Divergence::Ended { .. } => DivergenceKind::Ended,
        }
    }

    fn name(self) -> &'static str {
        match self {
            DivergenceKind::Kind => "kind",
            DivergenceKind::Fields => "fields",
            DivergenceKind::Ended => "ended",
        }
    }
}

/// A `Diverged` bundle's first primary divergence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryDivergence {
    /// The primary stream.
    pub stream: String,
    /// The ordinal of the first divergence.
    pub ordinal: u64,
    /// How it diverged.
    pub kind: DivergenceKind,
}

/// The canonical description of a bundle (§16.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The common scenario.
    pub scenario: Scenario,
    /// The comparison profile.
    pub profile: ProfileEntry,
    /// The expected side, then the actual side.
    pub sides: [SideEntry; 2],
    /// The outcome.
    pub outcome: Outcome,
    /// The first primary divergence, for `Diverged` only.
    pub primary_divergence: Option<PrimaryDivergence>,
    /// Every other entry's path and BLAKE3, in path order.
    pub entries: Vec<(String, [u8; 32])>,
}

impl Manifest {
    /// The canonical rendering: fixed key order and layout, LF line endings.
    pub fn render(&self) -> String {
        let mut o = String::new();
        self.render_into(&mut o).expect("writing to a String");
        o
    }

    fn render_into(&self, o: &mut String) -> fmt::Result {
        let s = &self.scenario;
        let p = &self.profile;
        writeln!(o, "{{")?;
        writeln!(o, "  \"format\": {},", quote(FORMAT))?;
        writeln!(o, "  \"version\": {BUNDLE_VERSION},")?;
        writeln!(o, "  \"scenario\": {{")?;
        writeln!(o, "    \"platform\": {},", quote(&s.platform))?;
        writeln!(o, "    \"topology_hash\": \"{}\",", hex(&s.topology_hash))?;
        writeln!(o, "    \"cpu_profile\": {},", quote(&s.cpu_profile))?;
        writeln!(o, "    \"program_blake3\": \"{}\",", hex(&s.program))?;
        writeln!(o, "    \"locator\": {},", quote(&s.locator))?;
        writeln!(o, "    \"seed\": {}", s.seed)?;
        writeln!(o, "  }},")?;
        writeln!(o, "  \"profile\": {{")?;
        writeln!(o, "    \"name\": {},", quote(&p.name))?;
        writeln!(o, "    \"version\": {},", p.version)?;
        writeln!(o, "    \"identity\": \"{}\",", hex(&p.identity))?;
        writeln!(o, "    \"primary\": {},", quote(&p.primary))?;
        writeln!(o, "    \"auxiliary\": {},", strings(&p.auxiliary))?;
        writeln!(o, "    \"stop_rule\": {},", quote(&p.stop_rule))?;
        writeln!(o, "    \"report_window\": {}", p.report_window)?;
        writeln!(o, "  }},")?;
        writeln!(o, "  \"sides\": [")?;
        for (i, side) in self.sides.iter().enumerate() {
            let b = &side.backend;
            let caps: Vec<&str> = side.capabilities.iter().map(|&c| capability(c)).collect();
            let origin = match side.origin {
                None => "\"initial\"".to_owned(),
                Some(covered) => format!("{{\"checkpoint\": {covered}}}"),
            };
            writeln!(o, "    {{")?;
            writeln!(o, "      \"role\": \"{}\",", role(side.role))?;
            writeln!(o, "      \"backend\": {{")?;
            writeln!(o, "        \"name\": {},", quote(&b.name))?;
            writeln!(o, "        \"version\": {},", quote(&b.version))?;
            writeln!(o, "        \"build\": {}", quote(&b.build))?;
            writeln!(o, "      }},")?;
            writeln!(o, "      \"capabilities\": {},", strings(&caps))?;
            writeln!(o, "      \"input\": \"{}\",", hex(&side.input))?;
            writeln!(
                o,
                "      \"replay_config\": {{\"budget\": {}}},",
                side.config.budget
            )?;
            writeln!(o, "      \"origin\": {origin}")?;
            writeln!(o, "    }}{}", if i == 0 { "," } else { "" })?;
        }
        writeln!(o, "  ],")?;
        writeln!(o, "  \"outcome\": \"{}\",", outcome(self.outcome))?;
        match &self.primary_divergence {
            None => writeln!(o, "  \"primary_divergence\": null,")?,
            Some(d) => {
                writeln!(o, "  \"primary_divergence\": {{")?;
                writeln!(o, "    \"stream\": {},", quote(&d.stream))?;
                writeln!(o, "    \"ordinal\": {},", d.ordinal)?;
                writeln!(o, "    \"kind\": \"{}\"", d.kind.name())?;
                writeln!(o, "  }},")?;
            }
        }
        writeln!(o, "  \"entries\": [")?;
        for (i, (path, hash)) in self.entries.iter().enumerate() {
            let comma = if i + 1 < self.entries.len() { "," } else { "" };
            writeln!(
                o,
                "    {{\"path\": {}, \"blake3\": \"{}\"}}{comma}",
                quote(path),
                hex(hash)
            )?;
        }
        writeln!(o, "  ]")?;
        writeln!(o, "}}")
    }

    /// Reads a manifest, which must be exactly its canonical rendering.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
        let json: Json = serde_json::from_slice(bytes).map_err(|e| format!("not JSON: {e}"))?;
        let manifest = Manifest::from_json(&json)?;
        if manifest.render().as_bytes() != bytes {
            return Err("not the canonical rendering".to_owned());
        }
        Ok(manifest)
    }

    fn from_json(j: &Json) -> Result<Manifest, String> {
        if text(j, "format")? != FORMAT {
            return Err("not a reproducer".to_owned());
        }
        if number(j, "version")? != u64::from(BUNDLE_VERSION) {
            return Err(format!(
                "version {} is not {BUNDLE_VERSION}",
                number(j, "version")?
            ));
        }
        let s = field(j, "scenario")?;
        let scenario = Scenario {
            platform: text(s, "platform")?.to_owned(),
            topology_hash: hash(s, "topology_hash")?,
            cpu_profile: text(s, "cpu_profile")?.to_owned(),
            program: hash(s, "program_blake3")?,
            locator: text(s, "locator")?.to_owned(),
            seed: number(s, "seed")?,
        };
        let p = field(j, "profile")?;
        let profile = ProfileEntry {
            name: text(p, "name")?.to_owned(),
            version: small(p, "version")?,
            identity: hash(p, "identity")?,
            primary: text(p, "primary")?.to_owned(),
            auxiliary: names(p, "auxiliary")?,
            stop_rule: text(p, "stop_rule")?.to_owned(),
            report_window: small(p, "report_window")?,
        };
        let sides = field(j, "sides")?
            .as_array()
            .ok_or("sides is not an array")?;
        let [e, a] = &sides[..] else {
            return Err(format!("{} sides, not 2", sides.len()));
        };
        let sides = [side_entry(e)?, side_entry(a)?];
        if sides.iter().map(|s| s.role).ne(ROLES) {
            return Err("the sides are not expected, then actual".to_owned());
        }
        let outcome = lookup(&OUTCOMES, text(j, "outcome")?, "outcome")?;
        let primary_divergence = match field(j, "primary_divergence")? {
            Json::Null => None,
            d => Some(PrimaryDivergence {
                stream: text(d, "stream")?.to_owned(),
                ordinal: number(d, "ordinal")?,
                kind: lookup(
                    &[
                        (DivergenceKind::Kind, "kind"),
                        (DivergenceKind::Fields, "fields"),
                        (DivergenceKind::Ended, "ended"),
                    ],
                    text(d, "kind")?,
                    "divergence kind",
                )?,
            }),
        };
        let entries = field(j, "entries")?
            .as_array()
            .ok_or("entries is not an array")?
            .iter()
            .map(|e| Ok((text(e, "path")?.to_owned(), hash(e, "blake3")?)))
            .collect::<Result<_, String>>()?;
        Ok(Manifest {
            scenario,
            profile,
            sides,
            outcome,
            primary_divergence,
            entries,
        })
    }
}

fn side_entry(j: &Json) -> Result<SideEntry, String> {
    let b = field(j, "backend")?;
    let role = lookup(
        &[(Role::Expected, "expected"), (Role::Actual, "actual")],
        text(j, "role")?,
        "role",
    )?;
    let capabilities = names(j, "capabilities")?
        .iter()
        .map(|c| lookup(&CAPABILITIES, c, "capability"))
        .collect::<Result<_, String>>()?;
    // Any other string re-renders as "initial", which the canonical check refuses.
    let origin = match field(j, "origin")? {
        Json::String(_) => None,
        o => Some(number(o, "checkpoint")?),
    };
    Ok(SideEntry {
        role,
        backend: BackendId {
            name: text(b, "name")?.to_owned(),
            version: text(b, "version")?.to_owned(),
            build: text(b, "build")?.to_owned(),
        },
        capabilities,
        input: hash(j, "input")?,
        config: BackendRunConfig {
            budget: number(field(j, "replay_config")?, "budget")?,
        },
        origin,
    })
}

fn field<'a>(j: &'a Json, key: &str) -> Result<&'a Json, String> {
    j.get(key).ok_or_else(|| format!("no {key}"))
}

fn text<'a>(j: &'a Json, key: &str) -> Result<&'a str, String> {
    field(j, key)?
        .as_str()
        .ok_or_else(|| format!("{key} is not a string"))
}

fn number(j: &Json, key: &str) -> Result<u64, String> {
    field(j, key)?
        .as_u64()
        .ok_or_else(|| format!("{key} is not an unsigned integer"))
}

fn small(j: &Json, key: &str) -> Result<u32, String> {
    u32::try_from(number(j, key)?).map_err(|_| format!("{key} is too large"))
}

fn names(j: &Json, key: &str) -> Result<Vec<String>, String> {
    field(j, key)?
        .as_array()
        .ok_or_else(|| format!("{key} is not an array"))?
        .iter()
        .map(|n| {
            n.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} holds a non-string"))
        })
        .collect()
}

fn hash(j: &Json, key: &str) -> Result<[u8; 32], String> {
    let s = text(j, key)?;
    let bad = || format!("{key} is not 64 hex digits");
    if s.len() != 64 {
        return Err(bad());
    }
    // Uppercase or signed digits parse here, and the canonical check refuses them.
    let mut out = [0; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = s
            .get(2 * i..2 * i + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .ok_or_else(bad)?;
    }
    Ok(out)
}

fn lookup<T: Copy>(table: &[(T, &str)], name: &str, what: &str) -> Result<T, String> {
    table
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(t, _)| *t)
        .ok_or_else(|| format!("unknown {what} {name:?}"))
}

fn name<T: PartialEq>(table: &[(T, &'static str)], t: &T) -> &'static str {
    table
        .iter()
        .find(|(x, _)| x == t)
        .map(|(_, n)| *n)
        .expect("every value is named")
}

fn capability(c: Capability) -> &'static str {
    name(&CAPABILITIES, &c)
}

fn outcome(o: Outcome) -> &'static str {
    name(&OUTCOMES, &o)
}

fn role(r: Role) -> &'static str {
    match r {
        Role::Expected => "expected",
        Role::Actual => "actual",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if u32::from(c) < 0x20 => o.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn strings<S: AsRef<str>>(items: &[S]) -> String {
    let items: Vec<String> = items.iter().map(|s| quote(s.as_ref())).collect();
    format!("[{}]", items.join(", "))
}

/// One side's primary observation evidence: a `.obs` entry (§16.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationEvidence {
    /// The ordinal of the first observation.
    pub first: u64,
    /// The observations, from `first`.
    pub observations: Vec<Observation>,
}

impl ObservationEvidence {
    /// The canonical bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.raw(&OBS_MAGIC);
        e.u32(OBS_VERSION);
        e.u64(self.first);
        e.len(self.observations.len());
        for o in &self.observations {
            o.encode(&mut e);
        }
        e.into_bytes()
    }

    /// Reads [`ObservationEvidence::to_bytes`]'s output, refusing trailing bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<ObservationEvidence, DecodeError> {
        let mut d = Decoder::new(bytes);
        if d.array::<4>()? != OBS_MAGIC {
            return Err(DecodeError::InvalidTag {
                what: "observation evidence magic",
                tag: 0,
            });
        }
        let version = d.u32()?;
        if version != OBS_VERSION {
            return Err(DecodeError::InvalidTag {
                what: "observation evidence version",
                tag: version.min(255) as u8,
            });
        }
        let first = d.u64()?;
        let observations = (0..d.len()?)
            .map(|_| Observation::decode(&mut d))
            .collect::<Result<_, _>>()?;
        d.finish()?;
        Ok(ObservationEvidence {
            first,
            observations,
        })
    }
}

/// A reproducer: every entry by its bundle path, `/`-separated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bundle {
    /// The entries, the manifest among them.
    pub entries: BTreeMap<String, Vec<u8>>,
}

fn checkpoint_path(r: Role) -> String {
    format!("{}/checkpoint.bin", role(r))
}

fn input_path(r: Role) -> String {
    format!("{}/input.bin", role(r))
}

fn obs_path(r: Role) -> String {
    format!("{}.obs", role(r))
}

fn perturbation_path(r: Role) -> String {
    format!("{}/perturbation.bin", role(r))
}

/// The perturbation a reproducer's `role` side was built with (`docs/m5-design.md` §18),
/// or `None` for an unperturbed side. A replay builds that side with it.
pub fn perturbation(bundle: &Bundle, r: Role) -> Option<&[u8]> {
    bundle.entries.get(&perturbation_path(r)).map(Vec::as_slice)
}

/// Verifies `inputs[1]` against `inputs[0]` from `origins` as [`from_origins`] does, and
/// bundles the material and the result (§16). Only an `Agree`, `Diverged`, or
/// `AuxiliaryDivergence` result is a reproducer; an engine failure is returned, and an
/// incomparable case is an error.
pub fn write(
    plan: &Plan,
    scenario: &Scenario,
    sides: [Builder<'_>; 2],
    inputs: [InputVariant; 2],
    origins: [Option<&Checkpoint>; 2],
) -> Result<Bundle, EngineError> {
    bundled(plan, scenario, sides, inputs, origins).map(|(b, _)| b)
}

pub(crate) fn bundled(
    plan: &Plan,
    scenario: &Scenario,
    sides: [Builder<'_>; 2],
    inputs: [InputVariant; 2],
    origins: [Option<&Checkpoint>; 2],
) -> Result<(Bundle, VerificationResult), EngineError> {
    let result = from_origins(plan, scenario, sides, inputs.clone(), origins);
    let comparison = match &result.verdict {
        Ok(Verdict::Compared(c)) => c,
        Ok(Verdict::Incomparable(why)) => {
            return Err(EngineError::Internal(format!(
                "an incomparable case has no reproducer: {why:?}"
            )));
        }
        Err(e) => return Err(e.clone()),
    };
    let mut entries = BTreeMap::new();
    let mut entries_of = Vec::new();
    for i in 0..2 {
        let b = sides[i].make.build();
        if let Some(c) = origins[i] {
            entries.insert(checkpoint_path(ROLES[i]), c.to_bytes());
        }
        if inputs[i] != InputVariant::None {
            entries.insert(input_path(ROLES[i]), inputs[i].to_bytes());
        }
        if let Some(p) = sides[i].perturbation {
            entries.insert(perturbation_path(ROLES[i]), p.to_vec());
        }
        entries_of.push(SideEntry {
            role: ROLES[i],
            backend: b.describe(),
            capabilities: b.capabilities(),
            input: inputs[i].identity(),
            config: sides[i].config,
            origin: origins[i]
                .and_then(|c| c.mark(&plan.profile.primary.stream).map(|m| m.covered)),
        });
    }
    let primary_divergence = primary_of(comparison);
    if primary_divergence.is_some() {
        let w = result
            .window
            .as_ref()
            .ok_or_else(|| EngineError::Internal("a divergence without a window".to_owned()))?;
        for (r, observations) in ROLES.into_iter().zip([&w.expected, &w.actual]) {
            let obs = ObservationEvidence {
                first: w.first,
                observations: observations.clone(),
            };
            entries.insert(obs_path(r), obs.to_bytes());
        }
    }
    entries.insert(RESULT.to_owned(), result.to_bytes());
    let manifest = Manifest {
        scenario: scenario.clone(),
        profile: ProfileEntry::of(&plan.profile),
        sides: entries_of.try_into().expect("two sides"),
        outcome: comparison.outcome(),
        primary_divergence,
        entries: entries
            .iter()
            .map(|(p, b): (&String, &Vec<u8>)| (p.clone(), *blake3::hash(b).as_bytes()))
            .collect(),
    };
    entries.insert(MANIFEST.to_owned(), manifest.render().into_bytes());
    Ok((Bundle { entries }, result))
}

/// The manifest's record of a comparison's first primary divergence.
fn primary_of(c: &Comparison) -> Option<PrimaryDivergence> {
    match &c.primary {
        StreamResult::Diverged {
            stream,
            ordinal,
            divergence,
        } => Some(PrimaryDivergence {
            stream: stream.clone(),
            ordinal: *ordinal,
            kind: DivergenceKind::of(divergence),
        }),
        _ => None,
    }
}

/// Why a bundle did not replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// The bundle failed validation, and nothing ran: always
    /// [`EngineError::CorruptReproducer`].
    Corrupt(EngineError),
    /// The replay ran and recomputed different bytes for this entry.
    Differs(String),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::Corrupt(e) => write!(f, "{e}"),
            ReplayError::Differs(path) => write!(f, "the replay recomputed a different {path}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The case a bundle must record: the caller's plan, scenario, and each side's backend and
/// canonical replay configuration.
#[derive(Clone, Copy)]
pub struct Case<'a> {
    /// The plan.
    pub plan: &'a Plan,
    /// The common scenario.
    pub scenario: &'a Scenario,
    /// The expected and the actual side.
    pub sides: [Builder<'a>; 2],
}

/// The replay material of a valid bundle.
struct Material {
    inputs: [InputVariant; 2],
    checkpoints: [Option<Checkpoint>; 2],
}

/// Validates `bundle` against `case` (§16.2): every entry matches its manifest BLAKE3, the
/// entries are exactly those the manifest's outcome and sides call for, the scenario,
/// profile, replay configuration, and backends are the case's, each checkpoint belongs
/// to its side's `ExecutionId`, restores, and is the one the side's own run takes, and
/// the result is the manifest's. Nothing is compared yet. Returns the manifest.
pub fn validate(bundle: &Bundle, case: Case<'_>) -> Result<Manifest, EngineError> {
    material(bundle, case).map(|(m, _)| m)
}

fn material(bundle: &Bundle, case: Case<'_>) -> Result<(Manifest, Material), EngineError> {
    let Case {
        plan,
        scenario,
        sides,
    } = case;
    let corrupt = |why: String| EngineError::CorruptReproducer(why);
    let get = |path: &str| {
        bundle
            .entries
            .get(path)
            .ok_or_else(|| corrupt(format!("no {path}")))
    };
    let m = Manifest::parse(get(MANIFEST)?).map_err(|why| corrupt(format!("{MANIFEST}: {why}")))?;
    let listed: Vec<&str> = m.entries.iter().map(|(p, _)| p.as_str()).collect();
    let held: Vec<&str> = bundle
        .entries
        .keys()
        .map(String::as_str)
        .filter(|p| *p != MANIFEST)
        .collect();
    if listed != held {
        return Err(corrupt(format!(
            "the manifest lists {listed:?}, the bundle holds {held:?}"
        )));
    }
    for (path, h) in &m.entries {
        if blake3::hash(get(path)?).as_bytes() != h {
            return Err(corrupt(format!("{path} does not match its BLAKE3")));
        }
    }
    let primary = &plan.profile.primary.stream;
    let mut wanted = vec![RESULT.to_owned()];
    for (s, side) in m.sides.iter().zip(&sides) {
        if side.perturbation.is_some() {
            wanted.push(perturbation_path(s.role));
        }
        if s.origin.is_some() {
            wanted.push(checkpoint_path(s.role));
        }
        if s.input != InputVariant::None.identity() {
            wanted.push(input_path(s.role));
        }
        if m.outcome == Outcome::Diverged {
            wanted.push(obs_path(s.role));
        }
    }
    wanted.sort();
    if listed != wanted {
        return Err(corrupt(format!(
            "the entries are {listed:?}, not {wanted:?}"
        )));
    }
    if m.scenario != *scenario {
        return Err(corrupt("the scenario is not the case's".to_owned()));
    }
    if m.profile != ProfileEntry::of(&plan.profile) {
        return Err(corrupt("the profile is not the plan's".to_owned()));
    }
    if (m.outcome == Outcome::Diverged) != m.primary_divergence.is_some() {
        return Err(corrupt(
            "primary_divergence does not match the outcome".to_owned(),
        ));
    }
    let mut inputs = [InputVariant::None, InputVariant::None];
    let mut checkpoints = [None, None];
    for (i, s) in m.sides.iter().enumerate() {
        let r = role(s.role);
        let config = sides[i].config;
        let b = sides[i].make.build();
        if s.backend != b.describe() || s.capabilities != b.capabilities() {
            return Err(corrupt(format!("the {r} backend is not the one recorded")));
        }
        if s.config != config {
            return Err(corrupt(format!(
                "the {r} replay configuration is not the case's"
            )));
        }
        if perturbation(bundle, s.role) != sides[i].perturbation {
            return Err(corrupt(format!("the {r} perturbation is not the case's")));
        }
        if let Some(bytes) = bundle.entries.get(&input_path(s.role)) {
            inputs[i] = InputVariant::from_bytes(bytes)
                .map_err(|e| corrupt(format!("{}: {e}", input_path(s.role))))?;
        }
        if inputs[i].identity() != s.input {
            return Err(corrupt(format!("the {r} input is not the one recorded")));
        }
        let Some(covered) = s.origin else {
            continue;
        };
        let path = checkpoint_path(s.role);
        // Only a SystemScope runtime restores, and re-derives, a checkpoint.
        let Make::Runtime(make) = sides[i].make else {
            return Err(corrupt(format!("the {r} backend cannot restore {path}")));
        };
        let c = Checkpoint::from_bytes(get(&path)?).map_err(|e| corrupt(format!("{path}: {e}")))?;
        if c.execution != execution_id(scenario, &inputs[i], &s.backend) {
            return Err(corrupt(format!("{path} belongs to another execution")));
        }
        if c.mark(primary).map(|m| m.covered) != Some(covered) {
            return Err(corrupt(format!(
                "{path} does not cover {covered} boundaries"
            )));
        }
        let mut b = make();
        b.start(scenario, &inputs[i], &config, &plan.profile)
            .and_then(|()| b.restore(&c))
            .map_err(|e| corrupt(format!("{path}: {e}")))?;
        // The side's own run takes the same checkpoint after as many events, marks and
        // all: a mark the result never reads is checked too.
        let mut b = make();
        b.start(scenario, &inputs[i], &config, &plan.profile)
            .and_then(|()| {
                while b.events() < Some(c.event_index) {
                    b.step_event()?;
                }
                b.checkpoint()
            })
            .ok()
            .filter(|again| *again == c)
            .ok_or_else(|| corrupt(format!("{path} is not the side's own checkpoint")))?;
        checkpoints[i] = Some(c);
    }
    let result = VerificationResult::from_bytes(get(RESULT)?)
        .map_err(|e| corrupt(format!("{RESULT}: {e}")))?;
    let found = match &result.verdict {
        Ok(Verdict::Compared(c)) => c,
        _ => return Err(corrupt(format!("{RESULT} is not a reproducible outcome"))),
    };
    if found.outcome() != m.outcome || primary_of(found) != m.primary_divergence {
        return Err(corrupt(format!("{RESULT} is not the manifest's outcome")));
    }
    for s in &m.sides {
        if let Some(bytes) = bundle.entries.get(&obs_path(s.role)) {
            ObservationEvidence::from_bytes(bytes)
                .map_err(|e| corrupt(format!("{}: {e}", obs_path(s.role))))?;
        }
    }
    let material = Material {
        inputs,
        checkpoints,
    };
    Ok((m, material))
}

/// Validates `bundle` as [`validate`] does, then replays it: each side from its
/// checkpoint or its initial state, under the case. The recomputed bundle, its result
/// and observation evidence among it, must equal `bundle` byte for byte. Returns the
/// recomputed result.
pub fn replay(bundle: &Bundle, case: Case<'_>) -> Result<VerificationResult, ReplayError> {
    let (_, m) = material(bundle, case).map_err(ReplayError::Corrupt)?;
    let origins = [m.checkpoints[0].as_ref(), m.checkpoints[1].as_ref()];
    let Case {
        plan,
        scenario,
        sides,
    } = case;
    let (again, result) = bundled(plan, scenario, sides, m.inputs, origins)
        .map_err(|_| ReplayError::Differs(RESULT.to_owned()))?;
    // The result first: the manifest and the evidence follow from it.
    let order = std::iter::once(RESULT).chain(again.entries.keys().map(String::as_str));
    for path in order.chain(bundle.entries.keys().map(String::as_str)) {
        if again.entries.get(path) != bundle.entries.get(path) {
            return Err(ReplayError::Differs(path.to_owned()));
        }
    }
    Ok(result)
}

/// Whether `path` names a file inside a bundle directory: `/`-separated, relative, with
/// no empty, `.`, or `..` component, and nothing a platform reads as a root or a drive.
fn is_inside(path: &str) -> bool {
    !path.contains(['\\', ':']) && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// Writes `bundle` into a new directory `dir`. Every entry path must stay inside `dir`;
/// otherwise nothing is written.
pub fn write_dir(bundle: &Bundle, dir: &Path) -> io::Result<()> {
    if let Some(path) = bundle.entries.keys().find(|p| !is_inside(p)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path:?} is not a path inside the bundle"),
        ));
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(dir)?;
    for (path, bytes) in &bundle.entries {
        let file = dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, bytes)?;
    }
    Ok(())
}

/// Reads every file under `dir` into a bundle, by its `/`-separated path. A symbolic
/// link is refused rather than followed.
pub fn read_dir(dir: &Path) -> io::Result<Bundle> {
    let mut bundle = Bundle::default();
    let mut pending = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&at)? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|n| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{n:?} is not UTF-8"))
            })?;
            let path = format!("{prefix}{name}");
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{path} is a symbolic link"),
                ));
            }
            if kind.is_dir() {
                pending.push((entry.path(), format!("{path}/")));
            } else {
                bundle.entries.insert(path, std::fs::read(entry.path())?);
            }
        }
    }
    Ok(bundle)
}
