//! The SystemScope runtime adapter: steps a runtime until a projection of its trace
//! records yields the next observation (`docs/m4-design.md` §8.4, §9.2).

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::{Control, Observer, WorldView};
use systemscope_contracts::time::Tick;
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_runtime::runtime::Runtime;
use systemscope_runtime::snapshot::component_states;

use crate::backend::{Backend, Capabilities, Capability};
use crate::case::{BackendId, BackendRunConfig, InputVariant, Scenario, StatePatch, execution_id};
use crate::checkpoint::{Checkpoint, StreamMark};
use crate::error::EngineError;
use crate::observation::{Observation, ObservedValue, PrefixDigest};
use crate::profile::{ComparisonProfile, Support};
use crate::state::{Source, StateField};

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
/// re-executes (§8.4). A projection holds no state across event boundaries except whether
/// the stop rule has been reached, so a fresh one resumes a restored checkpoint.
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

/// Builds the runtime of a scenario under a side's input variant, elaborated and not yet
/// initialized. A build applies [`InputVariant::Media`] to the scenario's media, or
/// refuses it; it may ignore the other variants, which the adapter applies.
pub type Build = Box<dyn Fn(&Scenario, &InputVariant) -> Result<Runtime, String>>;
/// Makes a fresh projection.
pub type MakeProjection = Box<dyn Fn() -> Box<dyn Projection>>;
/// Applies a state patch to runtime snapshot bytes, checking the old value.
pub type Patch = Box<dyn Fn(&mut [u8], &StatePatch) -> Result<(), String>>;
/// Reads a platform's state fields from its snapshot bytes (§13.2).
pub type StateDecoder = Box<dyn Fn(&[u8]) -> Result<Vec<StateField>, String>>;

/// What the observer shares with the adapter.
struct Shared {
    projection: Box<dyn Projection>,
    profile: ComparisonProfile,
    /// The events dispatched once the current one is done; 0 during `init`.
    event_index: u64,
    /// Every compared stream, in name order.
    streams: BTreeMap<String, Stream>,
    error: Option<EngineError>,
    /// The component paths, by id, while a state read is asked for; the state read.
    reading: Option<Vec<String>>,
    state: Option<Vec<StateField>>,
    /// Every trace record since capture began, until taken.
    captured: Option<Vec<TraceRecord>>,
}

/// One compared stream of a running side.
struct Stream {
    /// The observations emitted and not yet taken.
    queue: VecDeque<Observation>,
    /// The prefix digest of every observation emitted, from ordinal 0.
    prefix: PrefixDigest,
    /// The ordinal of `events[0]`: 0, or the covered count of a restored checkpoint.
    first: u64,
    /// The event index that emitted each observation since `first`.
    events: Vec<u64>,
}

impl Stream {
    fn resume(covered: u64, prefix: [u8; 32]) -> Stream {
        Stream {
            queue: VecDeque::new(),
            prefix: PrefixDigest::resume(prefix, covered),
            first: covered,
            events: Vec::new(),
        }
    }
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
        let s = self
            .streams
            .get_mut(&p.stream)
            .expect("every compared stream has an entry");
        let o = Observation {
            ordinal: s.prefix.count(),
            stream: p.stream,
            kind: p.kind,
            fields,
        };
        s.prefix.absorb(&o);
        s.events.push(self.event_index);
        s.queue.push_back(o);
        Ok(())
    }
}

/// Hands every record to the shared projection.
struct Tap(Rc<RefCell<Shared>>);

impl Observer for Tap {
    fn on_trace(&mut self, record: &TraceRecord) {
        let mut shared = self.0.borrow_mut();
        if let Some(c) = &mut shared.captured {
            c.push(record.clone());
        }
        shared.take(record);
    }

    fn on_observe(&mut self, _: Tick, world: &WorldView<'_>) -> Control {
        let mut shared = self.0.borrow_mut();
        if let Some(paths) = shared.reading.take() {
            let mut fields = Vec::new();
            for (id, path) in paths.iter().enumerate() {
                let view = world.inspect(ComponentId(id as u32)).unwrap_or_default();
                fields.extend(view.fields.into_iter().map(|(name, value)| StateField {
                    component: path.clone(),
                    field: name.to_owned(),
                    value,
                    source: Source::Inspect,
                }));
            }
            shared.state = Some(fields);
        }
        Control::Continue
    }
}

/// A running side.
struct Session {
    runtime: Runtime,
    shared: Rc<RefCell<Shared>>,
    scenario: Scenario,
    input: InputVariant,
    execution: [u8; 32],
    budget: u64,
    events: u64,
    /// The input variant's patch, and whether it is still to be applied.
    patch: Option<StatePatch>,
    pending: bool,
}

/// A [`Backend`] over any SystemScope runtime and projection.
pub struct RuntimeBackend {
    id: BackendId,
    build: Build,
    projection: MakeProjection,
    patch: Option<Patch>,
    decoder: Option<StateDecoder>,
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
            decoder: None,
            session: None,
        }
    }

    /// Adds `decoder`'s fields to every state read.
    pub fn with_decoder(mut self, decoder: StateDecoder) -> RuntimeBackend {
        self.decoder = Some(decoder);
        self
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
        let s = shared.streams.get(stream)?;
        let at = ordinal.checked_sub(s.first)?;
        s.events.get(usize::try_from(at).ok()?).copied()
    }

    /// The running runtime, for read-only inspection.
    pub fn runtime(&self) -> Option<&Runtime> {
        self.session.as_ref().map(|s| &s.runtime)
    }

    /// Keeps every trace record the started side emits from now on, until the next
    /// `start` or `restore` (§14.1).
    pub fn capture(&mut self) -> Result<(), EngineError> {
        let Some(s) = self.session.as_mut() else {
            return Err(EngineError::Internal("capture before start".to_owned()));
        };
        s.shared.borrow_mut().captured.get_or_insert_with(Vec::new);
        Ok(())
    }

    /// The trace records kept since the last call, in emission order.
    pub fn captured(&mut self) -> Vec<TraceRecord> {
        self.session
            .as_mut()
            .and_then(|s| s.shared.borrow_mut().captured.as_mut().map(std::mem::take))
            .unwrap_or_default()
    }

    /// The events the started side has dispatched.
    pub fn events(&self) -> Option<u64> {
        self.session.as_ref().map(|s| s.events)
    }

    /// Dispatches one event, under the same budget and patch as [`Backend::next`]; its
    /// observations are queued for `next`. Same-engine only: an event is not a
    /// comparison boundary (§12.1).
    pub fn step_event(&mut self) -> Result<(), EngineError> {
        let RuntimeBackend {
            session,
            build,
            patch,
            ..
        } = self;
        let Some(s) = session.as_mut() else {
            return Err(EngineError::Internal("step before start".to_owned()));
        };
        if let Some(e) = s.shared.borrow().error.clone() {
            return Err(e);
        }
        s.advance(build, patch.as_ref())
    }
}

impl Session {
    /// Applies the state patch once `events` reaches its event index, so that every
    /// checkpoint after that many events holds it.
    fn patch_if_due(&mut self, build: &Build, patch: Option<&Patch>) -> Result<(), EngineError> {
        let Some(p) = self.patch.clone() else {
            return Ok(());
        };
        if self.pending && p.event_index == self.events {
            self.pending = false;
            self.apply(build, patch, &p)?;
        }
        Ok(())
    }

    /// Dispatches one event, then applies the state patch if it is due.
    fn advance(&mut self, build: &Build, patch: Option<&Patch>) -> Result<(), EngineError> {
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
                self.patch_if_due(build, patch)
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

    /// Reads every component's `inspect()` at an observe point registered at tick 0,
    /// which is due at once, and runs no further than tick 0, where no event is left.
    /// Observe points are driver state, so the read changes nothing (§10).
    fn inspect(&mut self) -> Result<Vec<StateField>, EngineError> {
        let s = self;
        if s.runtime.peek_key().is_some_and(|k| k.tick == Tick(0)) {
            return Err(EngineError::Internal(
                "state while events at tick 0 are pending".to_owned(),
            ));
        }
        let paths = s.runtime.component_paths().map(str::to_owned).collect();
        s.shared.borrow_mut().reading = Some(paths);
        s.runtime.observe_at(Tick(0));
        let ran = s.runtime.run(Tick(0));
        let mut shared = s.shared.borrow_mut();
        shared.reading = None;
        let state = shared.state.take();
        match ran {
            Ok(r) if r.events == 0 => {}
            Ok(_) => {
                return Err(EngineError::Internal(
                    "a state read ran an event".to_owned(),
                ));
            }
            Err(e) => return Err(EngineError::BackendFailed(format!("state: {e}"))),
        }
        state.ok_or_else(|| EngineError::Internal("the state read did not fire".to_owned()))
    }

    /// Snapshots, patches, and resumes the patched checkpoint in a fresh runtime.
    fn apply(
        &mut self,
        build: &Build,
        patch: Option<&Patch>,
        p: &StatePatch,
    ) -> Result<(), EngineError> {
        let Some(patch) = patch else {
            return Err(EngineError::Unsupported("state patch".to_owned()));
        };
        let failed = |why: String| EngineError::BackendFailed(format!("state patch: {why}"));
        let mut bytes = self.runtime.snapshot().map_err(|e| failed(e.to_string()))?;
        patch(&mut bytes[..], p).map_err(failed)?;
        let mut fresh = build(&self.scenario, &self.input).map_err(failed)?;
        fresh
            .restore(&bytes)
            .map_err(|e| failed(format!("the patched checkpoint was refused: {e}")))?;
        fresh.add_observer(Box::new(Tap(Rc::clone(&self.shared))));
        self.runtime = fresh;
        Ok(())
    }

    /// Refuses a stream the profile does not compare.
    fn check_stream(&self, stream: &str) -> Result<(), EngineError> {
        if self.shared.borrow().streams.contains_key(stream) {
            Ok(())
        } else {
            Err(EngineError::Internal(format!(
                "{stream} is not a compared stream"
            )))
        }
    }
}

/// A fresh observer state for `profile`, with every compared stream at `mark`.
fn shared(
    make: &MakeProjection,
    profile: &ComparisonProfile,
    event_index: u64,
    mark: impl Fn(&str) -> (u64, [u8; 32]),
) -> Result<Rc<RefCell<Shared>>, EngineError> {
    let mut projection = make();
    projection
        .begin(profile)
        .map_err(|why| EngineError::Internal(format!("projection refused the profile: {why}")))?;
    let streams = profile
        .streams()
        .map(|s| {
            let (covered, prefix) = mark(&s.stream);
            (s.stream.clone(), Stream::resume(covered, prefix))
        })
        .collect();
    Ok(Rc::new(RefCell::new(Shared {
        projection,
        profile: profile.clone(),
        event_index,
        streams,
        error: None,
        reading: None,
        state: None,
        captured: None,
    })))
}

impl Backend for RuntimeBackend {
    fn describe(&self) -> BackendId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        let mut c = Capabilities::from([
            Capability::StepBoundary,
            Capability::DeterministicReplay,
            Capability::Snapshot,
            Capability::Restore,
            Capability::State,
        ]);
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
            InputVariant::None | InputVariant::Media(_) => None,
            InputVariant::State(p) if self.patch.is_some() => Some(p.clone()),
            InputVariant::State(_) => {
                return Err(EngineError::Unsupported("state patch".to_owned()));
            }
        };
        let shared = shared(&self.projection, profile, 0, |_| (0, [0; 32]))?;
        let mut runtime = (self.build)(scenario, input).map_err(EngineError::BackendFailed)?;
        runtime.add_observer(Box::new(Tap(Rc::clone(&shared))));
        runtime
            .init()
            .map_err(|e| EngineError::BackendFailed(format!("init: {e}")))?;
        let mut session = Session {
            runtime,
            shared,
            scenario: scenario.clone(),
            input: input.clone(),
            execution: execution_id(scenario, input, &self.id),
            budget: config.budget,
            events: 0,
            pending: patch.is_some(),
            patch,
        };
        session.patch_if_due(&self.build, self.patch.as_ref())?;
        self.session = Some(session);
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
        s.check_stream(stream)?;
        loop {
            {
                let mut shared = s.shared.borrow_mut();
                let queue = &mut shared.streams.get_mut(stream).expect("checked").queue;
                if let Some(o) = queue.pop_front() {
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

    fn emitted(&mut self, stream: &str) -> Result<Vec<Observation>, EngineError> {
        let Some(s) = self.session.as_mut() else {
            return Err(EngineError::Internal("emitted before start".to_owned()));
        };
        s.check_stream(stream)?;
        let mut shared = s.shared.borrow_mut();
        let queue = &mut shared.streams.get_mut(stream).expect("checked").queue;
        Ok(queue.drain(..).collect())
    }

    fn checkpoint(&self) -> Result<Checkpoint, EngineError> {
        let Some(s) = self.session.as_ref() else {
            return Err(EngineError::Internal("checkpoint before start".to_owned()));
        };
        let shared = s.shared.borrow();
        if let Some(e) = shared.error.clone() {
            return Err(e);
        }
        if shared.projection.finished() {
            return Err(EngineError::Internal(
                "checkpoint after the stop rule".to_owned(),
            ));
        }
        let state = s
            .runtime
            .snapshot()
            .map_err(|e| EngineError::BackendFailed(format!("snapshot: {e}")))?;
        Ok(Checkpoint {
            execution: s.execution,
            event_index: s.events,
            streams: shared
                .streams
                .iter()
                .map(|(name, st)| StreamMark {
                    stream: name.clone(),
                    covered: st.prefix.count(),
                    prefix: st.prefix.digest(),
                })
                .collect(),
            state,
        })
    }

    /// Every component's `inspect()`, then each component's snapshot schema and the
    /// BLAKE3 of its snapshot bytes, labeled derived, which differ whenever its stored
    /// state does, then the decoder's fields.
    fn state(&mut self) -> Result<Vec<StateField>, EngineError> {
        let Some(s) = self.session.as_mut() else {
            return Err(EngineError::Internal("state before start".to_owned()));
        };
        let mut fields = s.inspect()?;
        let paths: Vec<&str> = s.runtime.component_paths().collect();
        let bytes = s
            .runtime
            .snapshot()
            .map_err(|e| EngineError::BackendFailed(format!("snapshot: {e}")))?;
        let states = component_states(&bytes)
            .map_err(|e| EngineError::Internal(format!("the snapshot does not read back: {e}")))?;
        for (id, schema, state) in states {
            let component = paths.get(id as usize).copied().unwrap_or("?");
            let field = |field: &str, value| StateField {
                component: component.to_owned(),
                field: field.to_owned(),
                value,
                source: Source::Derived("snapshot".to_owned()),
            };
            fields.push(field("schema", Value::U64(schema.into())));
            fields.push(field(
                "blake3",
                Value::Bytes(blake3::hash(state).as_bytes().to_vec()),
            ));
        }
        if let Some(decode) = &self.decoder {
            let decoded = decode(&bytes)
                .map_err(|why| EngineError::BackendFailed(format!("state decoder: {why}")))?;
            fields.extend(decoded);
        }
        Ok(fields)
    }

    fn restore(&mut self, c: &Checkpoint) -> Result<(), EngineError> {
        let Some(s) = self.session.as_mut() else {
            return Err(EngineError::Internal("restore before start".to_owned()));
        };
        if c.execution != s.execution {
            return Err(EngineError::Internal(
                "the checkpoint belongs to another execution".to_owned(),
            ));
        }
        let profile = s.shared.borrow().profile.clone();
        let names: Vec<&str> = c.streams.iter().map(|m| m.stream.as_str()).collect();
        if !names
            .iter()
            .copied()
            .eq(s.shared.borrow().streams.keys().map(String::as_str))
        {
            return Err(EngineError::Internal(format!(
                "the checkpoint marks {names:?}, not the compared streams"
            )));
        }
        let shared = shared(&self.projection, &profile, c.event_index, |name| {
            let m = c.mark(name).expect("checked above");
            (m.covered, m.prefix)
        })?;
        let mut runtime =
            (self.build)(&s.scenario, &s.input).map_err(EngineError::BackendFailed)?;
        runtime
            .restore(&c.state)
            .map_err(|e| EngineError::BackendFailed(format!("restore: {e}")))?;
        runtime.add_observer(Box::new(Tap(Rc::clone(&shared))));
        s.runtime = runtime;
        s.shared = shared;
        s.events = c.event_index;
        s.pending = s
            .patch
            .as_ref()
            .is_some_and(|p| p.event_index > c.event_index);
        Ok(())
    }
}
