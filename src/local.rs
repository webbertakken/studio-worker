//! Local image generation — submit a prompt straight to the engine, no studio.
//!
//! Mirrors the studio job path (`ws::session::run_offered_job`): resolve the
//! model's [`ModelSource`] (here from the local [`Catalog`] instead of a studio
//! offer), build a [`Task::Image`], dispatch, and record the finished job — into
//! the dedicated local-queue ring so it shows up in the app.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;

use crate::catalog::Catalog;
use crate::engine::Engine;
use crate::job_run::JobRun;
use crate::runtime::{truncate_prompt, CurrentJob, JobOutcome, JobSource, WorkerObservers};
use crate::types::{ImageParams, Task, TaskKind, TaskResult};

/// A local image-generation request. Optional fields fall back to the model's
/// CLI defaults from the catalog.
#[derive(Debug, Clone, Default)]
pub struct LocalImageRequest {
    pub prompt: String,
    /// Model id; `None` uses the catalog's default image model.
    pub model: Option<String>,
    pub negative_prompt: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub steps: Option<u32>,
    pub seed: Option<u64>,
    pub ext: Option<String>,
}

/// Why a local generation could not run.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("unknown model '{0}' (not in the local catalog)")]
    UnknownModel(String),
    #[error("no {0} model configured in the local catalog")]
    NoModelForKind(TaskKind),
    #[error("no image model configured in the local catalog")]
    NoDefaultModel,
    #[error("model '{0}' is not an image model")]
    NotImageModel(String),
    #[error("model '{id}' is a {got} model, not {want}")]
    WrongKind {
        id: String,
        want: TaskKind,
        got: TaskKind,
    },
    #[error("engine error: {0}")]
    Engine(String),
}

pub(crate) fn next_job_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("local-{}-{n}", Utc::now().timestamp_millis())
}

/// Run one local image job: resolve the model from `catalog`, dispatch it on
/// `engine`, record it in the local-queue ring, and return the image bytes.
pub fn run_image(
    engine: &dyn Engine,
    catalog: &Catalog,
    observers: &WorkerObservers,
    req: &LocalImageRequest,
) -> Result<TaskResult, LocalError> {
    let model = match &req.model {
        Some(id) => catalog
            .get(id)
            .ok_or_else(|| LocalError::UnknownModel(id.clone()))?,
        None => catalog
            .default_image_model()
            .ok_or(LocalError::NoDefaultModel)?,
    };
    if model.kind != TaskKind::Image {
        return Err(LocalError::NotImageModel(model.id.clone()));
    }

    let defaults = &model.source.cli_defaults;
    let params = ImageParams {
        prompt: req.prompt.clone(),
        negative_prompt: req.negative_prompt.clone(),
        width: req.width.unwrap_or(defaults.width).max(1),
        height: req.height.unwrap_or(defaults.height).max(1),
        steps: req.steps.unwrap_or(defaults.steps).max(1),
        seed: req.seed,
        cfg_scale: Some(defaults.cfg_scale),
        sampling_method: defaults.sampling_method.clone(),
        ext: req.ext.clone().unwrap_or_else(|| "webp".to_string()),
        ..Default::default()
    };

    dispatch_and_record(engine, model, observers, &req.prompt, Task::Image(params))
}

/// Resolve a model of `kind` (an explicit id, else the catalog's
/// default for that kind), dispatch `task`, record the local job, and
/// return the result.  The generic core behind every non-image local
/// endpoint (chat / tts / stt / video) so each stays a thin adapter.
pub fn run_kind(
    engine: &dyn Engine,
    catalog: &Catalog,
    observers: &WorkerObservers,
    kind: TaskKind,
    model_id: Option<&str>,
    prompt_preview: &str,
    task: Task,
) -> Result<TaskResult, LocalError> {
    let model = match model_id {
        Some(id) => catalog
            .get(id)
            .ok_or_else(|| LocalError::UnknownModel(id.to_string()))?,
        None => catalog
            .default_model_for(kind)
            .ok_or(LocalError::NoModelForKind(kind))?,
    };
    if model.kind != kind {
        return Err(LocalError::WrongKind {
            id: model.id.clone(),
            want: kind,
            got: model.kind,
        });
    }
    dispatch_and_record(engine, model, observers, prompt_preview, task)
}

/// Serve a chat on the lane of a loaded model, recording it like any
/// local job.  `None` when the resolved model is not loaded (or is not
/// a chat model): the caller runs it as a transient job instead.
pub fn chat_on_lane(
    host: &crate::host::ModelHost,
    catalog: &Catalog,
    observers: &WorkerObservers,
    model_id: Option<&str>,
    prompt_preview: &str,
    params: crate::types::LlmParams,
) -> Option<Result<TaskResult, LocalError>> {
    let model = match model_id {
        Some(id) => catalog.get(id)?,
        None => catalog.default_model_for(TaskKind::Llm)?,
    };
    if model.kind != TaskKind::Llm {
        return None;
    }
    let served = host.with_lane(&model.id, |loaded, lane| {
        let chat = loaded.as_chat()?;
        let run = JobRun::begin(
            observers,
            CurrentJob {
                job_id: next_job_id(),
                kind: TaskKind::Llm,
                model: model.id.clone(),
                prompt: truncate_prompt(prompt_preview),
                started_at: Utc::now(),
                source: JobSource::Lane,
            },
        );
        let result = run
            .span()
            .in_scope(|| chat.chat(params, &|| lane.cancelled()));
        Some((run, result))
    });
    let (run, result) = match served {
        Ok(Some(served)) => served,
        // Not loaded, or loaded but not a chat model: the transient path decides.
        Ok(None) | Err(_) => return None,
    };
    let result = result.map(|json| TaskResult::Llm { json });
    run.finish(outcome_of(&result));
    Some(result.map_err(|err| LocalError::Engine(err.to_string())))
}

/// The outcome a finished job is recorded with.
fn outcome_of<E: std::fmt::Display>(result: &Result<TaskResult, E>) -> JobOutcome {
    match result {
        Ok(_) => JobOutcome::Completed,
        Err(err) => JobOutcome::Failed {
            reason: err.to_string(),
        },
    }
}

/// Dispatch `task` on `model` and record the finished job in the
/// local-queue ring.  Shared by [`run_image`] and [`run_kind`] so the
/// dispatch + bookkeeping lives in one place.
fn dispatch_and_record(
    engine: &dyn Engine,
    model: &crate::catalog::CatalogModel,
    observers: &WorkerObservers,
    prompt_preview: &str,
    task: Task,
) -> Result<TaskResult, LocalError> {
    let run = JobRun::begin(
        observers,
        CurrentJob {
            job_id: next_job_id(),
            kind: task.kind(),
            model: model.id.clone(),
            prompt: truncate_prompt(prompt_preview),
            started_at: Utc::now(),
            source: JobSource::Local,
        },
    );
    let result = run
        .span()
        .in_scope(|| engine.dispatch_with_source(&model.id, task, &model.source));
    if let Ok(result) = &result {
        run.keep_thumbnail(result);
    }
    run.finish(outcome_of(&result));

    result.map_err(|err| LocalError::Engine(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogModel;
    use crate::engine::SyntheticEngine;
    use crate::types::{ModelCliDefaults, ModelEngine, ModelSource};

    fn synthetic_model(id: &str, kind: TaskKind) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            display_name: id.into(),
            kind,
            vram_gb_estimate: 0.0,
            description: None,
            source: ModelSource {
                engine: ModelEngine::Synthetic,
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

    fn catalog_with(models: Vec<CatalogModel>) -> Catalog {
        Catalog {
            models,
            ..Default::default()
        }
    }

    #[test]
    fn generates_image_and_records_local_job() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![synthetic_model("synthetic-img", TaskKind::Image)]);
        let observers = WorkerObservers::default();
        let req = LocalImageRequest {
            prompt: "a red fox".into(),
            ..Default::default()
        };

        let result = run_image(&engine, &catalog, &observers, &req).unwrap();
        match result {
            TaskResult::Image { bytes, ext } => {
                assert!(!bytes.is_empty());
                assert_eq!(ext, "webp");
            }
            other => panic!("expected image, got {other:?}"),
        }

        let ring = observers.local_jobs.lock();
        assert_eq!(ring.len(), 1);
        let job = &ring[0];
        assert_eq!(job.model, "synthetic-img");
        assert_eq!(job.outcome, JobOutcome::Completed);
        assert_eq!(job.prompt, "a red fox");
        // The studio ring stays empty — local jobs are their own queue.
        assert!(observers.recent_jobs.lock().is_empty());
    }

    #[test]
    fn an_image_job_keeps_a_thumbnail_and_a_job_log() {
        crate::test_support::install_job_log_capture();
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![synthetic_model("img", TaskKind::Image)]);
        let observers = WorkerObservers::default();
        let req = LocalImageRequest {
            prompt: "a lighthouse".into(),
            ..Default::default()
        };
        run_image(&engine, &catalog, &observers, &req).unwrap();

        let job = observers.local_jobs.lock()[0].clone();
        assert_eq!(job.source, JobSource::Local);
        assert!(observers.thumbnails.contains(&job.job_id));
        assert!(observers.active_jobs.lock().is_empty());
        let log = crate::job_log::global().get(&job.job_id).expect("job log");
        assert!(log.lines[0].message.starts_with("job started"));
        assert!(log
            .lines
            .last()
            .is_some_and(|l| l.message.starts_with("job finished")));
    }

    #[test]
    fn a_chat_on_a_loaded_model_is_recorded_as_a_lane_job() {
        let mut model = synthetic_model("chat", TaskKind::Llm);
        model.source.engine = ModelEngine::LlamaCpp;
        let catalog = catalog_with(vec![model]);
        let shared = std::sync::Arc::new(parking_lot::Mutex::new(catalog.clone()));
        let host = crate::host::ModelHost::new(
            shared,
            std::sync::Arc::new(crate::test_support::InstantRuntime),
            std::sync::Arc::new(crate::test_support::FixedProbe(20.0)),
            crate::residency::Residency::load_for_serving(None),
        );
        host.load("chat").unwrap();
        host.wait_for(
            "chat",
            crate::lifecycle::ModelState::serves,
            std::time::Duration::from_secs(5),
        )
        .expect("loaded");
        let observers = WorkerObservers::default();
        let params = crate::types::LlmParams {
            messages: vec![crate::types::ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            ..Default::default()
        };

        let answer = chat_on_lane(&host, &catalog, &observers, None, "hello", params)
            .expect("served on the lane")
            .unwrap();

        assert!(matches!(answer, TaskResult::Llm { .. }));
        let job = observers.local_jobs.lock()[0].clone();
        assert_eq!(job.source, JobSource::Lane);
        assert_eq!(job.outcome, JobOutcome::Completed);
        assert!(observers.active_jobs.lock().is_empty());
    }

    #[test]
    fn defaults_to_the_only_image_model_when_unspecified() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![synthetic_model("only-img", TaskKind::Image)]);
        let observers = WorkerObservers::default();
        let req = LocalImageRequest {
            prompt: "x".into(),
            model: None,
            ..Default::default()
        };
        let out = run_image(&engine, &catalog, &observers, &req).unwrap();
        assert!(matches!(out, TaskResult::Image { .. }));
        assert_eq!(observers.local_jobs.lock()[0].model, "only-img");
    }

    #[test]
    fn unknown_model_is_rejected() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![synthetic_model("a", TaskKind::Image)]);
        let observers = WorkerObservers::default();
        let req = LocalImageRequest {
            prompt: "x".into(),
            model: Some("missing".into()),
            ..Default::default()
        };
        let err = run_image(&engine, &catalog, &observers, &req).unwrap_err();
        assert!(matches!(err, LocalError::UnknownModel(m) if m == "missing"));
        assert!(observers.local_jobs.lock().is_empty());
    }

    #[test]
    fn no_image_model_yields_no_default() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![]);
        let observers = WorkerObservers::default();
        let req = LocalImageRequest {
            prompt: "x".into(),
            ..Default::default()
        };
        let err = run_image(&engine, &catalog, &observers, &req).unwrap_err();
        assert!(matches!(err, LocalError::NoDefaultModel));
    }

    #[test]
    fn non_image_model_is_rejected() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![synthetic_model("chat", TaskKind::Llm)]);
        let observers = WorkerObservers::default();
        let req = LocalImageRequest {
            prompt: "x".into(),
            model: Some("chat".into()),
            ..Default::default()
        };
        let err = run_image(&engine, &catalog, &observers, &req).unwrap_err();
        assert!(matches!(err, LocalError::NotImageModel(m) if m == "chat"));
    }

    #[test]
    fn run_kind_dispatches_llm_and_records_the_job() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![synthetic_model("chat", TaskKind::Llm)]);
        let observers = WorkerObservers::default();
        let task = Task::Llm(crate::types::LlmParams {
            messages: vec![crate::types::ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            ..Default::default()
        });
        let out = run_kind(
            &engine,
            &catalog,
            &observers,
            TaskKind::Llm,
            None,
            "hi",
            task,
        )
        .unwrap();
        assert!(matches!(out, TaskResult::Llm { .. }));
        assert_eq!(observers.local_jobs.lock()[0].kind, TaskKind::Llm);
    }

    #[test]
    fn run_kind_rejects_a_wrong_kind_model() {
        let engine = SyntheticEngine::new();
        // An image model explicitly requested for an LLM job.
        let catalog = catalog_with(vec![synthetic_model("img", TaskKind::Image)]);
        let observers = WorkerObservers::default();
        let task = Task::Llm(crate::types::LlmParams::default());
        let err = run_kind(
            &engine,
            &catalog,
            &observers,
            TaskKind::Llm,
            Some("img"),
            "",
            task,
        )
        .unwrap_err();
        assert!(
            matches!(err, LocalError::WrongKind { ref id, want, got }
                if id == "img" && want == TaskKind::Llm && got == TaskKind::Image),
            "got {err:?}"
        );
        assert!(
            observers.local_jobs.lock().is_empty(),
            "no job recorded on reject"
        );
    }

    #[test]
    fn run_kind_reports_no_model_for_kind() {
        let engine = SyntheticEngine::new();
        let catalog = catalog_with(vec![]);
        let observers = WorkerObservers::default();
        let err = run_kind(
            &engine,
            &catalog,
            &observers,
            TaskKind::AudioTts,
            None,
            "",
            Task::AudioTts(crate::types::AudioTtsParams::default()),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LocalError::NoModelForKind(TaskKind::AudioTts)
        ));
    }
}
