//! Per-job log capture.
//!
//! A job runs inside a tracing span named `job` carrying a `job_id` field
//! ([`job_span`]).  [`JobLogLayer`] copies every event emitted inside such a
//! span, or carrying a `job_id` field itself, into that job's log in a
//! bounded [`JobLogStore`].  Engines need not know about jobs: their events
//! land in the right log because they run inside the job's span.
//!
//! The daemon installs the layer next to the stderr formatter (`main.rs`),
//! so the job log sees the same, `RUST_LOG`-filtered, events.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Tracing target for job bookkeeping (started / finished / thumbnail).
pub const TRACE_TARGET: &str = "studio_worker::job";

/// Logs of this many most recent jobs are kept.  Covers both 50-job rings
/// plus the running jobs, with slack.
pub const JOB_LOG_JOBS_CAP: usize = 128;

/// Lines kept per job; older lines are dropped (and counted).
pub const JOB_LOG_LINES_CAP: usize = 400;

/// A longer message is clipped to this many characters.
pub const JOB_LOG_LINE_CHARS: usize = 2_000;

/// One captured log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobLogLine {
    pub ts: DateTime<Utc>,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// A job's captured log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobLog {
    pub lines: Vec<JobLogLine>,
    /// Lines dropped from the front because the job logged more than
    /// [`JOB_LOG_LINES_CAP`].
    pub dropped: u64,
}

#[derive(Default)]
struct Entry {
    job_id: String,
    lines: VecDeque<JobLogLine>,
    dropped: u64,
}

/// Bounded store of per-job logs, oldest job evicted first.  Cheap to clone.
#[derive(Clone, Default)]
pub struct JobLogStore {
    inner: Arc<Mutex<VecDeque<Entry>>>,
}

impl JobLogStore {
    /// Append `line` to `job_id`'s log.
    pub fn push(&self, job_id: &str, mut line: JobLogLine) {
        if line.message.chars().count() > JOB_LOG_LINE_CHARS {
            line.message = line.message.chars().take(JOB_LOG_LINE_CHARS).collect();
            line.message.push('…');
        }
        let mut jobs = self.inner.lock();
        let index = match jobs.iter().rposition(|e| e.job_id == job_id) {
            Some(index) => index,
            None => {
                jobs.push_back(Entry {
                    job_id: job_id.to_string(),
                    ..Default::default()
                });
                if jobs.len() > JOB_LOG_JOBS_CAP {
                    jobs.pop_front();
                }
                jobs.len() - 1
            }
        };
        let entry = &mut jobs[index];
        entry.lines.push_back(line);
        if entry.lines.len() > JOB_LOG_LINES_CAP {
            entry.lines.pop_front();
            entry.dropped += 1;
        }
    }

    /// The log of `job_id`, if any line was captured for it.
    pub fn get(&self, job_id: &str) -> Option<JobLog> {
        let jobs = self.inner.lock();
        jobs.iter().rfind(|e| e.job_id == job_id).map(|e| JobLog {
            lines: e.lines.iter().cloned().collect(),
            dropped: e.dropped,
        })
    }

    /// Number of jobs with a log.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// True when no job has a log.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The process-wide store the installed [`JobLogLayer`] writes to.
pub fn global() -> &'static JobLogStore {
    static STORE: OnceLock<JobLogStore> = OnceLock::new();
    STORE.get_or_init(JobLogStore::default)
}

/// The span a job runs in.  Every event inside it lands in the job's log.
pub fn job_span(job_id: &str) -> tracing::Span {
    tracing::info_span!(target: TRACE_TARGET, "job", job_id = %job_id)
}

/// The job id a span carries, kept in the span's extensions.
struct SpanJobId(String);

/// Keep a new span's `job_id` in its extensions, once (both layers call
/// this; an extension type may only be inserted once per span).
fn remember_job_id<S>(attrs: &Attributes<'_>, id: &Id, ctx: &Context<'_, S>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut fields = Fields::default();
    attrs.record(&mut fields);
    if let (Some(job_id), Some(span)) = (fields.job_id, ctx.span(id)) {
        let mut extensions = span.extensions_mut();
        if extensions.get_mut::<SpanJobId>().is_none() {
            extensions.insert(SpanJobId(job_id));
        }
    }
}

/// Collects the `job_id` field and renders the rest as `message k=v …`.
#[derive(Default)]
struct Fields {
    job_id: Option<String>,
    message: String,
    rest: String,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "job_id" => self.job_id = Some(value.to_string()),
            "message" => self.message.push_str(value),
            name => {
                let _ = write!(self.rest, " {name}={value:?}");
            }
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "job_id" => self.job_id = Some(format!("{value:?}")),
            "message" => {
                let _ = write!(self.message, "{value:?}");
            }
            name => {
                let _ = write!(self.rest, " {name}={value:?}");
            }
        }
    }
}

impl Fields {
    fn rendered(self) -> String {
        format!("{}{}", self.message, self.rest)
    }
}

/// The worker log ring the Logs tab shows: the entries and the sequence
/// number of the newest (see `runtime::recent_logs_after`).
#[derive(Clone, Default)]
pub struct WorkerLogRing {
    pub entries: Arc<Mutex<VecDeque<crate::types::LogEntry>>>,
    pub seq: Arc<std::sync::atomic::AtomicU64>,
}

impl WorkerLogRing {
    /// Append `entry`, keeping the newest [`crate::runtime::RECENT_LOGS_CAP`].
    pub fn push(&self, entry: crate::types::LogEntry) {
        let mut ring = self.entries.lock();
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ring.push_back(entry);
        while ring.len() > crate::runtime::RECENT_LOGS_CAP {
            ring.pop_front();
        }
    }
}

/// The process-wide worker log ring the installed [`WorkerLogLayer`]
/// writes to; the daemon's observers share it.
pub fn global_worker_log() -> &'static WorkerLogRing {
    static RING: OnceLock<WorkerLogRing> = OnceLock::new();
    RING.get_or_init(WorkerLogRing::default)
}

/// Copies the worker's own info / warn / error events into a
/// [`WorkerLogRing`], so the Logs tab shows everything the daemon does,
/// not only the studio session's breadcrumbs.  Events on the bare
/// `studio_worker` target are skipped: `runtime::push_log` writes those
/// into the ring itself.
pub struct WorkerLogLayer {
    ring: WorkerLogRing,
}

impl WorkerLogLayer {
    pub fn new(ring: WorkerLogRing) -> Self {
        Self { ring }
    }

    /// A layer writing to [`global_worker_log`].
    pub fn global() -> Self {
        Self::new(global_worker_log().clone())
    }
}

impl<S> Layer<S> for WorkerLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        remember_job_id(attrs, id, &ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let target = meta.target();
        if *meta.level() > tracing::Level::INFO
            || target == "studio_worker"
            || !target.starts_with("studio_worker")
        {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let job_id = fields.job_id.take().or_else(|| {
            ctx.event_scope(event)?
                .find_map(|span| span.extensions().get::<SpanJobId>().map(|j| j.0.clone()))
        });
        let mut message = fields.rendered();
        if message.chars().count() > JOB_LOG_LINE_CHARS {
            message = message.chars().take(JOB_LOG_LINE_CHARS).collect();
            message.push('…');
        }
        self.ring.push(crate::types::LogEntry {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            level: meta.level().as_str().to_ascii_lowercase(),
            category: target
                .strip_prefix("studio_worker::")
                .unwrap_or(target)
                .to_string(),
            message,
            job_id,
        });
    }
}

/// Copies job-scoped events into a [`JobLogStore`].
pub struct JobLogLayer {
    store: JobLogStore,
}

impl JobLogLayer {
    /// A layer writing to `store`.
    pub fn new(store: JobLogStore) -> Self {
        Self { store }
    }

    /// A layer writing to the process-wide [`global`] store.
    pub fn global() -> Self {
        Self::new(global().clone())
    }
}

impl<S> Layer<S> for JobLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        remember_job_id(attrs, id, &ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let job_id = fields.job_id.take().or_else(|| {
            ctx.event_scope(event)?
                .find_map(|span| span.extensions().get::<SpanJobId>().map(|j| j.0.clone()))
        });
        let Some(job_id) = job_id else { return };
        let meta = event.metadata();
        self.store.push(
            &job_id,
            JobLogLine {
                ts: Utc::now(),
                level: meta.level().as_str().to_ascii_lowercase(),
                target: meta.target().to_string(),
                message: fields.rendered(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    fn line(message: &str) -> JobLogLine {
        JobLogLine {
            ts: Utc::now(),
            level: "info".into(),
            target: "t".into(),
            message: message.into(),
        }
    }

    /// Run `f` with a subscriber whose only layer captures into `store`.
    fn with_layer(store: &JobLogStore, f: impl FnOnce()) {
        let subscriber = tracing_subscriber::registry().with(JobLogLayer::new(store.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            f();
        });
    }

    #[test]
    fn events_inside_a_job_span_land_in_that_jobs_log() {
        let store = JobLogStore::default();
        with_layer(&store, || {
            let span = job_span("job-a");
            let _entered = span.enter();
            tracing::info!(target: "studio_worker::engine", op = "download", bytes = 42, "fetching weights");
        });
        let log = store.get("job-a").expect("job-a has a log");
        assert_eq!(log.lines.len(), 1);
        assert_eq!(log.lines[0].level, "info");
        assert_eq!(log.lines[0].target, "studio_worker::engine");
        assert_eq!(
            log.lines[0].message,
            "fetching weights op=\"download\" bytes=42"
        );
    }

    #[test]
    fn nested_spans_resolve_to_the_enclosing_job() {
        let store = JobLogStore::default();
        with_layer(&store, || {
            let job = job_span("job-b");
            let _job = job.enter();
            let inner = tracing::info_span!("inner-work");
            let _inner = inner.enter();
            tracing::warn!("deep inside");
        });
        let log = store.get("job-b").expect("job-b has a log");
        assert_eq!(log.lines[0].level, "warn");
        assert_eq!(log.lines[0].message, "deep inside");
    }

    #[test]
    fn an_event_carrying_a_job_id_field_lands_in_that_jobs_log() {
        let store = JobLogStore::default();
        with_layer(&store, || {
            tracing::info!(job_id = "job-c", "[ws] accepted");
        });
        let log = store.get("job-c").expect("job-c has a log");
        assert_eq!(log.lines[0].message, "[ws] accepted");
    }

    #[test]
    fn an_explicit_job_id_field_wins_over_the_enclosing_span() {
        let store = JobLogStore::default();
        with_layer(&store, || {
            let span = job_span("outer");
            let _entered = span.enter();
            tracing::info!(job_id = "other", "for the other job");
        });
        assert!(store.get("outer").is_none());
        assert_eq!(store.get("other").unwrap().lines.len(), 1);
    }

    #[test]
    fn events_outside_any_job_are_ignored() {
        let store = JobLogStore::default();
        with_layer(&store, || {
            tracing::info!("background chatter");
        });
        assert!(store.is_empty());
    }

    #[test]
    fn the_worker_log_takes_the_workers_own_info_and_up() {
        let ring = WorkerLogRing::default();
        let subscriber = tracing_subscriber::registry().with(WorkerLogLayer::new(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let span = job_span("job-w");
            let _entered = span.enter();
            tracing::info!(target: "studio_worker::host", op = "load", "model loaded");
            tracing::warn!(target: "studio_worker::local_api", "denied");
            tracing::debug!(target: "studio_worker::host", "too chatty");
            tracing::info!(target: "studio_worker", "[ws] pushed by push_log");
            tracing::info!(target: "hyper::client", "not ours");
        });
        let entries: Vec<_> = ring.entries.lock().iter().cloned().collect();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].category, "host");
        assert_eq!(entries[0].message, "model loaded op=\"load\"");
        assert_eq!(entries[0].job_id.as_deref(), Some("job-w"));
        assert_eq!(entries[1].level, "warn");
        assert_eq!(ring.seq.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn both_layers_together_share_the_span_job_id() {
        let store = JobLogStore::default();
        let ring = WorkerLogRing::default();
        let subscriber = tracing_subscriber::registry()
            .with(JobLogLayer::new(store.clone()))
            .with(WorkerLogLayer::new(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let span = job_span("job-both");
            let _entered = span.enter();
            tracing::info!(target: "studio_worker::host", "loaded");
        });
        assert_eq!(store.get("job-both").unwrap().lines.len(), 1);
        assert_eq!(ring.entries.lock()[0].job_id.as_deref(), Some("job-both"));
    }

    #[test]
    fn the_worker_log_keeps_only_the_newest_entries() {
        let ring = WorkerLogRing::default();
        for i in 0..(crate::runtime::RECENT_LOGS_CAP + 2) {
            ring.push(crate::types::LogEntry {
                ts: String::new(),
                level: "info".into(),
                category: "c".into(),
                message: format!("m{i}"),
                job_id: None,
            });
        }
        assert_eq!(ring.entries.lock().len(), crate::runtime::RECENT_LOGS_CAP);
        assert_eq!(ring.entries.lock()[0].message, "m2");
    }

    #[test]
    fn a_job_keeps_only_its_newest_lines_and_counts_the_drop() {
        let store = JobLogStore::default();
        for i in 0..(JOB_LOG_LINES_CAP + 3) {
            store.push("busy", line(&format!("line {i}")));
        }
        let log = store.get("busy").unwrap();
        assert_eq!(log.lines.len(), JOB_LOG_LINES_CAP);
        assert_eq!(log.dropped, 3);
        assert_eq!(log.lines[0].message, "line 3");
    }

    #[test]
    fn only_the_most_recent_jobs_keep_a_log() {
        let store = JobLogStore::default();
        for i in 0..(JOB_LOG_JOBS_CAP + 2) {
            store.push(&format!("job-{i}"), line("x"));
        }
        assert_eq!(store.len(), JOB_LOG_JOBS_CAP);
        assert!(store.get("job-0").is_none());
        assert!(store.get("job-1").is_none());
        assert!(store
            .get(&format!("job-{}", JOB_LOG_JOBS_CAP + 1))
            .is_some());
    }

    #[test]
    fn a_long_message_is_clipped() {
        let store = JobLogStore::default();
        store.push("long", line(&"x".repeat(JOB_LOG_LINE_CHARS + 50)));
        let message = &store.get("long").unwrap().lines[0].message;
        assert_eq!(message.chars().count(), JOB_LOG_LINE_CHARS + 1);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn a_job_log_round_trips_through_json() {
        let log = JobLog {
            lines: vec![line("hello")],
            dropped: 2,
        };
        let json = serde_json::to_value(&log).unwrap();
        assert_eq!(json["dropped"], 2);
        assert_eq!(json["lines"][0]["message"], "hello");
        let back: JobLog = serde_json::from_value(json).unwrap();
        assert_eq!(back, log);
    }

    #[test]
    fn the_global_store_is_one_instance() {
        global().push("global-probe", line("g"));
        assert!(JobLogLayer::global().store.get("global-probe").is_some());
    }
}
