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
use crate::runtime::{record_local_job, truncate_prompt, JobOutcome, RecentJob, WorkerObservers};
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
    #[error("no image model configured in the local catalog")]
    NoDefaultModel,
    #[error("model '{0}' is not an image model")]
    NotImageModel(String),
    #[error("engine error: {0}")]
    Engine(String),
}

fn next_job_id() -> String {
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

    let job_id = next_job_id();
    let started_at = Utc::now();
    let result = engine.dispatch_with_source(&model.id, Task::Image(params), &model.source);
    let finished_at = Utc::now();

    let outcome = match &result {
        Ok(_) => JobOutcome::Completed,
        Err(err) => JobOutcome::Failed {
            reason: err.to_string(),
        },
    };
    record_local_job(
        observers,
        RecentJob {
            job_id,
            kind: TaskKind::Image,
            model: model.id.clone(),
            prompt: truncate_prompt(&req.prompt),
            outcome,
            started_at,
            finished_at,
        },
    );

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
        }
    }

    fn catalog_with(models: Vec<CatalogModel>) -> Catalog {
        Catalog { models }
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
}
