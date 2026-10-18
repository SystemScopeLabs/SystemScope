//! The SystemScope runtime adapter: steps a runtime until a projection of its trace
//! records yields the next observation (`docs/m4-design.md` §8.4, §9.2).

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use systemscope_contracts::observe::Observer;
use systemscope_contracts::trace::TraceRecord;
use systemscope_runtime::runtime::Runtime;

use crate::backend::{Backend, Capabilities, Capability};
use crate::case::{BackendId, BackendRunConfig, InputVariant, Scenario, StatePatch};
use crate::error::EngineError;
use crate::observation::{Observation, ObservedValue};
use crate::profile::{ComparisonProfile, Support};

/// A boundary a projection found, before the adapter places it in its stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projected {
    /// The stream.
    pub stream: String,
    /// The boundary kind.
    pub kind: String,
    /// Every field the projection reports for the kind, in any order.
    pub fields: Vec<(String, ObservedValue)>,
}

/// Maps trace records to boundaries: a pure function of the records, which never
/// re-executes (§8.4).
pub trait Projection {
    /// The streams and fields it reports.
    fn support(&self) -> Support;
    /// Accepts the profile's windows and stop rule, or refuses ones it cannot apply.
    fn begin(&mut self, profile: &ComparisonProfile) -> Result<(), String>;
    /// Reads one record, appending the boundaries it completes. A record the projection
    /// cannot read is an error.
    fn project(&mut self, record: &TraceRecord, out: &mut Vec<Projected>) -> Result<(), String>;
    /// Whether the stop rule has been reached.
    fn finished(&self) -> bool;
}

/// Builds the runtime of a scenario, elaborated and not yet initialized.
pub type Build = Box<dyn Fn(&Scenario) -> Result<Runtime, String>>;
/// Makes a fresh projection.
pub type MakeProjection = Box<dyn Fn() -> Box<dyn Projection>>;
/// Applies a state patch to runtime snapshot bytes, checking the old value.
pub type Patch = Box<dyn Fn(&mut [u8], &StatePatch) -> Result<(), String>>;

/// What the observer shares with the adapter.
struct Shared {
    projection: Box<dyn Projection>,
    profile: ComparisonProfile,
    /// The events dispatched once the current one is done; 0 during `init`.
    event_index: u64,
    /// Per compared stream: the observations not yet taken, and the event index that
    /// emitted each observation so far.
    queues: BTreeMap<String, VecDeque<Observation>>,
    events: BTreeMap<String, Vec<u64>>,
    error: Option<EngineError>,
}

impl Shared {
    fn take(&mut self, record: &TraceRecord) {
        if self.error.is_some() {
            return;
        }
        let mut found = Vec::new();
        if let Err(why) = self.projection.project(record, &mut found) {
            self.error = Some(EngineError::BackendFailed(format!(
                "event {}: {why}",
                self.event_index
            )));
            return;
        }
        for p in found {
            if let Err(e) = self.place(p) {
                self.error = Some(e);
                return;
            }
        }
    }

    /// Keeps a boundary of a compared stream and kind, with exactly the profile's fields
    /// in its order, at the next ordinal of its stream.
    fn place(&mut self, p: Projected) -> Result<(), EngineError> {
        let Some(spec) = self.profile.streams().find(|s| s.stream == p.stream) else {
            return Ok(());
        };
        let Some(kind) = spec.kind(&p.kind) else {
            return Ok(());
        };
        let mut fields = Vec::with_capacity(kind.fields.len());
        for f in &kind.fields {
            let Some((_, v)) = p.fields.iter().find(|(n, _)| *n == f.name) else {
                return Err(EngineError::Internal(format!(
                    "the projection reported no {}/{}/{}",
                    p.stream, p.kind, f.name
                )));
            };
            fields.push((f.name.clone(), v.clone()));
        }
        let events = self.events.entry(p.stream.clone()).or_default();
        let ordinal = events.len() as u64;
        events.push(self.event_index);
        self.queues
            .entry(p.stream.clone())
            .or_default()
            .push_back(Observation {
                stream: p.stream,
                ordinal,
                kind: p.kind,
                fields,
            });
        Ok(())
    }
}

/// Hands every record to the shared projection.
struct Tap(Rc<RefCell<Shared>>);

impl Observer for Tap {
    fn on_trace(&mut self, record: &TraceRecord) {
        self.0.borrow_mut().take(record);
    }
}

/// A running side.
struct Session {
    runtime: Runtime,
    shared: Rc<RefCell<Shared>>,
    scenario: Scenario,
    budget: u64,
    events: u64,
    patch: Option<StatePatch>,
}

/// A [`Backend`] over any SystemScope runtime and projection.
pub struct RuntimeBackend {
    id: BackendId,
    build: Build,
    projection: MakeProjection,
    patch: Option<Patch>,
    session: Option<Session>,
}

impl RuntimeBackend {
    /// An adapter that builds runtimes with `build` and reads them with `projection`.
    pub fn new(id: BackendId, build: Build, projection: MakeProjection) -> RuntimeBackend {
        RuntimeBackend {
            id,
            build,
            projection,
            patch: None,
            session: None,
        }
    }

    /// Lets the adapter apply [`InputVariant::State`] with `patch`.
    pub fn with_patch(mut self, patch: Patch) -> RuntimeBackend {
        self.patch = Some(patch);
        self
    }

    /// The events dispatched when observation `ordinal` of `stream` was emitted: the
    /// checkpoint "after boundary `ordinal`" is the one after this many events.
    pub fn event_index(&self, stream: &str, ordinal: u64) -> Option<u64> {
        let s = self.session.as_ref()?;
        let shared = s.shared.borrow();
        shared.events.get(stream)?.get(ordinal as usize).copied()
    }

    /// The running runtime, for read-only inspection.
    pub fn runtime(&self) -> Option<&Runtime> {
        self.session.as_ref().map(|s| &s.runtime)
    }
}

impl Session {
    /// Dispatches one event, applying the state patch first when it is due.
    fn advance(&mut self, build: &Build, patch: Option<&Patch>) -> Result<(), EngineError> {
        if self
            .patch
            .as_ref()
            .is_some_and(|p| p.event_index == self.events)
        {
            let p = self.patch.take().expect("checked above");
            self.apply(build, patch, &p)?;
        }
        if self.events >= self.budget {
            return Err(EngineError::BackendFailed(format!(
                "the safety budget of {} events ran out before the stop rule",
                self.budget
            )));
        }
        self.shared.borrow_mut().event_index = self.events + 1;
        match self.runtime.step() {
            Ok(Some(_)) => {
                self.events += 1;
                Ok(())
            }
            Ok(None) => Err(EngineError::BackendFailed(format!(
                "the event queue drained after {} events, before the stop rule",
                self.events
            ))),
            Err(e) => Err(EngineError::BackendFailed(format!(
                "event {}: {e}",
                self.events + 1
            ))),
        }
    }

    /// Snapshots, patches, and resumes the patched checkpoint in a fresh runtime.
    fn apply(
        &mut self,
        build: &Build,
        patch: Option<&Patch>,
        p: &StatePatch,
    ) -> Result<(), EngineError> {
        let Some(patch) = patch else {
            return Err(EngineError::Unsupported("state patch"));
        };
        let failed = |why: String| EngineError::BackendFailed(format!("state patch: {why}"));
        let mut bytes = self.runtime.snapshot().map_err(|e| failed(e.to_string()))?;
        patch(&mut bytes[..], p).map_err(failed)?;
        let mut fresh = build(&self.scenario).map_err(failed)?;
        fresh
            .restore(&bytes)
            .map_err(|e| failed(format!("the patched checkpoint was refused: {e}")))?;
        fresh.add_observer(Box::new(Tap(Rc::clone(&self.shared))));
        self.runtime = fresh;
        Ok(())
    }
}

impl Backend for RuntimeBackend {
    fn describe(&self) -> BackendId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = Capabilities::from([Capability::StepBoundary, Capability::DeterministicReplay]);
        let streams = (self.projection)().support().streams;
        for (name, cap) in [
            ("arch", Capability::ArchStream),
            ("effect", Capability::EffectStream),
            ("os", Capability::OsStream),
        ] {
            if streams.contains(name) {
                c.insert(cap);
            }
        }
        c
    }

    fn support(&self) -> Support {
        (self.projection)().support()
    }

    fn start(
        &mut self,
        scenario: &Scenario,
        input: &InputVariant,
        config: &BackendRunConfig,
        profile: &ComparisonProfile,
    ) -> Result<(), EngineError> {
        self.session = None;
        let patch = match input {
            InputVariant::None => None,
            InputVariant::State(p) if self.patch.is_some() => Some(p.clone()),
            InputVariant::State(_) => return Err(EngineError::Unsupported("state patch")),
        };
        let mut projection = (self.projection)();
        projection.begin(profile).map_err(|why| {
            EngineError::Internal(format!("projection refused the profile: {why}"))
        })?;
        let shared = Rc::new(RefCell::new(Shared {
            projection,
            profile: profile.clone(),
            event_index: 0,
            queues: BTreeMap::new(),
            events: BTreeMap::new(),
            error: None,
        }));
        let mut runtime = (self.build)(scenario).map_err(EngineError::BackendFailed)?;
        runtime.add_observer(Box::new(Tap(Rc::clone(&shared))));
        runtime
            .init()
            .map_err(|e| EngineError::BackendFailed(format!("init: {e}")))?;
        self.session = Some(Session {
            runtime,
            shared,
            scenario: scenario.clone(),
            budget: config.budget,
            events: 0,
            patch,
        });
        Ok(())
    }

    fn next(&mut self, stream: &str) -> Result<Option<Observation>, EngineError> {
        let RuntimeBackend {
            session,
            build,
            patch,
            ..
        } = self;
        let Some(s) = session.as_mut() else {
            return Err(EngineError::Internal("next before start".to_owned()));
        };
        if !s
            .shared
            .borrow()
            .profile
            .streams()
            .any(|x| x.stream == stream)
        {
            return Err(EngineError::Internal(format!(
                "{stream} is not a compared stream"
            )));
        }
        loop {
            {
                let mut shared = s.shared.borrow_mut();
                if let Some(o) = shared.queues.get_mut(stream).and_then(VecDeque::pop_front) {
                    return Ok(Some(o));
                }
                if let Some(e) = shared.error.clone() {
                    return Err(e);
                }
                if shared.projection.finished() {
                    return Ok(None);
                }
            }
            s.advance(build, patch.as_ref())?;
        }
    }
}
