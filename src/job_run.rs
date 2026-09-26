//! One job's bookkeeping, shared by every job path (studio offers, local
//! transient jobs, lane requests, streaming sessions).
//!
//! [`JobRun::begin`] lists the job as running, opens its log span and logs
//! "job started"; [`JobRun::finish`] logs "job finished" and records the job
//! in its ring.  Dropping a run without finishing it (an early return or a
//! panic) still takes it off the running list.

use std::time::Instant;

use chrono::Utc;

use crate::job_log::{job_span, TRACE_TARGET};
use crate::runtime::{
    record_local_job, record_recent_job, CurrentJob, JobOutcome, JobSource, RecentJob,
    WorkerObservers,
};
use crate::types::TaskResult;

/// A running job.  See the module docs.
pub struct JobRun {
    observers: WorkerObservers,
    job: CurrentJob,
    span: tracing::Span,
    started: Instant,
}

impl JobRun {
    /// List `job` as running and log its start inside its span.
    pub fn begin(observers: &WorkerObservers, job: CurrentJob) -> Self {
        let span = job_span(&job.job_id);
        span.in_scope(|| {
            tracing::info!(
                target: TRACE_TARGET,
                op = "job",
                source = job.source.as_str(),
                kind = job.kind.as_str(),
                model = %job.model,
                "job started"
            );
        });
        observers.active_jobs.lock().push(job.clone());
        Self {
            observers: observers.clone(),
            job,
            span,
            started: Instant::now(),
        }
    }

    /// The job's id.
    pub fn job_id(&self) -> &str {
        &self.job.job_id
    }

    /// The span to run the job's work in; its events land in the job log.
    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    /// Replace the job's prompt preview, e.g. with a stream's final text.
    pub fn set_prompt(&mut self, prompt: &str) {
        self.job.prompt = crate::runtime::truncate_prompt(prompt);
    }

    /// Keep a thumbnail when `result` is an image.  A thumbnail that cannot
    /// be made is logged in the job's log; the job is unaffected.
    pub fn keep_thumbnail(&self, result: &TaskResult) {
        self.thumbnail_keeper().keep(result);
    }

    /// A `Send` handle that keeps this job's thumbnail, for a blocking
    /// thread that holds the result.
    pub fn thumbnail_keeper(&self) -> ThumbnailKeeper {
        ThumbnailKeeper {
            thumbnails: self.observers.thumbnails.clone(),
            job_id: self.job.job_id.clone(),
            span: self.span.clone(),
        }
    }

    /// Log the job's end, take it off the running list and record it in its
    /// ring (studio jobs in the recent ring, the rest in the local ring).
    pub fn finish(self, outcome: JobOutcome) -> RecentJob {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.span.in_scope(|| match &outcome {
            JobOutcome::Completed => tracing::info!(
                target: TRACE_TARGET,
                op = "job",
                outcome = "completed",
                elapsed_ms,
                "job finished"
            ),
            JobOutcome::Failed { reason } => tracing::warn!(
                target: TRACE_TARGET,
                op = "job",
                outcome = "failed",
                elapsed_ms,
                reason = %reason,
                "job finished"
            ),
        });
        let recent = RecentJob {
            job_id: self.job.job_id.clone(),
            kind: self.job.kind,
            model: self.job.model.clone(),
            prompt: self.job.prompt.clone(),
            outcome,
            started_at: self.job.started_at,
            finished_at: Utc::now(),
            source: self.job.source,
        };
        match self.job.source {
            JobSource::Studio => record_recent_job(&self.observers, recent.clone()),
            _ => record_local_job(&self.observers, recent.clone()),
        }
        recent
    }
}

/// Keeps one job's thumbnail; see [`JobRun::thumbnail_keeper`].
#[derive(Clone)]
pub struct ThumbnailKeeper {
    thumbnails: crate::thumbnail::Thumbnails,
    job_id: String,
    span: tracing::Span,
}

impl ThumbnailKeeper {
    /// Keep a thumbnail when `result` is an image; log in the job's log
    /// when one cannot be made.
    pub fn keep(&self, result: &TaskResult) {
        let TaskResult::Image { bytes, .. } = result else {
            return;
        };
        match crate::thumbnail::make_thumbnail(bytes) {
            Ok(png) => self.thumbnails.insert(&self.job_id, png),
            Err(err) => self.span.in_scope(|| {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "thumbnail",
                    error = %format!("{err:#}"),
                    "could not make a thumbnail of the job's image"
                );
            }),
        }
    }
}

impl Drop for JobRun {
    fn drop(&mut self) {
        self.observers
            .active_jobs
            .lock()
            .retain(|j| j.job_id != self.job.job_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TaskKind;

    fn job(id: &str, source: JobSource) -> CurrentJob {
        CurrentJob {
            job_id: id.into(),
            kind: TaskKind::Image,
            model: "m".into(),
            prompt: "a fox".into(),
            started_at: Utc::now(),
            source,
        }
    }

    fn webp() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::WebP,
            )
            .unwrap();
        out
    }

    #[test]
    fn a_running_job_is_listed_until_it_finishes() {
        let observers = WorkerObservers::default();
        let run = JobRun::begin(&observers, job("run-1", JobSource::Local));
        assert_eq!(observers.active_jobs.lock()[0].job_id, "run-1");
        let recent = run.finish(JobOutcome::Completed);
        assert!(observers.active_jobs.lock().is_empty());
        assert_eq!(recent.source, JobSource::Local);
        assert_eq!(observers.local_jobs.lock()[0], recent);
    }

    #[test]
    fn a_studio_job_is_recorded_in_the_recent_ring() {
        let observers = WorkerObservers::default();
        JobRun::begin(&observers, job("studio-1", JobSource::Studio)).finish(JobOutcome::Failed {
            reason: "boom".into(),
        });
        assert_eq!(observers.recent_jobs.lock()[0].job_id, "studio-1");
        assert!(observers.local_jobs.lock().is_empty());
    }

    #[test]
    fn lane_and_stream_jobs_are_recorded_in_the_local_ring() {
        let observers = WorkerObservers::default();
        JobRun::begin(&observers, job("lane-1", JobSource::Lane)).finish(JobOutcome::Completed);
        JobRun::begin(&observers, job("stream-1", JobSource::Stream)).finish(JobOutcome::Completed);
        let ids: Vec<_> = observers
            .local_jobs
            .lock()
            .iter()
            .map(|j| j.job_id.clone())
            .collect();
        assert_eq!(ids, ["stream-1", "lane-1"]);
    }

    #[test]
    fn the_prompt_can_be_set_once_known() {
        let observers = WorkerObservers::default();
        let mut run = JobRun::begin(&observers, job("stream-2", JobSource::Stream));
        run.set_prompt("hello there");
        assert_eq!(run.finish(JobOutcome::Completed).prompt, "hello there");
    }

    #[test]
    fn dropping_an_unfinished_job_takes_it_off_the_running_list() {
        let observers = WorkerObservers::default();
        {
            let _run = JobRun::begin(&observers, job("dropped", JobSource::Local));
            assert_eq!(observers.active_jobs.lock().len(), 1);
        }
        assert!(observers.active_jobs.lock().is_empty());
        assert!(observers.local_jobs.lock().is_empty());
    }

    #[test]
    fn an_image_result_keeps_a_thumbnail() {
        let observers = WorkerObservers::default();
        let run = JobRun::begin(&observers, job("thumb-1", JobSource::Local));
        run.keep_thumbnail(&TaskResult::Image {
            bytes: webp(),
            ext: "webp".into(),
        });
        assert!(observers.thumbnails.contains("thumb-1"));
    }

    #[test]
    fn a_non_image_result_keeps_no_thumbnail() {
        let observers = WorkerObservers::default();
        let run = JobRun::begin(&observers, job("thumb-2", JobSource::Local));
        run.keep_thumbnail(&TaskResult::Llm {
            json: serde_json::json!({}),
        });
        assert!(observers.thumbnails.is_empty());
    }

    #[test]
    fn the_job_log_records_start_finish_and_a_failed_thumbnail() {
        crate::test_support::install_job_log_capture();
        let observers = WorkerObservers::default();
        let run = JobRun::begin(&observers, job("logged-1", JobSource::Local));
        run.keep_thumbnail(&TaskResult::Image {
            bytes: b"garbage".to_vec(),
            ext: "webp".into(),
        });
        run.finish(JobOutcome::Failed {
            reason: "engine exploded".into(),
        });
        let log = crate::job_log::global().get("logged-1").expect("job log");
        let messages: Vec<_> = log.lines.iter().map(|l| l.message.as_str()).collect();
        assert!(messages[0].starts_with("job started"), "{messages:?}");
        assert!(messages[0].contains("source=\"local\""), "{messages:?}");
        assert!(
            messages[1].starts_with("could not make a thumbnail"),
            "{messages:?}"
        );
        assert!(messages[1].contains("op=\"thumbnail\""), "{messages:?}");
        assert!(messages[2].starts_with("job finished"), "{messages:?}");
        assert!(
            messages[2].contains("reason=engine exploded"),
            "{messages:?}"
        );
        assert_eq!(log.lines[2].level, "warn");
        assert!(observers.thumbnails.is_empty());
    }
}
