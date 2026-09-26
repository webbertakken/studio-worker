//! Shared test-only helpers.
//!
//! Exposed unconditionally (marked `#[doc(hidden)]` from `lib.rs`) so
//! both library unit tests and integration tests can share a single
//! implementation.  The module is a few dozen lines and unused code
//! gets eliminated by LTO in release builds.
//!
//! `capture` installs **one** process-global `tracing-subscriber` the
//! first time it's called and routes every formatted event into a
//! thread-local buffer.  Each invocation spawns a fresh OS thread,
//! clears that thread's buffer, runs the closure, and returns its
//! contents.  Two side benefits fall out of the spawn:
//!
//! 1. `#[tokio::test]` cases that hit `reqwest::blocking` (which
//!    panics when called from inside a tokio runtime) work without
//!    extra ceremony.
//! 2. Each capture gets a brand-new thread — cargo's test runner can
//!    reuse worker threads, but our captures never share a buffer.
//!
//! The previous pattern installed a fresh `with_default` subscriber on
//! every call, which interacted badly with `tracing`'s callsite
//! Interest cache and produced empty captures under load.  See
//! `LESSONS_LEARNED.md` for the history.

use std::cell::RefCell;
use std::io;
use std::sync::OnceLock;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::Layer as _;

thread_local! {
    /// Per-thread sink that backs every formatted tracing event.
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct ThreadLocalWriter;

impl io::Write for ThreadLocalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        BUF.with(|buf| buf.borrow_mut().extend_from_slice(bytes));
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ThreadLocalMakeWriter;

impl<'a> MakeWriter<'a> for ThreadLocalMakeWriter {
    type Writer = ThreadLocalWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ThreadLocalWriter
    }
}

static GLOBAL_INSTALLED: OnceLock<()> = OnceLock::new();

fn install_once() {
    GLOBAL_INSTALLED.get_or_init(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(ThreadLocalMakeWriter)
            .with_target(true)
            .with_ansi(false)
            .without_time()
            .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
        // try_init() is a no-op if a global subscriber is already
        // installed (e.g. by another test crate's test_support helper
        // when several integration suites link the same lib).  We
        // tolerate that because the existing subscriber will also
        // route through our thread-local writer once installed.
        // The job-log layer writes to the process-wide store, so tests can
        // assert what a job logged (see `install_job_log_capture`).
        let job_logs = crate::job_log::JobLogLayer::global()
            .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
        let _ = tracing_subscriber::registry()
            .with(layer)
            .with(job_logs)
            .try_init();
    });
}

/// Run `f` on a freshly spawned OS thread and return everything it
/// emitted via `tracing` as formatted log output.
///
/// Events emitted on threads other than the spawned capture thread
/// are not captured (they land in those threads' own thread-local
/// buffers).  All call sites in this codebase pass closures that emit
/// events synchronously on the spawned thread, so this is the correct
/// trade-off.
pub fn capture<F: FnOnce() + Send + 'static>(f: F) -> String {
    install_once();
    // Re-evaluate every registered callsite against the now-installed
    // global subscriber before running the closure.  A callsite first
    // hit by a *parallel* test in the narrow window around the
    // one-time subscriber install can have its Interest cached as
    // `never`, which then silently drops the very event we're trying
    // to capture — an empty buffer, order-dependent flake (see
    // LESSONS_LEARNED).  `rebuild_interest_cache()` is idempotent and
    // cheap, so calling it per capture closes the race for every
    // caller, not only the one that wins the install.
    tracing::callsite::rebuild_interest_cache();
    std::thread::spawn(move || {
        BUF.with(|b| b.borrow_mut().clear());
        f();
        BUF.with(|b| String::from_utf8(b.borrow().clone()).expect("tracing output should be UTF-8"))
    })
    .join()
    .expect("capture thread panicked")
}

/// Route job-scoped events into [`crate::job_log::global`] for the rest of
/// the process, so a test can read back what a job logged.
pub fn install_job_log_capture() {
    install_once();
    tracing::callsite::rebuild_interest_cache();
}

/// A device-memory probe that always reports `free` GiB (of 24 total).
pub struct FixedProbe(pub f32);

impl crate::admission::MemoryProbe for FixedProbe {
    fn free_gib(&self) -> Option<f32> {
        Some(self.0)
    }
    fn total_gib(&self) -> Option<f32> {
        Some(24.0)
    }
}

/// A loaded model that only knows its id.
pub struct TestLoaded {
    pub id: String,
}

impl crate::host::LoadedModel for TestLoaded {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_chat(&self) -> Option<&dyn crate::host::ChatModel> {
        Some(self)
    }

    fn as_stream(&self) -> Option<&dyn crate::host::StreamingModel> {
        Some(self)
    }
}

/// Streams one word (`w1`, `w2`, ...) per 100 ms chunk.
impl crate::host::StreamingModel for TestLoaded {
    fn open(
        &self,
    ) -> anyhow::Result<Box<dyn crate::stt_stream::session::StreamingTranscriber + '_>> {
        Ok(Box::new(WordPerChunk(0)))
    }
}

/// A streaming transcriber that says `wN` for every chunk it hears.
pub struct WordPerChunk(pub usize);

impl crate::stt_stream::session::StreamingTranscriber for WordPerChunk {
    fn chunk_samples(&self) -> usize {
        1600
    }
    fn step(&mut self, chunk: &[f32]) -> anyhow::Result<String> {
        if chunk.iter().all(|s| *s == 0.0) {
            return Ok(String::new());
        }
        self.0 += 1;
        Ok(format!("\u{2581}w{}", self.0))
    }
    fn reset(&mut self) {
        self.0 = 0;
    }
}

/// Echoes the last message as `resident:<text>`, plus the kwargs it got,
/// so tests can tell a resident answer from a transient one.
impl crate::host::ChatModel for TestLoaded {
    fn chat(
        &self,
        params: crate::types::LlmParams,
        _cancelled: &dyn Fn() -> bool,
        on_piece: &mut dyn FnMut(&str),
    ) -> anyhow::Result<serde_json::Value> {
        let last = params
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        // Streams `resident:` then the last message, word by word.
        on_piece("resident:");
        for (i, word) in last.split(' ').enumerate() {
            on_piece(&if i == 0 {
                word.to_string()
            } else {
                format!(" {word}")
            });
        }
        Ok(serde_json::json!({
            "object": "chat.completion",
            "model": self.id,
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": format!("resident:{last}") }, "finish_reason": "stop" }],
            "kwargs": params.chat_template_kwargs,
            "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 },
        }))
    }

    /// One token per character.
    fn tokenize(&self, text: &str, _add_special: bool) -> anyhow::Result<Vec<i32>> {
        Ok(text.chars().map(|c| c as i32).collect())
    }
}

/// A model runtime whose loads succeed at once.
pub struct InstantRuntime;

impl crate::host::ModelRuntime for InstantRuntime {
    fn load(
        &self,
        model: &crate::catalog::CatalogModel,
    ) -> anyhow::Result<std::sync::Arc<dyn crate::host::LoadedModel>> {
        Ok(std::sync::Arc::new(TestLoaded {
            id: model.id.clone(),
        }))
    }
}

/// A daemon's local API on an ephemeral loopback port, with daemon control
/// attached and a discovery file in a temporary config directory.  Its
/// catalogue holds a synthetic image model (`img`) and a chat model
/// (`chat`) that loads instantly.
pub struct DaemonHarness {
    pub url: String,
    pub config_path: std::path::PathBuf,
    pub control: crate::control::DaemonControl,
    pub observers: crate::runtime::WorkerObservers,
    pub host: crate::host::ModelHost,
    pub catalog: std::sync::Arc<parking_lot::Mutex<crate::catalog::Catalog>>,
    engine: std::sync::Arc<dyn crate::engine::Engine>,
    _dir: tempfile::TempDir,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Token the [`DaemonHarness`] serves with.
pub const HARNESS_TOKEN: &str = "harness-token-0123456789abcdef";

fn harness_model(id: &str, kind: crate::types::TaskKind) -> crate::catalog::CatalogModel {
    use crate::types::{ModelCliDefaults, ModelEngine, ModelSource, TaskKind};
    crate::catalog::CatalogModel {
        id: id.into(),
        display_name: id.into(),
        kind,
        vram_gb_estimate: 1.0,
        description: None,
        source: ModelSource {
            engine: if kind == TaskKind::Llm {
                ModelEngine::LlamaCpp
            } else {
                ModelEngine::Synthetic
            },
            files: vec![],
            cli_defaults: ModelCliDefaults {
                cfg_scale: 1.0,
                steps: 4,
                width: 64,
                height: 64,
                ..Default::default()
            },
        },
        enabled: true,
        origin: "local".into(),
        exclusive_group: None,
    }
}

impl DaemonHarness {
    pub fn start() -> Self {
        use std::sync::Arc;
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let control = crate::control::DaemonControl::new(
            crate::config::shared(crate::config::Config::default()),
            config_path.clone(),
            24.0,
        );
        let catalog = Arc::new(parking_lot::Mutex::new(crate::catalog::Catalog {
            models: vec![
                harness_model("chat", crate::types::TaskKind::Llm),
                harness_model("img", crate::types::TaskKind::Image),
            ],
            ..Default::default()
        }));
        let host = crate::host::ModelHost::new(
            catalog.clone(),
            Arc::new(InstantRuntime),
            Arc::new(FixedProbe(20.0)),
            crate::residency::Residency::load_for_serving(None),
        );
        let observers = crate::runtime::WorkerObservers::default();
        let engine: Arc<dyn crate::engine::Engine> =
            Arc::new(crate::engine::SyntheticEngine::new());
        let api = crate::local_api::LocalApi::bind(
            "127.0.0.1:0",
            engine.clone(),
            catalog.clone(),
            None,
            observers.clone(),
            HARNESS_TOKEN.to_string(),
            crate::job_gate::JobGate::new(),
            None,
            crate::local_api::ModelServices::new(host.clone()),
        )
        .expect("bind")
        .with_control(control.clone());
        let url = api.url();
        *observers.local_api_url.lock() = Some(url.clone());
        let discovery = crate::config::local_api_discovery_path_for(&config_path).expect("path");
        crate::local_api::write_discovery_file(&discovery, &url, HARNESS_TOKEN).expect("discovery");
        let stop = control.stop.clone();
        let handle = std::thread::spawn(move || api.serve(&stop));
        Self {
            url,
            config_path,
            control,
            observers,
            host,
            catalog,
            engine,
            _dir: dir,
            handle: Some(handle),
        }
    }

    pub fn client(&self) -> crate::daemon_client::DaemonClient {
        crate::daemon_client::DaemonClient::new(&self.url, HARNESS_TOKEN).expect("client")
    }

    /// Block until model `id` reports `state` (by name).
    pub fn wait_state(&self, id: &str, state: &str) {
        self.host
            .wait_for(id, |s| s.name() == state, std::time::Duration::from_secs(5))
            .unwrap_or_else(|| panic!("{id} never reached {state}"));
    }

    /// Run one local image job; answers its id.
    pub fn run_image_job(&self) -> String {
        let catalog = self.catalog.lock().clone();
        let req = crate::local::LocalImageRequest {
            prompt: "a harness fox".into(),
            model: Some("img".into()),
            ..Default::default()
        };
        crate::local::run_image(self.engine.as_ref(), &catalog, &self.observers, &req)
            .expect("image job");
        self.observers.local_jobs.lock()[0].job_id.clone()
    }

    /// Push one worker log entry.
    pub fn push_log(&self, message: &str) {
        let queue = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        crate::runtime::push_log_with_observers(
            &queue,
            Some(&self.observers),
            "info",
            "test",
            message,
            None,
        );
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        self.control
            .stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_collects_events_emitted_inside_the_closure() {
        let out = capture(|| {
            tracing::info!(target: "studio_worker::test_support_demo", marker = "alpha", "hello");
        });
        assert!(out.contains("INFO"), "missing INFO level: {out:?}");
        assert!(
            out.contains("studio_worker::test_support_demo"),
            "missing target: {out:?}"
        );
        assert!(out.contains("marker=\"alpha\""), "missing field: {out:?}");
        assert!(out.contains("hello"), "missing message: {out:?}");
    }

    #[test]
    fn capture_isolates_between_invocations() {
        // Each call spawns its own thread, so even back-to-back calls
        // on the same test cannot bleed into each other.
        let first = capture(|| tracing::info!("first message"));
        let second = capture(|| tracing::info!("second message"));
        assert!(first.contains("first message") && !first.contains("second message"));
        assert!(second.contains("second message") && !second.contains("first message"));
    }

    #[test]
    fn capture_isolates_between_threads() {
        // A sibling thread emitting events while we capture must not
        // pollute the captured buffer.
        let handle = std::thread::spawn(|| {
            for _ in 0..50 {
                tracing::info!("sibling thread noise");
            }
        });
        let out = capture(|| {
            tracing::info!("primary capture message");
        });
        handle.join().unwrap();
        assert!(out.contains("primary capture message"));
        assert!(
            !out.contains("sibling thread noise"),
            "cross-thread leak: {out:?}"
        );
    }

    #[test]
    fn capture_works_inside_a_multi_thread_tokio_runtime() {
        // Mirrors the `#[tokio::test]` cases in tests/http_errors.rs:
        // closures that need a non-tokio thread (e.g. reqwest::blocking)
        // must Just Work via `capture` without manual `detached(...)`
        // wrapping.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(async {
            capture(|| {
                tracing::info!("emitted from spawned capture thread");
            })
        });
        assert!(out.contains("emitted from spawned capture thread"));
    }
}
