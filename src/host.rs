//! The model host: owns loaded models, drives each model's lifecycle,
//! persists residency and enforces admission and exclusive groups
//! (see `docs/runtime/model-lifecycle.md`).
//!
//! Synchronous by design (the local API is a thread-pool server): loads
//! and unloads run on their own threads and report back through the
//! lifecycle.  Lock order is always `entries` before `residency`.

use crate::admission::{self, FreeMemory, MemoryProbe, Refused};
use crate::catalog::{Catalog, CatalogModel};
use crate::lifecycle::{Command, Lifecycle, ModelState};
use crate::residency::Residency;
use chrono::{DateTime, Utc};
use parking_lot::{Condvar, Mutex, MutexGuard};
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const TRACE_TARGET: &str = "studio_worker::lifecycle";

/// How long an unload waits for the request in flight to notice it was
/// cancelled before freeing anyway (the request keeps the weights alive
/// until it returns).  A streaming chunk takes well under a second.
/// Safe range 1..=60 s.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a swap waits for the outgoing group member to unload before
/// loading anyway.  Covers `DRAIN_TIMEOUT` plus freeing.  Safe range
/// `DRAIN_TIMEOUT`..=120 s.
pub const SWAP_TIMEOUT: Duration = Duration::from_secs(30);

/// A model whose weights are in memory.  Engines downcast it back to
/// their own type through `as_any`.
pub trait LoadedModel: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    /// The chat interface, for loaded LLMs.
    fn as_chat(&self) -> Option<&dyn ChatModel> {
        None
    }

    /// The streaming interface, for loaded speech models.
    fn as_stream(&self) -> Option<&dyn StreamingModel> {
        None
    }
}

/// A loaded streaming speech model.  Each `open` is an independent
/// utterance state over the shared weights.
pub trait StreamingModel {
    fn open(
        &self,
    ) -> anyhow::Result<Box<dyn crate::stt_stream::session::StreamingTranscriber + '_>>;
}

/// A loaded model that answers chat completions.
pub trait ChatModel {
    /// Run one completion; `cancelled` turns true when an unload starts.
    /// Returns OpenAI `chat.completion`-shaped JSON.
    fn chat(
        &self,
        params: crate::types::LlmParams,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<serde_json::Value>;
}

/// Loads catalogue models into memory.  Freed by dropping the result.
pub trait ModelRuntime: Send + Sync {
    fn load(&self, model: &CatalogModel) -> anyhow::Result<Arc<dyn LoadedModel>>;
}

/// One model's observable status.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelStatus {
    pub id: String,
    pub state: ModelState,
    pub resident: bool,
    pub since: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("unknown model: {0}")]
    UnknownModel(String),
    #[error("model is disabled: {0}")]
    Disabled(String),
    #[error(transparent)]
    Refused(#[from] Refused),
    #[error("model {id} is not loaded ({state})")]
    NotLoaded { id: String, state: &'static str },
    #[error("model {0} is busy serving another request")]
    LaneBusy(String),
    #[error("could not persist residency: {0}")]
    Persist(#[from] std::io::Error),
}

/// The serving path of one loaded model: one request at a time, and a
/// cancel flag an unload raises so a long request (a stream) can end.
pub struct Lane {
    busy: Mutex<()>,
    cancel: AtomicBool,
}

impl Lane {
    fn new() -> Self {
        Self {
            busy: Mutex::new(()),
            cancel: AtomicBool::new(false),
        }
    }

    /// True once an unload has started; long requests should return.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

struct Entry {
    lifecycle: Lifecycle,
    since: DateTime<Utc>,
    loaded: Option<(Arc<dyn LoadedModel>, Arc<Lane>)>,
}

impl Entry {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            since: Utc::now(),
            loaded: None,
        }
    }
}

struct Inner {
    catalog: Arc<Mutex<Catalog>>,
    runtime: Arc<dyn ModelRuntime>,
    probe: Arc<dyn MemoryProbe + Send + Sync>,
    residency: Mutex<Residency>,
    entries: Mutex<HashMap<String, Entry>>,
    changed: Condvar,
    subscribers: Mutex<Vec<mpsc::Sender<ModelStatus>>>,
}

/// Cheap to clone; every clone is the same host.
#[derive(Clone)]
pub struct ModelHost {
    inner: Arc<Inner>,
}

impl ModelHost {
    pub fn new(
        catalog: Arc<Mutex<Catalog>>,
        runtime: Arc<dyn ModelRuntime>,
        probe: Arc<dyn MemoryProbe + Send + Sync>,
        residency: Residency,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                catalog,
                runtime,
                probe,
                residency: Mutex::new(residency),
                entries: Mutex::new(HashMap::new()),
                changed: Condvar::new(),
                subscribers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Status of one catalogue model.
    pub fn status(&self, id: &str) -> Result<ModelStatus, HostError> {
        self.catalogue_model(id)?;
        let mut entries = self.inner.entries.lock();
        Ok(self.status_locked(&mut entries, id))
    }

    /// Status of every catalogue model, in catalogue order.
    pub fn statuses(&self) -> Vec<ModelStatus> {
        let ids: Vec<String> = self
            .inner
            .catalog
            .lock()
            .list()
            .iter()
            .map(|m| m.id.clone())
            .collect();
        let mut entries = self.inner.entries.lock();
        ids.iter()
            .map(|id| self.status_locked(&mut entries, id))
            .collect()
    }

    /// Receive every state transition from now on.
    pub fn subscribe(&self) -> mpsc::Receiver<ModelStatus> {
        let (tx, rx) = mpsc::channel();
        self.inner.subscribers.lock().push(tx);
        rx
    }

    /// Sum of the estimates of models holding (or about to hold) memory.
    pub fn loaded_gib(&self) -> f32 {
        let catalog = self.inner.catalog.lock().list().to_vec();
        let entries = self.inner.entries.lock();
        loaded_gib(&catalog, &entries)
    }

    /// Load `id` and mark it resident.  Answers the state after the
    /// request: `loading`, or `loaded` when it already was.
    pub fn load(&self, id: &str) -> Result<ModelStatus, HostError> {
        let model = self.catalogue_model(id)?;
        if !model.enabled {
            return Err(HostError::Disabled(id.to_string()));
        }
        let catalog = self.inner.catalog.lock().list().to_vec();
        let mut entries = self.inner.entries.lock();
        let needs_load = matches!(
            entry(&mut entries, id).lifecycle.state(),
            ModelState::Unloaded | ModelState::Failed { .. }
        );
        let swap_out: Vec<String> = match &model.exclusive_group {
            Some(group) => catalog
                .iter()
                .filter(|m| m.id != id && m.exclusive_group.as_ref() == Some(group))
                .filter(|m| {
                    entries.get(&m.id).is_some_and(|e| {
                        matches!(
                            e.lifecycle.state(),
                            ModelState::Loading | ModelState::Loaded
                        )
                    })
                })
                .map(|m| m.id.clone())
                .collect(),
            None => Vec::new(),
        };
        if needs_load {
            let freed: f32 = catalog
                .iter()
                .filter(|m| swap_out.contains(&m.id))
                .map(|m| m.vram_gb_estimate)
                .sum();
            let free =
                admission::free_now(self.inner.probe.as_ref(), loaded_gib(&catalog, &entries));
            let free = credit(free, freed);
            if let Err(refused) = admission::admit(model.vram_gb_estimate, &free) {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "admit",
                    model = id,
                    error = %refused,
                    "load refused"
                );
                return Err(refused.into());
            }
        }
        self.inner.residency.lock().set(id, true)?;
        for other in &swap_out {
            self.inner.residency.lock().set(other, false)?;
            self.request_unload_locked(&mut entries, other, "swap");
        }
        let from = entry(&mut entries, id).lifecycle.state().clone();
        let command = entry(&mut entries, id).lifecycle.request_load();
        self.after_transition(&mut entries, id, "load", &from, None);
        if command == Command::BeginLoad {
            self.spawn_load(model, swap_out);
        }
        Ok(self.status_locked(&mut entries, id))
    }

    /// Unload `id` and clear its residency.
    pub fn unload(&self, id: &str) -> Result<ModelStatus, HostError> {
        self.catalogue_model(id)?;
        let mut entries = self.inner.entries.lock();
        self.inner.residency.lock().set(id, false)?;
        self.request_unload_locked(&mut entries, id, "unload");
        Ok(self.status_locked(&mut entries, id))
    }

    /// Load every resident model, in catalogue order.  Failures are
    /// logged; a refused or failed model stays resident for next time.
    pub fn restore_residents(&self) {
        let resident: Vec<String> = self
            .inner
            .residency
            .lock()
            .ids()
            .map(String::from)
            .collect();
        let catalog_ids: Vec<String> = self
            .inner
            .catalog
            .lock()
            .list()
            .iter()
            .map(|m| m.id.clone())
            .collect();
        for id in resident.iter().filter(|id| !catalog_ids.contains(id)) {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "restore",
                model = %id,
                "resident model is not in the catalogue; skipped"
            );
        }
        for id in catalog_ids.iter().filter(|id| resident.contains(id)) {
            match self.load(id) {
                Ok(_) => tracing::info!(
                    target: TRACE_TARGET,
                    op = "restore",
                    model = %id,
                    "restoring resident model"
                ),
                Err(err) => tracing::warn!(
                    target: TRACE_TARGET,
                    op = "restore",
                    model = %id,
                    error = %err,
                    "resident model not restored; stays resident for the next start"
                ),
            }
        }
    }

    /// Serve one request on `id`'s lane.  Blocks while the lane is busy.
    pub fn with_lane<R>(
        &self,
        id: &str,
        f: impl FnOnce(&dyn LoadedModel, &Lane) -> R,
    ) -> Result<R, HostError> {
        let (model, lane) = self.lane_of(id)?;
        let _busy = lane.busy.lock();
        Self::serve(id, model.as_ref(), &lane, f)
    }

    /// Like [`Self::with_lane`] but refuses (`LaneBusy`) instead of waiting,
    /// for long requests such as a stream that would otherwise queue.
    pub fn try_with_lane<R>(
        &self,
        id: &str,
        f: impl FnOnce(&dyn LoadedModel, &Lane) -> R,
    ) -> Result<R, HostError> {
        let (model, lane) = self.lane_of(id)?;
        let Some(_busy) = lane.busy.try_lock() else {
            return Err(HostError::LaneBusy(id.to_string()));
        };
        Self::serve(id, model.as_ref(), &lane, f)
    }

    fn serve<R>(
        id: &str,
        model: &dyn LoadedModel,
        lane: &Lane,
        f: impl FnOnce(&dyn LoadedModel, &Lane) -> R,
    ) -> Result<R, HostError> {
        if lane.cancelled() {
            return Err(HostError::NotLoaded {
                id: id.to_string(),
                state: ModelState::Unloading.name(),
            });
        }
        Ok(f(model, lane))
    }

    fn lane_of(&self, id: &str) -> Result<(Arc<dyn LoadedModel>, Arc<Lane>), HostError> {
        let mut entries = self.inner.entries.lock();
        let e = entry(&mut entries, id);
        match (&e.loaded, e.lifecycle.state().serves()) {
            (Some((m, l)), true) => Ok((m.clone(), l.clone())),
            _ => Err(HostError::NotLoaded {
                id: id.to_string(),
                state: e.lifecycle.state().name(),
            }),
        }
    }

    /// Block until `id`'s state satisfies `pred`, or `timeout` passes.
    pub fn wait_for(
        &self,
        id: &str,
        pred: impl Fn(&ModelState) -> bool,
        timeout: Duration,
    ) -> Option<ModelStatus> {
        let deadline = Instant::now() + timeout;
        let mut entries = self.inner.entries.lock();
        loop {
            if pred(entry(&mut entries, id).lifecycle.state()) {
                return Some(self.status_locked(&mut entries, id));
            }
            if self
                .inner
                .changed
                .wait_until(&mut entries, deadline)
                .timed_out()
            {
                return None;
            }
        }
    }

    fn catalogue_model(&self, id: &str) -> Result<CatalogModel, HostError> {
        self.inner
            .catalog
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| HostError::UnknownModel(id.to_string()))
    }

    fn status_locked(&self, entries: &mut HashMap<String, Entry>, id: &str) -> ModelStatus {
        let e = entry(entries, id);
        ModelStatus {
            id: id.to_string(),
            state: e.lifecycle.state().clone(),
            resident: self.inner.residency.lock().is_resident(id),
            since: e.since,
        }
    }

    fn request_unload_locked(
        &self,
        entries: &mut HashMap<String, Entry>,
        id: &str,
        op: &'static str,
    ) {
        let from = entry(entries, id).lifecycle.state().clone();
        let command = entry(entries, id).lifecycle.request_unload();
        self.after_transition(entries, id, op, &from, None);
        if command == Command::BeginUnload {
            self.spawn_unload(entries, id);
        }
    }

    /// Log, timestamp and publish a transition if the state changed.
    fn after_transition(
        &self,
        entries: &mut HashMap<String, Entry>,
        id: &str,
        op: &'static str,
        from: &ModelState,
        error: Option<&str>,
    ) {
        let e = entry(entries, id);
        let to = e.lifecycle.state().clone();
        if &to == from {
            return;
        }
        e.since = Utc::now();
        match error {
            None => tracing::info!(
                target: TRACE_TARGET,
                op,
                model = id,
                from = from.name(),
                to = to.name(),
                "model state changed"
            ),
            Some(error) => tracing::warn!(
                target: TRACE_TARGET,
                op,
                model = id,
                from = from.name(),
                to = to.name(),
                error,
                "model state changed"
            ),
        }
        let status = self.status_locked(entries, id);
        self.inner
            .subscribers
            .lock()
            .retain(|tx| tx.send(status.clone()).is_ok());
        self.inner.changed.notify_all();
    }

    fn spawn_load(&self, model: CatalogModel, wait_for_unloaded: Vec<String>) {
        let host = self.clone();
        std::thread::spawn(move || {
            for other in &wait_for_unloaded {
                if host
                    .wait_for(
                        other,
                        |s| matches!(s, ModelState::Unloaded | ModelState::Failed { .. }),
                        SWAP_TIMEOUT,
                    )
                    .is_none()
                {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "swap",
                        model = %model.id,
                        outgoing = %other,
                        "outgoing model did not unload in time; loading anyway"
                    );
                }
            }
            let result = host.inner.runtime.load(&model);
            host.finish_load(&model.id, result);
        });
    }

    fn finish_load(&self, id: &str, result: anyhow::Result<Arc<dyn LoadedModel>>) {
        let mut entries = self.inner.entries.lock();
        let from = entry(&mut entries, id).lifecycle.state().clone();
        let (outcome, loaded) = match result {
            Ok(m) => (Ok(()), Some((m, Arc::new(Lane::new())))),
            Err(err) => (Err(format!("{err:#}")), None),
        };
        let error = outcome.as_ref().err().cloned();
        let e = entry(&mut entries, id);
        match e.lifecycle.load_finished(outcome) {
            Ok(command) => {
                e.loaded = loaded;
                self.after_transition(&mut entries, id, "load", &from, error.as_deref());
                if command == Command::BeginUnload {
                    self.spawn_unload(&mut entries, id);
                }
            }
            Err(unexpected) => tracing::error!(
                target: TRACE_TARGET,
                op = "load",
                model = id,
                error = %unexpected,
                "load finished in a state that never started it; result dropped"
            ),
        }
    }

    fn spawn_unload(&self, entries: &mut HashMap<String, Entry>, id: &str) {
        let lane = entry(entries, id).loaded.as_ref().map(|(_, l)| l.clone());
        let host = self.clone();
        let id = id.to_string();
        std::thread::spawn(move || {
            if let Some(lane) = lane {
                lane.cancel.store(true, Ordering::SeqCst);
                if lane.busy.try_lock_for(DRAIN_TIMEOUT).is_none() {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "unload",
                        model = %id,
                        "request in flight did not end in time; memory frees when it returns"
                    );
                }
            }
            let mut entries = host.inner.entries.lock();
            let from = entry(&mut entries, &id).lifecycle.state().clone();
            let e = entry(&mut entries, &id);
            e.loaded = None;
            match e.lifecycle.unload_finished(Ok(())) {
                Ok(_) => host.after_transition(&mut entries, &id, "unload", &from, None),
                Err(unexpected) => tracing::error!(
                    target: TRACE_TARGET,
                    op = "unload",
                    model = %id,
                    error = %unexpected,
                    "unload finished in a state that never started it"
                ),
            }
        });
    }
}

fn entry<'a>(entries: &'a mut HashMap<String, Entry>, id: &str) -> &'a mut Entry {
    entries.entry(id.to_string()).or_insert_with(Entry::new)
}

fn loaded_gib(catalog: &[CatalogModel], entries: &MutexGuard<'_, HashMap<String, Entry>>) -> f32 {
    catalog
        .iter()
        .filter(|m| {
            entries.get(&m.id).is_some_and(|e| {
                matches!(
                    e.lifecycle.state(),
                    ModelState::Loading | ModelState::Loaded | ModelState::Unloading
                )
            })
        })
        .map(|m| m.vram_gb_estimate)
        .sum()
}

/// Credit memory a swap will free to the measured free memory.
fn credit(free: FreeMemory, freed_gib: f32) -> FreeMemory {
    match free {
        FreeMemory::Unknown => FreeMemory::Unknown,
        other if freed_gib == 0.0 => other,
        other => FreeMemory::Probed {
            gib: other.gib() + freed_gib,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, CatalogModel};
    use crate::lifecycle::ModelState;
    use crate::test_support::FixedProbe;
    use crate::types::{ModelEngine, ModelSource, TaskKind};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    struct FakeLoaded {
        id: String,
        drops: Arc<AtomicUsize>,
    }
    impl LoadedModel for FakeLoaded {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    impl Drop for FakeLoaded {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Loads succeed unless the id is in `fail`; `gate` holds loads until opened.
    #[derive(Default)]
    struct FakeRuntime {
        fail: Mutex<Vec<String>>,
        gate: Mutex<bool>,
        gate_cv: Condvar,
        loads: Mutex<Vec<String>>,
        drops: Arc<AtomicUsize>,
    }
    impl FakeRuntime {
        fn open() -> Arc<Self> {
            let r = Self::default();
            *r.gate.lock() = true;
            Arc::new(r)
        }
        fn held() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn release(&self) {
            *self.gate.lock() = true;
            self.gate_cv.notify_all();
        }
    }
    impl ModelRuntime for FakeRuntime {
        fn load(&self, model: &CatalogModel) -> anyhow::Result<Arc<dyn LoadedModel>> {
            let mut open = self.gate.lock();
            while !*open {
                self.gate_cv.wait(&mut open);
            }
            drop(open);
            self.loads.lock().push(model.id.clone());
            if self.fail.lock().contains(&model.id) {
                anyhow::bail!("cannot load {}", model.id);
            }
            Ok(Arc::new(FakeLoaded {
                id: model.id.clone(),
                drops: self.drops.clone(),
            }))
        }
    }

    fn model(id: &str, gib: f32, group: Option<&str>) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            display_name: id.into(),
            kind: TaskKind::AudioStt,
            vram_gb_estimate: gib,
            description: None,
            source: ModelSource {
                engine: ModelEngine::Synthetic,
                files: vec![],
                cli_defaults: Default::default(),
            },
            enabled: true,
            origin: "local".into(),
            exclusive_group: group.map(Into::into),
        }
    }

    struct Fixture {
        host: ModelHost,
        _dir: tempfile::TempDir,
        residency_path: std::path::PathBuf,
    }

    fn fixture(models: Vec<CatalogModel>, free_gib: f32, runtime: Arc<FakeRuntime>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let residency_path = dir.path().join("residency.json");
        fixture_in(dir, residency_path, models, free_gib, runtime)
    }

    fn fixture_in(
        dir: tempfile::TempDir,
        residency_path: std::path::PathBuf,
        models: Vec<CatalogModel>,
        free_gib: f32,
        runtime: Arc<FakeRuntime>,
    ) -> Fixture {
        let catalog = Arc::new(Mutex::new(Catalog { models }));
        let residency = Residency::load_for_serving(Some(residency_path.clone()));
        let host = ModelHost::new(catalog, runtime, Arc::new(FixedProbe(free_gib)), residency);
        Fixture {
            host,
            _dir: dir,
            residency_path,
        }
    }

    fn state_of(host: &ModelHost, id: &str) -> ModelState {
        host.status(id).unwrap().state
    }

    #[test]
    fn every_catalogue_model_starts_unloaded_and_not_resident() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        let s = f.host.status("a").unwrap();
        assert_eq!(s.state, ModelState::Unloaded);
        assert!(!s.resident);
        assert_eq!(f.host.statuses().len(), 1);
    }

    #[test]
    fn load_reaches_loaded_and_marks_resident() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        let s = f.host.load("a").unwrap();
        assert!(matches!(s.state, ModelState::Loading | ModelState::Loaded));
        assert!(s.resident);
        let s = f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        assert_eq!(s.state, ModelState::Loaded);
        assert!(std::fs::read_to_string(&f.residency_path)
            .unwrap()
            .contains("\"a\""));
    }

    #[test]
    fn a_held_load_shows_loading() {
        let rt = FakeRuntime::held();
        let f = fixture(vec![model("a", 1.0, None)], 20.0, rt.clone());
        assert_eq!(f.host.load("a").unwrap().state, ModelState::Loading);
        assert_eq!(state_of(&f.host, "a"), ModelState::Loading);
        rt.release();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
    }

    #[test]
    fn a_second_load_while_loading_starts_nothing_new() {
        let rt = FakeRuntime::held();
        let f = fixture(vec![model("a", 1.0, None)], 20.0, rt.clone());
        f.host.load("a").unwrap();
        f.host.load("a").unwrap();
        rt.release();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        assert_eq!(rt.loads.lock().len(), 1);
    }

    #[test]
    fn a_refused_load_changes_nothing() {
        let f = fixture(vec![model("a", 8.0, None)], 5.0, FakeRuntime::open());
        let err = f.host.load("a").unwrap_err();
        assert!(matches!(err, HostError::Refused(_)), "{err}");
        let s = f.host.status("a").unwrap();
        assert_eq!(s.state, ModelState::Unloaded);
        assert!(!s.resident);
        assert!(!f.residency_path.exists());
    }

    #[test]
    fn unknown_and_disabled_models_are_rejected_by_name() {
        let mut off = model("off", 1.0, None);
        off.enabled = false;
        let f = fixture(vec![off], 20.0, FakeRuntime::open());
        assert!(matches!(f.host.load("nope"), Err(HostError::UnknownModel(id)) if id == "nope"));
        assert!(matches!(
            f.host.status("nope"),
            Err(HostError::UnknownModel(_))
        ));
        assert!(matches!(
            f.host.unload("nope"),
            Err(HostError::UnknownModel(_))
        ));
        assert!(matches!(f.host.load("off"), Err(HostError::Disabled(id)) if id == "off"));
    }

    #[test]
    fn a_failed_load_shows_failed_with_the_reason_and_stays_resident() {
        let rt = FakeRuntime::open();
        rt.fail.lock().push("a".into());
        let f = fixture(vec![model("a", 1.0, None)], 20.0, rt);
        f.host.load("a").unwrap();
        let s = f
            .host
            .wait_for("a", |s| matches!(s, ModelState::Failed { .. }), WAIT)
            .unwrap();
        match s.state {
            ModelState::Failed { reason } => assert!(reason.contains("cannot load a"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(s.resident, "the wish survives so the next start retries");
    }

    #[test]
    fn unload_frees_the_model_and_clears_residency() {
        let rt = FakeRuntime::open();
        let f = fixture(vec![model("a", 1.0, None)], 20.0, rt.clone());
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        let s = f.host.unload("a").unwrap();
        assert!(!s.resident);
        f.host
            .wait_for("a", |s| *s == ModelState::Unloaded, WAIT)
            .unwrap();
        assert_eq!(rt.drops.load(Ordering::SeqCst), 1, "weights dropped");
        assert!(!std::fs::read_to_string(&f.residency_path)
            .unwrap()
            .contains("\"a\""));
    }

    #[test]
    fn unload_of_an_unloaded_model_is_a_noop() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        assert_eq!(f.host.unload("a").unwrap().state, ModelState::Unloaded);
    }

    #[test]
    fn unload_waits_for_the_request_in_flight_and_signals_it() {
        let rt = FakeRuntime::open();
        let f = fixture(vec![model("a", 1.0, None)], 20.0, rt.clone());
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        let host = f.host.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let serving = std::thread::spawn(move || {
            host.with_lane("a", |_m, lane| {
                started_tx.send(()).unwrap();
                while !lane.cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                "stopped"
            })
        });
        started_rx.recv_timeout(WAIT).unwrap();
        f.host.unload("a").unwrap();
        assert_eq!(serving.join().unwrap().unwrap(), "stopped");
        f.host
            .wait_for("a", |s| *s == ModelState::Unloaded, WAIT)
            .unwrap();
    }

    #[test]
    fn serving_needs_a_loaded_model() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        let err = f.host.with_lane("a", |_m, _l| ()).unwrap_err();
        assert!(
            matches!(&err, HostError::NotLoaded { id, state } if id == "a" && *state == "unloaded"),
            "{err}"
        );
    }

    #[test]
    fn try_with_lane_refuses_a_busy_lane_instead_of_waiting() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        let host = f.host.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            host.with_lane("a", |_m, _l| {
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap()
        });
        held_rx.recv_timeout(WAIT).unwrap();
        let err = f.host.try_with_lane("a", |_m, _l| ()).unwrap_err();
        assert!(
            matches!(&err, HostError::LaneBusy(id) if id == "a"),
            "{err}"
        );
        assert_eq!(err.to_string(), "model a is busy serving another request");
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(f.host.try_with_lane("a", |_m, _l| ()).is_ok());
    }

    #[test]
    fn try_with_lane_needs_a_loaded_model() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        assert!(matches!(
            f.host.try_with_lane("a", |_m, _l| ()),
            Err(HostError::NotLoaded { .. })
        ));
    }

    #[test]
    fn serving_hands_out_the_loaded_model() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        let id = f
            .host
            .with_lane("a", |m, _l| {
                m.as_any().downcast_ref::<FakeLoaded>().unwrap().id.clone()
            })
            .unwrap();
        assert_eq!(id, "a");
    }

    #[test]
    fn a_lane_serves_one_request_at_a_time() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (host, active, peak) = (f.host.clone(), active.clone(), peak.clone());
                std::thread::spawn(move || {
                    host.with_lane("a", |_m, _l| {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(10));
                        active.fetch_sub(1, Ordering::SeqCst);
                    })
                    .unwrap()
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn loading_a_group_member_swaps_out_the_other() {
        let rt = FakeRuntime::open();
        let f = fixture(
            vec![model("a", 3.0, Some("stt")), model("b", 3.0, Some("stt"))],
            20.0,
            rt.clone(),
        );
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        f.host.load("b").unwrap();
        f.host.wait_for("b", ModelState::serves, WAIT).unwrap();
        let a = f.host.status("a").unwrap();
        assert_eq!(a.state, ModelState::Unloaded);
        assert!(!a.resident, "swapped out means no longer wished");
        assert!(f.host.status("b").unwrap().resident);
        assert_eq!(rt.drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_swap_is_admitted_against_the_memory_it_frees() {
        // 4 GiB free; `a` (3 GiB) is loaded; `b` needs 5 GiB: 4 + 3 - 1 margin = 6 fits.
        let f = fixture(
            vec![model("a", 3.0, Some("stt")), model("b", 5.0, Some("stt"))],
            4.0,
            FakeRuntime::open(),
        );
        // Load `a` first on a host with room for it.
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        f.host.load("b").unwrap();
        f.host.wait_for("b", ModelState::serves, WAIT).unwrap();
    }

    #[test]
    fn models_outside_a_group_are_left_alone() {
        let f = fixture(
            vec![model("a", 1.0, Some("stt")), model("llm", 1.0, None)],
            20.0,
            FakeRuntime::open(),
        );
        f.host.load("llm").unwrap();
        f.host.load("a").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        f.host.wait_for("llm", ModelState::serves, WAIT).unwrap();
    }

    #[test]
    fn restore_loads_residents_in_catalogue_order_and_skips_unknown_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("residency.json");
        std::fs::write(&path, r#"{"version":1,"resident":["b","gone","a"]}"#).unwrap();
        let rt = FakeRuntime::open();
        let f = fixture_in(
            dir,
            path,
            vec![
                model("a", 1.0, None),
                model("b", 1.0, None),
                model("c", 1.0, None),
            ],
            20.0,
            rt.clone(),
        );
        let logs = crate::test_support::capture({
            let host = f.host.clone();
            move || host.restore_residents()
        });
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        f.host.wait_for("b", ModelState::serves, WAIT).unwrap();
        assert_eq!(state_of(&f.host, "c"), ModelState::Unloaded);
        assert!(
            logs.contains("resident model is not in the catalogue"),
            "{logs}"
        );
        assert!(logs.contains("gone"), "{logs}");
    }

    #[test]
    fn transitions_are_published_to_subscribers() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        let rx = f.host.subscribe();
        f.host.load("a").unwrap();
        let seen: Vec<String> = (0..2)
            .map(|_| rx.recv_timeout(WAIT).unwrap().state.name().to_string())
            .collect();
        assert_eq!(seen, ["loading", "loaded"]);
    }

    #[test]
    fn transitions_leave_a_lifecycle_breadcrumb() {
        let f = fixture(vec![model("a", 1.0, None)], 20.0, FakeRuntime::open());
        let logs = crate::test_support::capture({
            let host = f.host.clone();
            move || {
                host.load("a").unwrap();
                host.wait_for("a", ModelState::serves, WAIT).unwrap();
            }
        });
        assert!(
            logs.contains("op=\"load\"") || logs.contains("op=load"),
            "{logs}"
        );
        assert!(
            logs.contains("from=\"unloaded\"") || logs.contains("from=unloaded"),
            "{logs}"
        );
    }

    #[test]
    fn loaded_estimates_are_summed_for_accounting() {
        let f = fixture(
            vec![model("a", 1.5, None), model("b", 2.0, None)],
            20.0,
            FakeRuntime::open(),
        );
        f.host.load("a").unwrap();
        f.host.load("b").unwrap();
        f.host.wait_for("a", ModelState::serves, WAIT).unwrap();
        f.host.wait_for("b", ModelState::serves, WAIT).unwrap();
        assert_eq!(f.host.loaded_gib(), 3.5);
    }

    #[test]
    fn host_errors_read_well() {
        assert_eq!(
            HostError::UnknownModel("x".into()).to_string(),
            "unknown model: x"
        );
        assert_eq!(
            HostError::Disabled("x".into()).to_string(),
            "model is disabled: x"
        );
        assert_eq!(
            HostError::NotLoaded {
                id: "x".into(),
                state: "loading"
            }
            .to_string(),
            "model x is not loaded (loading)"
        );
    }
}
