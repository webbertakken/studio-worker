//! Real vision E2E: the seeded MegaDetector and bird classifier on real pictures, through the
//! real ONNX Runtime (CUDA when the host has it).
//!
//! Skipped unless `RUN_REAL_ENGINE_TESTS=1`.  The models download into
//! `STUDIO_WORKER_MODELS_CACHE` (else the user cache dir) on first run.  The pictures come from
//! the environment, so none is committed:
//!
//! - `VISION_ANIMAL_PICTURE`: a camera trap picture with an animal in it;
//! - `VISION_BLANK_PICTURE`: a picture without animals;
//! - `VISION_BIRD_PICTURE` and `VISION_BIRD_SPECIES`: a bird and its label (`AMERICAN ROBIN`).
#![cfg(feature = "vision")]

use std::path::PathBuf;

use studio_worker::catalog::Catalog;
use studio_worker::engine::vision::{self, ClassifyOptions, DetectOptions};
use studio_worker::host::ModelRuntime;
use studio_worker::loaders::Loaders;

fn cache_root() -> PathBuf {
    if let Ok(p) = std::env::var("STUDIO_WORKER_MODELS_CACHE") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("gg", "minis", "minis-studio-worker")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("studio-worker-models"))
}

/// The picture at `$name`, or `None` (the test is skipped) when real tests are off.
fn picture(name: &str) -> Option<image::RgbImage> {
    std::env::var_os("RUN_REAL_ENGINE_TESTS")?;
    let path = std::env::var(name).unwrap_or_else(|_| panic!("{name} must name a picture"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    Some(vision::decode_picture(&bytes).expect("a picture"))
}

fn load(id: &str) -> std::sync::Arc<dyn studio_worker::host::LoadedModel> {
    let seed = Catalog::seed();
    let model = seed.get(id).expect("a seeded model");
    Loaders::new(cache_root()).load(model).expect("loads")
}

#[test]
fn megadetector_finds_the_animal_and_nothing_in_a_blank() {
    let (Some(animal), Some(blank)) = (
        picture("VISION_ANIMAL_PICTURE"),
        picture("VISION_BLANK_PICTURE"),
    ) else {
        eprintln!("[real_vision] skipped: set RUN_REAL_ENGINE_TESTS=1 and the pictures");
        return;
    };
    let loaded = load("megadetector-v5a");
    let detector = loaded.as_vision().expect("a vision model");

    let found = vision::detect(detector, &animal, &DetectOptions::default()).unwrap();
    eprintln!("[real_vision] {} on animal: {found:?}", detector.device());
    let best = found.first().expect("a detection");
    assert_eq!(best.label, "animal");
    assert!(best.animal);
    assert!(best.confidence > 0.5, "{}", best.confidence);
    assert!(best.area.width > 0.0 && best.area.height > 0.0);

    let found = vision::detect(detector, &blank, &DetectOptions::default()).unwrap();
    assert!(
        found.iter().all(|d| !d.animal),
        "a blank has no animal: {found:?}"
    );
}

#[test]
fn the_bird_classifier_names_the_species() {
    let Some(bird) = picture("VISION_BIRD_PICTURE") else {
        eprintln!("[real_vision] skipped: set RUN_REAL_ENGINE_TESTS=1 and the pictures");
        return;
    };
    let species = std::env::var("VISION_BIRD_SPECIES").expect("VISION_BIRD_SPECIES");
    let loaded = load("bird-species-efficientnet-b2");
    let classifier = loaded.as_vision().expect("a vision model");

    let ranked = vision::classify(classifier, &bird, &ClassifyOptions::default()).unwrap();
    eprintln!("[real_vision] {} on bird: {ranked:?}", classifier.device());
    assert_eq!(ranked.len(), 5);
    assert_eq!(ranked[0].label, species);
    assert!(ranked[0].confidence > 0.5, "{}", ranked[0].confidence);
    let total: f32 = ranked.iter().map(|p| p.confidence).sum();
    assert!(total <= 1.0 + 1e-4, "softmaxed: {total}");
}
