//! The tray UI's link to its daemon.
//!
//! A [`Poller`] reads the daemon's state once a second over the local API
//! and applies it to a [`Replica`]: a local copy of the daemon's observers
//! that the UI tabs render from.  When the daemon cannot be reached the
//! replica is emptied (no stale data) and, when no daemon holds the daemon
//! lock, one is started.  [`perform`] carries the operator's actions back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::auto_register::{RegistrationState, SharedRegistration};
use crate::config::{Config, SharedConfig};
use crate::daemon_api::{DaemonStatus, EditableConfig, LogsPage, ModelEntry};
use crate::daemon_client::{ClientError, DaemonClient};
use crate::job_log::JobLog;
use crate::runtime::{SessionState, WorkerObservers, RECENT_LOGS_CAP};

const TRACE_TARGET: &str = "studio_worker::daemon_link";

/// How often the poller reads the daemon.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Shortest gap between two daemon starts.  A daemon takes a few seconds to
/// take its lock and bind; starting another sooner only adds a process that
/// exits at once.
pub const SPAWN_BACKOFF: Duration = Duration::from_secs(10);

/// How long no daemon must be running before the UI starts one.  Longer
/// than the gap while a daemon restarts itself after an update (about a
/// second), so a restarting daemon is never raced.  Safe range 5..=120 s.
pub const SPAWN_GRACE: Duration = Duration::from_secs(20);

/// The UI's view of its daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Not polled yet.
    Connecting,
    /// The daemon answers.
    Connected { url: String, version: String },
    /// A daemon holds the lock but does not answer (yet).
    Starting { error: String },
    /// No daemon answers and none holds the lock.
    Unreachable { error: String, started_daemon: bool },
}

impl LinkState {
    /// Stable one-word name, for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            LinkState::Connecting => "connecting",
            LinkState::Connected { .. } => "connected",
            LinkState::Starting { .. } => "starting",
            LinkState::Unreachable { .. } => "unreachable",
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, LinkState::Connected { .. })
    }

    /// One line for the UI's status bar.
    pub fn summary(&self) -> String {
        match self {
            LinkState::Connecting => "connecting to the worker daemon…".into(),
            LinkState::Connected { url, version } => format!("daemon v{version} at {url}"),
            LinkState::Starting { .. } => {
                "the worker daemon is starting (it holds its lock but does not answer yet)…".into()
            }
            LinkState::Unreachable {
                started_daemon: true,
                ..
            } => "daemon not reachable; started one, waiting for it to answer…".into(),
            LinkState::Unreachable {
                started_daemon: false,
                ..
            } => "daemon not reachable; retrying…".into(),
        }
    }
}

/// The UI's copy of the daemon's state.  Cheap to clone.
#[derive(Clone)]
pub struct Replica {
    /// The daemon's observers, as the pages read them.
    pub observers: WorkerObservers,
    /// The daemon's operator-editable config and worker id (no secrets).
    pub cfg: SharedConfig,
    pub busy: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub registration: SharedRegistration,
    /// The last snapshot, whole.
    pub status: Arc<Mutex<Option<DaemonStatus>>>,
    pub models: Arc<Mutex<Vec<ModelEntry>>>,
    pub link: Arc<Mutex<LinkState>>,
    /// The job whose log the UI shows; the poller keeps its log fresh.
    pub selected_job: Arc<Mutex<Option<String>>>,
    pub selected_log: Arc<Mutex<Option<(String, JobLog)>>>,
    logs_seq: Arc<AtomicU64>,
}

impl Default for Replica {
    fn default() -> Self {
        Self {
            observers: WorkerObservers::default(),
            cfg: crate::config::shared(Config::default()),
            busy: Arc::default(),
            paused: Arc::default(),
            registration: crate::auto_register::shared_initial(),
            status: Arc::default(),
            models: Arc::default(),
            link: Arc::new(Mutex::new(LinkState::Connecting)),
            selected_job: Arc::default(),
            selected_log: Arc::default(),
            logs_seq: Arc::default(),
        }
    }
}

impl Replica {
    /// Whether the daemon is registered with the studio.
    pub fn registered(&self) -> bool {
        self.status.lock().as_ref().is_some_and(|s| s.registered)
    }

    /// Mirror a snapshot into the replica.
    pub fn apply_status(&self, status: DaemonStatus) {
        let o = &self.observers;
        let active: Vec<_> = status.active_jobs.iter().map(|j| j.to_current()).collect();
        *o.current_job.lock() = status
            .current_job_id
            .as_ref()
            .and_then(|id| active.iter().find(|j| &j.job_id == id).cloned());
        *o.active_jobs.lock() = active;
        *o.recent_jobs.lock() = status
            .recent_jobs
            .iter()
            .filter_map(|j| j.to_recent())
            .collect();
        *o.local_jobs.lock() = status
            .local_jobs
            .iter()
            .filter_map(|j| j.to_recent())
            .collect();
        *o.local_api_url.lock() = status.local_api_url.clone();
        *o.last_heartbeat.lock() = status.heartbeat.clone();
        *o.session_state.lock() = status.session.clone();
        *o.gpu_runtime.lock() = status.gpu_runtime.clone();
        {
            let mut cfg = self.cfg.lock();
            status.config.apply_to(&mut cfg);
            cfg.worker_id = status.worker_id.clone();
        }
        self.busy.store(status.busy, Ordering::SeqCst);
        self.paused.store(status.paused, Ordering::SeqCst);
        *self.registration.lock() = status.registration.clone();
        let with_thumbnail: Vec<String> = status
            .active_jobs
            .iter()
            .chain(&status.recent_jobs)
            .chain(&status.local_jobs)
            .filter(|j| j.has_thumbnail)
            .map(|j| j.job_id.clone())
            .collect();
        o.thumbnails
            .retain(|id| with_thumbnail.iter().any(|j| j == id));
        *self.status.lock() = Some(status);
    }

    /// Job ids whose thumbnail the daemon has and the replica lacks.
    pub fn missing_thumbnails(&self) -> Vec<String> {
        let status = self.status.lock();
        let Some(status) = status.as_ref() else {
            return Vec::new();
        };
        status
            .active_jobs
            .iter()
            .chain(&status.recent_jobs)
            .chain(&status.local_jobs)
            .filter(|j| j.has_thumbnail && !self.observers.thumbnails.contains(&j.job_id))
            .map(|j| j.job_id.clone())
            .collect()
    }

    /// The sequence number to ask new log entries after.
    pub fn logs_after(&self) -> u64 {
        self.logs_seq.load(Ordering::SeqCst)
    }

    /// Append a page of log entries; a page from a restarted daemon (its
    /// sequence number went backwards) replaces the ring.
    pub fn apply_logs(&self, page: LogsPage) {
        let mut ring = self.observers.recent_logs.lock();
        if page.seq < self.logs_seq.load(Ordering::SeqCst) {
            ring.clear();
        }
        ring.extend(page.entries);
        while ring.len() > RECENT_LOGS_CAP {
            ring.pop_front();
        }
        self.logs_seq.store(page.seq, Ordering::SeqCst);
        self.observers
            .recent_logs_seq
            .store(page.seq, Ordering::SeqCst);
    }

    /// Forget everything the daemon said, so nothing stale is shown.
    pub fn clear(&self) {
        let o = &self.observers;
        *o.current_job.lock() = None;
        o.active_jobs.lock().clear();
        o.recent_jobs.lock().clear();
        o.local_jobs.lock().clear();
        *o.local_api_url.lock() = None;
        *o.last_heartbeat.lock() = None;
        *o.session_state.lock() = SessionState::default();
        *o.gpu_runtime.lock() = None;
        o.recent_logs.lock().clear();
        o.thumbnails.clear();
        self.logs_seq.store(0, Ordering::SeqCst);
        self.busy.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        *self.registration.lock() = RegistrationState::Pristine;
        *self.status.lock() = None;
        self.models.lock().clear();
        *self.selected_log.lock() = None;
    }
}

/// Starts the daemon when none runs.
pub trait DaemonStarter: Send {
    /// Whether a daemon holds the daemon lock.
    fn is_running(&self) -> std::io::Result<bool>;
    /// Start a detached daemon; answers its pid.
    fn start(&self) -> std::io::Result<u32>;
}

/// Starts `<exe> --config <path> run`, detached, output appended to
/// `<config dir>/daemon.log`.
pub struct ProcessStarter {
    pub exe: PathBuf,
    pub config_path: PathBuf,
}

/// The file a UI-started daemon writes its output to.
pub fn daemon_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("daemon.log")
}

/// The file a tray UI started by `setup` writes its output to.
pub fn ui_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("ui.log")
}

impl DaemonStarter for ProcessStarter {
    fn is_running(&self) -> std::io::Result<bool> {
        crate::daemon_lock::is_held(&self.config_path)
    }

    // Spawns a real, detached worker process; exercised by the end-to-end
    // run, not by unit tests (which substitute a fake starter).
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn start(&self) -> std::io::Result<u32> {
        use std::process::{Command, Stdio};
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(daemon_log_path(&self.config_path))?;
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--config")
            .arg(&self.config_path)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // Its own process group / no console, so it outlives the UI.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        // Reap it when it exits, and say so.
        std::thread::spawn(move || match child.wait() {
            Ok(status) => tracing::info!(
                target: TRACE_TARGET,
                op = "daemon_spawn",
                pid,
                status = %status,
                "the daemon this UI started has exited"
            ),
            Err(e) => tracing::warn!(
                target: TRACE_TARGET,
                op = "daemon_spawn",
                pid,
                error = %e,
                "could not wait for the daemon this UI started"
            ),
        });
        Ok(pid)
    }
}

/// Polls the daemon into a [`Replica`] and starts it when absent.
pub struct Poller {
    replica: Replica,
    config_path: PathBuf,
    starter: Box<dyn DaemonStarter>,
    last_spawn: Option<Instant>,
    spawn_backoff: Duration,
    /// Since when no daemon has been running (none while one runs).
    absent_since: Option<Instant>,
    spawn_grace: Duration,
}

impl Poller {
    pub fn new(replica: Replica, config_path: PathBuf, starter: Box<dyn DaemonStarter>) -> Self {
        Self {
            replica,
            config_path,
            starter,
            last_spawn: None,
            spawn_backoff: SPAWN_BACKOFF,
            absent_since: None,
            spawn_grace: SPAWN_GRACE,
        }
    }

    /// Override [`SPAWN_GRACE`] (tests).
    pub fn with_spawn_grace(mut self, grace: Duration) -> Self {
        self.spawn_grace = grace;
        self
    }

    /// Override [`SPAWN_BACKOFF`] (tests).
    pub fn with_spawn_backoff(mut self, backoff: Duration) -> Self {
        self.spawn_backoff = backoff;
        self
    }

    /// Poll once and update the link state.
    pub fn tick(&mut self) {
        let state = match self.poll() {
            Ok(state) => {
                self.absent_since = None;
                state
            }
            Err(err) => {
                self.replica.clear();
                self.recover(err)
            }
        };
        self.set_link(state);
    }

    fn poll(&self) -> Result<LinkState, ClientError> {
        let client = DaemonClient::discover(&self.config_path)?;
        let status = client.status()?;
        let page = client.logs(self.replica.logs_after())?;
        let models = client.models()?;
        let selected = self.replica.selected_job.lock().clone();
        let selected_log = match selected {
            Some(id) => client.job_log(&id)?.map(|log| (id, log)),
            None => None,
        };
        let state = LinkState::Connected {
            url: client.url().to_string(),
            version: status.version.clone(),
        };
        self.replica.apply_status(status);
        self.replica.apply_logs(page);
        *self.replica.models.lock() = models;
        *self.replica.selected_log.lock() = selected_log;
        for id in self.replica.missing_thumbnails() {
            if let Some(png) = client.thumbnail(&id)? {
                self.replica.observers.thumbnails.insert(&id, png);
            }
        }
        Ok(state)
    }

    fn recover(&mut self, err: ClientError) -> LinkState {
        let error = err.to_string();
        match self.starter.is_running() {
            Ok(true) => {
                self.absent_since = None;
                LinkState::Starting { error }
            }
            Ok(false) => {
                let since = *self.absent_since.get_or_insert_with(Instant::now);
                if since.elapsed() < self.spawn_grace {
                    return LinkState::Unreachable {
                        error,
                        started_daemon: false,
                    };
                }
                let due = self
                    .last_spawn
                    .is_none_or(|at| at.elapsed() >= self.spawn_backoff);
                if !due {
                    return LinkState::Unreachable {
                        error,
                        started_daemon: true,
                    };
                }
                self.last_spawn = Some(Instant::now());
                match self.starter.start() {
                    Ok(pid) => {
                        tracing::info!(
                            target: TRACE_TARGET,
                            op = "daemon_spawn",
                            pid,
                            log = %daemon_log_path(&self.config_path).display(),
                            "no daemon running; started one"
                        );
                        LinkState::Unreachable {
                            error,
                            started_daemon: true,
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: TRACE_TARGET,
                            op = "daemon_spawn",
                            error = %e,
                            "no daemon running and starting one failed"
                        );
                        LinkState::Unreachable {
                            error: format!("{error}; starting a daemon failed: {e}"),
                            started_daemon: false,
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "link",
                    error = %e,
                    "could not check the daemon lock"
                );
                LinkState::Unreachable {
                    error: format!("{error}; daemon lock unreadable: {e}"),
                    started_daemon: false,
                }
            }
        }
    }

    fn set_link(&self, state: LinkState) {
        let mut link = self.replica.link.lock();
        if link.kind() != state.kind() {
            match &state {
                LinkState::Connected { url, version } => tracing::info!(
                    target: TRACE_TARGET,
                    op = "link",
                    from = link.kind(),
                    to = state.kind(),
                    url = %url,
                    version = %version,
                    "daemon reachable"
                ),
                LinkState::Starting { error } | LinkState::Unreachable { error, .. } => {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "link",
                        from = link.kind(),
                        to = state.kind(),
                        error = %error,
                        "daemon not reachable"
                    )
                }
                LinkState::Connecting => {}
            }
        }
        *link = state;
    }

    /// Poll every [`POLL_INTERVAL`] until `stop`, calling `changed` after
    /// each poll (the UI asks for a repaint).
    // The loop only sequences `tick` (unit-tested) with sleeps.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn run(mut self, stop: Arc<AtomicBool>, changed: impl Fn()) {
        while !stop.load(Ordering::SeqCst) {
            self.tick();
            changed();
            let until = Instant::now() + POLL_INTERVAL;
            while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// An operator action sent to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SetPaused(bool),
    Load(String),
    Unload(String),
    ResetRegistration,
    Shutdown,
}

/// Carry `action` to the daemon of the config at `config_path`.  Answers
/// the one line the UI shows: what happened, or why it did not.
pub fn perform(config_path: &Path, action: &Action) -> Result<String, String> {
    let outcome = DaemonClient::discover(config_path).and_then(|client| match action {
        Action::SetPaused(true) => client.set_paused(true).map(|()| "paused".to_string()),
        Action::SetPaused(false) => client.set_paused(false).map(|()| "resumed".to_string()),
        Action::Load(id) => client.load_model(id).map(|state| format!("{id}: {state}")),
        Action::Unload(id) => client
            .unload_model(id)
            .map(|state| format!("{id}: {state}")),
        Action::ResetRegistration => client
            .reset_registration()
            .map(|()| "registration reset; asking the studio again".to_string()),
        Action::Shutdown => client.shutdown().map(|()| "daemon stopping".to_string()),
    });
    match &outcome {
        Ok(done) => tracing::info!(
            target: TRACE_TARGET,
            op = "action",
            action = ?action,
            outcome = %done,
            "action carried to the daemon"
        ),
        Err(err) => tracing::warn!(
            target: TRACE_TARGET,
            op = "action",
            action = ?action,
            error = %err,
            "action refused or not delivered"
        ),
    }
    outcome.map_err(|e| e.to_string())
}

/// Send the Config page's edit to the daemon; answers what it saved.
pub fn save_config(config_path: &Path, edit: &EditableConfig) -> Result<EditableConfig, String> {
    let outcome = DaemonClient::discover(config_path).and_then(|client| client.put_config(edit));
    if let Err(err) = &outcome {
        tracing::warn!(
            target: TRACE_TARGET,
            op = "action",
            action = "save_config",
            error = %err,
            "config not saved"
        );
    }
    outcome.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::DaemonHarness;
    use std::sync::atomic::AtomicU32;

    /// A starter that records starts and reports a configurable lock.
    #[derive(Clone, Default)]
    struct FakeStarter {
        running: Arc<AtomicBool>,
        starts: Arc<AtomicU32>,
        fail: bool,
    }

    impl DaemonStarter for FakeStarter {
        fn is_running(&self) -> std::io::Result<bool> {
            Ok(self.running.load(Ordering::SeqCst))
        }
        fn start(&self) -> std::io::Result<u32> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(std::io::Error::other("no exe"));
            }
            Ok(4242)
        }
    }

    fn poller_for(config: &Path, starter: FakeStarter) -> (Poller, Replica) {
        let replica = Replica::default();
        // No grace in tests that are about spawning itself.
        let poller = Poller::new(replica.clone(), config.to_path_buf(), Box::new(starter))
            .with_spawn_grace(Duration::ZERO);
        (poller, replica)
    }

    #[test]
    fn a_reachable_daemon_fills_the_replica() {
        crate::test_support::install_job_log_capture();
        let daemon = DaemonHarness::start();
        let job_id = daemon.run_image_job();
        daemon.push_log("hello from the daemon");
        daemon.control.set_paused(true);
        let starter = FakeStarter::default();
        let (mut poller, replica) = poller_for(&daemon.config_path, starter.clone());
        *replica.selected_job.lock() = Some(job_id.clone());

        poller.tick();

        assert!(replica.link.lock().is_connected());
        assert!(replica.paused.load(Ordering::SeqCst));
        assert_eq!(replica.observers.local_jobs.lock()[0].job_id, job_id);
        assert!(replica.observers.thumbnails.contains(&job_id));
        assert_eq!(replica.models.lock().len(), 2);
        assert!(replica
            .observers
            .recent_logs
            .lock()
            .iter()
            .any(|e| e.message == "hello from the daemon"));
        let (id, log) = replica.selected_log.lock().clone().expect("selected log");
        assert_eq!(id, job_id);
        assert!(!log.lines.is_empty());
        assert_eq!(
            replica.cfg.lock().vram_threshold_gb,
            Config::default().vram_threshold_gb
        );
        assert_eq!(starter.starts.load(Ordering::SeqCst), 0);

        // A second tick only appends log entries that are new.
        daemon.push_log("second");
        poller.tick();
        let messages: Vec<_> = replica
            .observers
            .recent_logs
            .lock()
            .iter()
            .map(|e| e.message.clone())
            .collect();
        assert_eq!(messages.iter().filter(|m| *m == "second").count(), 1);
        assert_eq!(
            messages
                .iter()
                .filter(|m| *m == "hello from the daemon")
                .count(),
            1
        );
    }

    #[test]
    fn a_daemon_briefly_missing_is_not_replaced() {
        // A daemon restarting itself after an update leaves a gap of about a
        // second; starting another then would take the lock from it.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let starter = FakeStarter::default();
        let (poller, replica) = poller_for(&config, starter.clone());
        let mut poller = poller.with_spawn_grace(Duration::from_millis(200));
        poller.tick();
        poller.tick();
        assert_eq!(starter.starts.load(Ordering::SeqCst), 0, "within the grace");
        assert!(matches!(
            *replica.link.lock(),
            LinkState::Unreachable {
                started_daemon: false,
                ..
            }
        ));
        std::thread::sleep(Duration::from_millis(250));
        poller.tick();
        assert_eq!(
            starter.starts.load(Ordering::SeqCst),
            1,
            "missing for longer than the grace"
        );
    }

    #[test]
    fn a_daemon_that_comes_back_resets_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let starter = FakeStarter::default();
        let (poller, _replica) = poller_for(&config, starter.clone());
        let mut poller = poller.with_spawn_grace(Duration::from_millis(200));
        poller.tick();
        std::thread::sleep(Duration::from_millis(150));
        starter.running.store(true, Ordering::SeqCst);
        poller.tick();
        starter.running.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        poller.tick();
        assert_eq!(
            starter.starts.load(Ordering::SeqCst),
            0,
            "the absence restarted"
        );
    }

    #[test]
    fn no_daemon_empties_the_replica_and_starts_one_with_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let starter = FakeStarter::default();
        let (poller, replica) = poller_for(&config, starter.clone());
        let mut poller = poller.with_spawn_backoff(Duration::from_secs(60));
        replica
            .observers
            .local_jobs
            .lock()
            .push_front(crate::runtime::RecentJob {
                job_id: "stale".into(),
                kind: crate::types::TaskKind::Image,
                model: "m".into(),
                prompt: String::new(),
                outcome: crate::runtime::JobOutcome::Completed,
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                source: crate::runtime::JobSource::Local,
            });

        let logs = crate::test_support::capture({
            let replica = replica.clone();
            move || {
                poller.tick();
                poller.tick();
                assert!(matches!(
                    *replica.link.lock(),
                    LinkState::Unreachable {
                        started_daemon: true,
                        ..
                    }
                ));
            }
        });

        assert_eq!(starter.starts.load(Ordering::SeqCst), 1, "backoff holds");
        assert!(
            replica.observers.local_jobs.lock().is_empty(),
            "no stale data"
        );
        assert!(logs.contains("op=\"daemon_spawn\""), "{logs}");
        assert!(logs.contains("pid=4242"), "{logs}");
        assert!(logs.contains("op=\"link\""), "{logs}");
        assert_eq!(
            logs.matches("to=\"unreachable\"").count(),
            1,
            "a link change is logged once: {logs}"
        );
    }

    #[test]
    fn a_held_lock_means_starting_and_no_second_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let starter = FakeStarter::default();
        starter.running.store(true, Ordering::SeqCst);
        let (mut poller, replica) = poller_for(&dir.path().join("config.toml"), starter.clone());
        poller.tick();
        assert_eq!(replica.link.lock().kind(), "starting");
        assert_eq!(starter.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failed_start_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let starter = FakeStarter {
            fail: true,
            ..Default::default()
        };
        let (mut poller, replica) = poller_for(&dir.path().join("config.toml"), starter);
        poller.tick();
        let link = replica.link.lock().clone();
        assert!(
            matches!(&link, LinkState::Unreachable { error, started_daemon: false } if error.contains("starting a daemon failed")),
            "{link:?}"
        );
    }

    #[test]
    fn the_process_starter_reads_the_real_lock() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let starter = ProcessStarter {
            exe: PathBuf::from("studio-worker"),
            config_path: config.clone(),
        };
        assert!(!starter.is_running().unwrap());
        let _lock = crate::daemon_lock::acquire_with(&config, 1, Duration::from_millis(1)).unwrap();
        assert!(starter.is_running().unwrap());
        assert_eq!(daemon_log_path(&config), dir.path().join("daemon.log"));
        assert_eq!(ui_log_path(&config), dir.path().join("ui.log"));
    }

    #[test]
    fn a_restarted_daemon_replaces_the_log_ring() {
        let replica = Replica::default();
        let entry = |m: &str| crate::types::LogEntry {
            ts: "t".into(),
            level: "info".into(),
            category: "c".into(),
            message: m.into(),
            job_id: None,
        };
        replica.apply_logs(LogsPage {
            entries: vec![entry("old-1"), entry("old-2")],
            seq: 50,
        });
        replica.apply_logs(LogsPage {
            entries: vec![entry("new-1")],
            seq: 1,
        });
        let ring = replica.observers.recent_logs.lock();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring[0].message, "new-1");
        assert_eq!(replica.logs_after(), 1);
    }

    #[test]
    fn link_summaries_name_the_situation() {
        let connected = LinkState::Connected {
            url: "http://127.0.0.1:1".into(),
            version: "1.2.3".into(),
        };
        assert!(connected.summary().contains("v1.2.3"));
        assert!(LinkState::Connecting.summary().contains("connecting"));
        assert!(LinkState::Starting { error: "e".into() }
            .summary()
            .contains("starting"));
        assert!(LinkState::Unreachable {
            error: "e".into(),
            started_daemon: true
        }
        .summary()
        .contains("started one"));
        assert!(LinkState::Unreachable {
            error: "e".into(),
            started_daemon: false
        }
        .summary()
        .contains("retrying"));
    }

    #[test]
    fn actions_reach_the_daemon_and_refusals_come_back() {
        let daemon = DaemonHarness::start();
        let path = daemon.config_path.clone();
        assert_eq!(perform(&path, &Action::SetPaused(true)).unwrap(), "paused");
        assert!(daemon.control.paused.load(Ordering::SeqCst));
        assert_eq!(
            perform(&path, &Action::SetPaused(false)).unwrap(),
            "resumed"
        );
        assert!(perform(&path, &Action::Load("chat".into()))
            .unwrap()
            .starts_with("chat: "));
        daemon.wait_state("chat", "loaded");
        assert!(perform(&path, &Action::Unload("chat".into())).is_ok());
        let refused = perform(&path, &Action::ResetRegistration).unwrap_err();
        assert!(refused.contains("not_rejected"), "{refused}");

        let mut edit = EditableConfig::from_config(&Config::default());
        edit.vram_threshold_gb = 4.0;
        assert_eq!(save_config(&path, &edit).unwrap().vram_threshold_gb, 4.0);
        edit.api_base_url = "bad".into();
        assert!(save_config(&path, &edit)
            .unwrap_err()
            .contains("invalid_config"));

        assert_eq!(
            perform(&path, &Action::Shutdown).unwrap(),
            "daemon stopping"
        );
        assert!(daemon.control.stop.load(Ordering::SeqCst));
    }

    #[test]
    fn an_action_without_a_daemon_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let err = perform(&dir.path().join("config.toml"), &Action::SetPaused(true)).unwrap_err();
        assert!(err.contains("not reachable"), "{err}");
    }
}
