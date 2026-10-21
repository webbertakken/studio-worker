//! Vision on the local API: detection, classification, and the two together (animals found,
//! then each one classified).  See `docs/engines/vision.md`.
//!
//! A loaded model answers on its lane, warm, beside other jobs.  A model that is not loaded
//! runs as a transient job: it takes the job gate, is admitted next to the loaded models,
//! loads, answers, and is freed, so nothing stays in memory between requests unless the
//! operator keeps it loaded.

use std::time::Instant;

use chrono::Utc;
use image::RgbImage;
use serde::Serialize;

use crate::catalog::{Catalog, CatalogModel};
use crate::engine::vision::{
    self, preprocess, ClassifyOptions, DetectOptions, Detection, Prediction, VisionModel,
};
use crate::host::ModelHost;
use crate::job_gate::{JobGate, JobReservation};
use crate::job_run::JobRun;
use crate::runtime::{CurrentJob, JobOutcome, JobSource, WorkerObservers};
use crate::types::{TaskKind, VisionTask};

const TRACE_TARGET: &str = "studio_worker::vision";

/// Most animals one picture has classified, strongest detections first.  Each costs one
/// classifier run.  Safe range 1..=300.
pub const MAX_CLASSIFIED: usize = 20;

/// Why a vision request was not answered.
#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    #[error("unknown model '{0}' (not in the local catalogue)")]
    UnknownModel(String),
    #[error("no enabled {} model in the local catalogue", .0.as_str())]
    NoModel(VisionTask),
    #[error("model '{id}' is {got}, not a {} model", .want.as_str())]
    WrongModel {
        id: String,
        want: VisionTask,
        got: String,
    },
    #[error("model '{0}' is disabled")]
    Disabled(String),
    #[error("the worker is busy with another job (studio or local); retry shortly")]
    Busy,
    #[error("{0}")]
    InsufficientMemory(crate::admission::Refused),
    #[error("model '{0}' is no vision model on this worker (built without the `vision` feature?)")]
    NotServed(String),
    #[error("{0}")]
    Failed(String),
}

/// What a vision request needs from the daemon.
pub struct VisionContext<'a> {
    pub host: &'a ModelHost,
    pub catalog: &'a Catalog,
    pub observers: &'a WorkerObservers,
    pub gate: &'a JobGate,
}

/// A picture's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PictureSize {
    pub width: u32,
    pub height: u32,
}

impl PictureSize {
    fn of(picture: &RgbImage) -> Self {
        Self {
            width: picture.width(),
            height: picture.height(),
        }
    }
}

/// The answer of `POST /vision/detect`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectAnswer {
    pub model: String,
    pub device: &'static str,
    pub picture: PictureSize,
    /// Whether any detection is an animal: false means the picture is blank.
    pub animal_present: bool,
    pub detections: Vec<Detection>,
    pub elapsed_ms: u64,
}

/// The answer of `POST /vision/classify`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifyAnswer {
    pub model: String,
    pub device: &'static str,
    pub picture: PictureSize,
    pub predictions: Vec<Prediction>,
    pub elapsed_ms: u64,
}

/// One detection of `POST /vision/species`, with the species of an animal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpeciesDetection {
    #[serde(flatten)]
    pub detection: Detection,
    /// The classifier's ranking of the animal's crop; absent for what is no animal, and for
    /// animals past [`MAX_CLASSIFIED`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub species: Option<Vec<Prediction>>,
}

/// The answer of `POST /vision/species`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeciesAnswer {
    pub detector: String,
    pub classifier: String,
    pub picture: PictureSize,
    pub animal_present: bool,
    pub detections: Vec<SpeciesDetection>,
    pub elapsed_ms: u64,
}

/// The vision model a request names, else the first enabled one doing `task`.
pub fn resolve<'a>(
    catalog: &'a Catalog,
    id: Option<&str>,
    task: VisionTask,
) -> Result<&'a CatalogModel, VisionError> {
    let does = |m: &CatalogModel| {
        m.kind == TaskKind::Vision
            && vision::defaults_of(m).is_ok_and(|defaults| defaults.task == task)
    };
    let Some(id) = id else {
        return catalog
            .list()
            .iter()
            .find(|m| m.enabled && does(m))
            .ok_or(VisionError::NoModel(task));
    };
    let model = catalog
        .get(id)
        .ok_or_else(|| VisionError::UnknownModel(id.to_string()))?;
    if !does(model) {
        let got = match vision::defaults_of(model) {
            Ok(defaults) if model.kind == TaskKind::Vision => {
                format!("a {} model", defaults.task.as_str())
            }
            _ => format!("a {} model", model.kind.as_str()),
        };
        return Err(VisionError::WrongModel {
            id: id.to_string(),
            want: task,
            got,
        });
    }
    if !model.enabled {
        return Err(VisionError::Disabled(id.to_string()));
    }
    Ok(model)
}

/// Run `work` on `model`: on its lane when loaded, else as a transient job holding the job
/// gate in `reservation` (taken once; the caller's later transient steps reuse it).  Recorded
/// as one job with a thumbnail of `picture`.
fn on_model<R>(
    ctx: &VisionContext<'_>,
    model: &CatalogModel,
    picture: &RgbImage,
    what: &str,
    reservation: &mut Option<JobReservation>,
    work: impl FnOnce(&dyn VisionModel) -> anyhow::Result<R>,
) -> Result<R, VisionError> {
    let preview = format!("{what} {}x{}", picture.width(), picture.height());
    let begin = |source| {
        let run = JobRun::begin(
            ctx.observers,
            CurrentJob {
                job_id: crate::local::next_job_id(),
                kind: TaskKind::Vision,
                model: model.id.clone(),
                prompt: preview.clone(),
                started_at: Utc::now(),
                source,
            },
        );
        run.thumbnail_keeper().keep_picture(picture);
        run
    };

    let loaded = ctx
        .host
        .status(&model.id)
        .is_ok_and(|status| status.state.serves());
    let mut work = Some(work);
    if loaded {
        let served = ctx.host.with_lane(&model.id, |loaded, _lane| {
            let vision = loaded.as_vision()?;
            let run = begin(JobSource::Lane);
            let work = work.take().expect("work runs once");
            let result = run.span().in_scope(|| work(vision));
            Some((run, result))
        });
        match served {
            Ok(Some((run, result))) => return finish(run, result),
            Ok(None) => return Err(VisionError::NotServed(model.id.clone())),
            // Unloaded since the check: serve it transiently instead.
            Err(_) => {}
        }
    }

    if reservation.is_none() {
        *reservation = Some(ctx.gate.try_reserve().ok_or(VisionError::Busy)?);
    }
    ctx.host
        .admit_transient(
            &model.id,
            crate::admission::MemoryNeed::of(model.vram_gb_estimate, &model.source),
        )
        .map_err(VisionError::InsufficientMemory)?;
    let run = begin(JobSource::Local);
    let result = run.span().in_scope(|| {
        let loaded = ctx.host.load_transient(model)?;
        let Some(vision) = loaded.as_vision() else {
            return Ok(Err(VisionError::NotServed(model.id.clone())));
        };
        let work = work.take().expect("work runs once");
        anyhow::Ok(Ok(work(vision)?))
    });
    match result {
        Ok(Err(not_served)) => {
            run.finish(JobOutcome::Failed {
                reason: not_served.to_string(),
            });
            Err(not_served)
        }
        Ok(Ok(answer)) => finish(run, Ok(answer)),
        Err(err) => finish(run, Err(err)),
    }
}

fn finish<R>(run: JobRun, result: anyhow::Result<R>) -> Result<R, VisionError> {
    match result {
        Ok(answer) => {
            run.finish(JobOutcome::Completed);
            Ok(answer)
        }
        Err(err) => {
            let reason = format!("{err:#}");
            run.finish(JobOutcome::Failed {
                reason: reason.clone(),
            });
            Err(VisionError::Failed(reason))
        }
    }
}

/// What a detector finds in `picture`.
pub fn detect(
    ctx: &VisionContext<'_>,
    model: Option<&str>,
    picture: &RgbImage,
    options: &DetectOptions,
) -> Result<DetectAnswer, VisionError> {
    let started = Instant::now();
    let model = resolve(ctx.catalog, model, VisionTask::Detect)?;
    let (device, detections) = on_model(ctx, model, picture, "detect", &mut None, |m| {
        Ok((m.device(), vision::detect(m, picture, options)?))
    })?;
    let answer = DetectAnswer {
        model: model.id.clone(),
        device,
        picture: PictureSize::of(picture),
        animal_present: detections.iter().any(|d| d.animal),
        detections,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    tracing::info!(
        target: TRACE_TARGET,
        op = "detect",
        model = %answer.model,
        device,
        detections = answer.detections.len(),
        animal_present = answer.animal_present,
        elapsed_ms = answer.elapsed_ms,
        "picture searched"
    );
    Ok(answer)
}

/// What a classifier sees in `picture`.
pub fn classify(
    ctx: &VisionContext<'_>,
    model: Option<&str>,
    picture: &RgbImage,
    options: &ClassifyOptions,
) -> Result<ClassifyAnswer, VisionError> {
    let started = Instant::now();
    let model = resolve(ctx.catalog, model, VisionTask::Classify)?;
    let (device, predictions) = on_model(ctx, model, picture, "classify", &mut None, |m| {
        Ok((m.device(), vision::classify(m, picture, options)?))
    })?;
    let answer = ClassifyAnswer {
        model: model.id.clone(),
        device,
        picture: PictureSize::of(picture),
        predictions,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    tracing::info!(
        target: TRACE_TARGET,
        op = "classify",
        model = %answer.model,
        device,
        top = answer.predictions.first().map(|p| p.label.as_str()).unwrap_or("-"),
        confidence = answer.predictions.first().map(|p| p.confidence).unwrap_or(0.0),
        elapsed_ms = answer.elapsed_ms,
        "picture classified"
    );
    Ok(answer)
}

/// The animals a detector finds in `picture`, each classified on its own crop.  A picture
/// without animals never runs the classifier.
pub fn species(
    ctx: &VisionContext<'_>,
    detector: Option<&str>,
    classifier: Option<&str>,
    picture: &RgbImage,
    detect_options: &DetectOptions,
    classify_options: &ClassifyOptions,
) -> Result<SpeciesAnswer, VisionError> {
    let started = Instant::now();
    let detector = resolve(ctx.catalog, detector, VisionTask::Detect)?;
    let classifier = resolve(ctx.catalog, classifier, VisionTask::Classify)?;
    let mut reservation = None;
    let found = on_model(ctx, detector, picture, "detect", &mut reservation, |m| {
        vision::detect(m, picture, detect_options)
    })?;
    let animals: Vec<usize> = found
        .iter()
        .enumerate()
        .filter(|(_, d)| d.animal)
        .map(|(i, _)| i)
        .take(MAX_CLASSIFIED)
        .collect();
    let mut ranked = vec![None; found.len()];
    if !animals.is_empty() {
        let rankings = on_model(ctx, classifier, picture, "species", &mut reservation, |m| {
            // Each classifier says how much of an animal's surroundings it wants.
            let margin = m.spec().crop_margin;
            animals
                .iter()
                .map(|&i| {
                    let crop = preprocess::crop(picture, &found[i].area, margin);
                    vision::classify(m, &crop, classify_options)
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })?;
        for (i, ranking) in animals.iter().zip(rankings) {
            ranked[*i] = Some(ranking);
        }
    }
    let answer = SpeciesAnswer {
        detector: detector.id.clone(),
        classifier: classifier.id.clone(),
        picture: PictureSize::of(picture),
        animal_present: !animals.is_empty(),
        detections: found
            .into_iter()
            .zip(ranked)
            .map(|(detection, species)| SpeciesDetection { detection, species })
            .collect(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    tracing::info!(
        target: TRACE_TARGET,
        op = "species",
        detector = %answer.detector,
        classifier = %answer.classifier,
        detections = answer.detections.len(),
        classified = animals.len(),
        elapsed_ms = answer.elapsed_ms,
        "picture searched for species"
    );
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::ModelState;
    use crate::test_support::{
        vision_catalog as catalog, vision_defaults as defaults, vision_model, FakeVisionRuntime,
    };
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        host: ModelHost,
        catalog: Catalog,
        observers: WorkerObservers,
        gate: JobGate,
    }

    impl Fixture {
        fn new(catalog: Catalog) -> Self {
            Self::with_free(catalog, 20.0)
        }

        fn with_free(catalog: Catalog, free_gib: f32) -> Self {
            let host = ModelHost::new(
                Arc::new(parking_lot::Mutex::new(catalog.clone())),
                Arc::new(FakeVisionRuntime),
                Arc::new(crate::test_support::FixedProbe(free_gib)),
                crate::residency::Residency::load_for_serving(None),
            );
            Self {
                host,
                catalog,
                observers: WorkerObservers::default(),
                gate: JobGate::new(),
            }
        }

        fn ctx(&self) -> VisionContext<'_> {
            VisionContext {
                host: &self.host,
                catalog: &self.catalog,
                observers: &self.observers,
                gate: &self.gate,
            }
        }

        fn load(&self, id: &str) {
            self.host.load(id).unwrap();
            self.host
                .wait_for(id, ModelState::serves, Duration::from_secs(5))
                .expect("loaded");
        }
    }

    fn picture() -> RgbImage {
        RgbImage::new(64, 64)
    }

    #[test]
    fn a_request_without_a_model_takes_the_first_doing_the_task() {
        let catalog = catalog();
        assert_eq!(
            resolve(&catalog, None, VisionTask::Detect).unwrap().id,
            "md"
        );
        assert_eq!(
            resolve(&catalog, None, VisionTask::Classify).unwrap().id,
            "birds"
        );
        assert!(matches!(
            resolve(&Catalog::default(), None, VisionTask::Detect),
            Err(VisionError::NoModel(VisionTask::Detect))
        ));
    }

    #[test]
    fn a_named_model_must_exist_do_the_task_and_be_enabled() {
        let mut catalog = catalog();
        assert!(matches!(
            resolve(&catalog, Some("nope"), VisionTask::Detect),
            Err(VisionError::UnknownModel(_))
        ));
        let err = resolve(&catalog, Some("birds"), VisionTask::Detect).unwrap_err();
        assert!(
            err.to_string()
                .contains("'birds' is a classify model, not a detect model"),
            "{err}"
        );
        catalog.models[0].enabled = false;
        assert!(matches!(
            resolve(&catalog, Some("md"), VisionTask::Detect),
            Err(VisionError::Disabled(_))
        ));
        assert!(matches!(
            resolve(&catalog, None, VisionTask::Detect),
            Err(VisionError::NoModel(_))
        ));
    }

    #[test]
    fn a_model_of_another_kind_is_named_by_its_kind() {
        let mut catalog = catalog();
        catalog.models[1].kind = TaskKind::Image;
        let err = resolve(&catalog, Some("birds"), VisionTask::Classify).unwrap_err();
        assert!(err.to_string().contains("is a image model"), "{err}");
    }

    #[test]
    fn a_transient_detection_records_a_local_job_and_frees_the_gate() {
        let f = Fixture::new(catalog());
        let answer = detect(&f.ctx(), None, &picture(), &DetectOptions::default()).unwrap();
        assert_eq!(answer.model, "md");
        assert!(answer.animal_present);
        assert_eq!(answer.detections.len(), 2);
        assert_eq!(
            answer.picture,
            PictureSize {
                width: 64,
                height: 64
            }
        );
        let job = f.observers.local_jobs.lock()[0].clone();
        assert_eq!(job.kind, TaskKind::Vision);
        assert_eq!(job.source, JobSource::Local);
        assert_eq!(job.prompt, "detect 64x64");
        assert_eq!(job.outcome, JobOutcome::Completed);
        assert!(f.observers.thumbnails.contains(&job.job_id));
        assert!(!f.gate.is_busy(), "the gate is free again");
    }

    #[test]
    fn a_loaded_model_answers_on_its_lane_while_the_gate_is_taken() {
        let f = Fixture::new(catalog());
        f.load("birds");
        let _studio_job = f.gate.try_reserve().unwrap();
        let answer = classify(&f.ctx(), None, &picture(), &ClassifyOptions::default()).unwrap();
        assert_eq!(answer.predictions[0].label, "robin");
        assert_eq!(answer.device, "cpu");
        assert_eq!(f.observers.local_jobs.lock()[0].source, JobSource::Lane);
    }

    #[test]
    fn a_transient_request_while_the_gate_is_taken_is_busy() {
        let f = Fixture::new(catalog());
        let _studio_job = f.gate.try_reserve().unwrap();
        let err = detect(&f.ctx(), None, &picture(), &DetectOptions::default()).unwrap_err();
        assert!(matches!(err, VisionError::Busy), "{err}");
        assert!(f.observers.local_jobs.lock().is_empty());
    }

    #[test]
    fn a_transient_request_that_does_not_fit_is_refused() {
        let f = Fixture::with_free(catalog(), 0.1);
        let err = detect(&f.ctx(), None, &picture(), &DetectOptions::default()).unwrap_err();
        assert!(matches!(err, VisionError::InsufficientMemory(_)), "{err}");
        assert!(!f.gate.is_busy());
    }

    #[test]
    fn a_failing_model_is_recorded_as_a_failed_job() {
        let mut catalog = catalog();
        // The detector answers a 2-class layout; a 5-class detector cannot read it.
        catalog.models[0] = vision_model(
            "md",
            64,
            defaults(VisionTask::Detect, &["a", "b", "c", "d", "e"]),
        );
        let f = Fixture::new(catalog);
        let err = detect(&f.ctx(), None, &picture(), &DetectOptions::default()).unwrap_err();
        assert!(
            matches!(err, VisionError::Failed(ref m) if m.contains("no YOLO layout")),
            "{err}"
        );
        let job = f.observers.local_jobs.lock()[0].clone();
        assert!(matches!(job.outcome, JobOutcome::Failed { .. }));
    }

    #[test]
    fn species_classifies_each_animal_and_leaves_the_rest() {
        let f = Fixture::new(catalog());
        let answer = species(
            &f.ctx(),
            None,
            None,
            &picture(),
            &DetectOptions::default(),
            &ClassifyOptions::default(),
        )
        .unwrap();
        assert_eq!(
            (answer.detector.as_str(), answer.classifier.as_str()),
            ("md", "birds")
        );
        assert!(answer.animal_present);
        let animal = &answer.detections[0];
        assert!(animal.detection.animal);
        assert_eq!(animal.species.as_ref().unwrap()[0].label, "robin");
        assert!(
            answer.detections[1].species.is_none(),
            "a person is not classified"
        );
        // One detector job and one classifier job, both transient.
        let jobs = f.observers.local_jobs.lock().clone();
        let models: Vec<&str> = jobs.iter().map(|j| j.model.as_str()).collect();
        assert_eq!(models.len(), 2);
        assert!(models.contains(&"md") && models.contains(&"birds"));
    }

    #[test]
    fn a_blank_picture_never_runs_the_classifier() {
        let mut catalog = catalog();
        // Nothing is an animal for this detector: every picture is blank.
        let mut no_animals = defaults(VisionTask::Detect, &["animal", "person"]);
        no_animals.animal_labels = Some(vec![]);
        catalog.models[0] = vision_model("md", 64, no_animals);
        let f = Fixture::new(catalog);
        let answer = species(
            &f.ctx(),
            None,
            None,
            &picture(),
            &DetectOptions::default(),
            &ClassifyOptions::default(),
        )
        .unwrap();
        assert!(!answer.animal_present);
        assert!(answer.detections.iter().all(|d| d.species.is_none()));
        assert_eq!(
            f.observers.local_jobs.lock().len(),
            1,
            "only the detector ran"
        );
    }

    #[test]
    fn species_needs_both_models_before_running_either() {
        let mut catalog = catalog();
        catalog.models.remove(1);
        let f = Fixture::new(catalog);
        let err = species(
            &f.ctx(),
            None,
            None,
            &picture(),
            &DetectOptions::default(),
            &ClassifyOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, VisionError::NoModel(VisionTask::Classify)),
            "{err}"
        );
        assert!(f.observers.local_jobs.lock().is_empty());
    }

    #[test]
    fn a_model_that_loads_as_no_vision_model_is_not_served() {
        let catalog = catalog();
        let host = ModelHost::new(
            Arc::new(parking_lot::Mutex::new(catalog.clone())),
            Arc::new(crate::test_support::InstantRuntime),
            Arc::new(crate::test_support::FixedProbe(20.0)),
            crate::residency::Residency::load_for_serving(None),
        );
        let observers = WorkerObservers::default();
        let gate = JobGate::new();
        let ctx = VisionContext {
            host: &host,
            catalog: &catalog,
            observers: &observers,
            gate: &gate,
        };
        let err = detect(&ctx, None, &picture(), &DetectOptions::default()).unwrap_err();
        assert!(matches!(err, VisionError::NotServed(_)), "{err}");
        host.load("md").unwrap();
        host.wait_for("md", ModelState::serves, Duration::from_secs(5))
            .unwrap();
        let err = detect(&ctx, None, &picture(), &DetectOptions::default()).unwrap_err();
        assert!(matches!(err, VisionError::NotServed(_)), "{err}");
    }

    #[test]
    fn answers_serialise_in_camel_case() {
        let f = Fixture::new(catalog());
        let answer = species(
            &f.ctx(),
            None,
            None,
            &picture(),
            &DetectOptions::default(),
            &ClassifyOptions::default(),
        )
        .unwrap();
        let json = serde_json::to_value(&answer).unwrap();
        assert_eq!(json["animalPresent"], true);
        let first = &json["detections"][0];
        assert_eq!(first["label"], "animal");
        assert_eq!(first["classId"], 0);
        assert!(first["box"]["width"].is_number());
        assert!(first["boxNormalised"]["x"].is_number());
        assert_eq!(first["species"][0]["label"], "robin");
        assert!(json["detections"][1].get("species").is_none());
    }
}
