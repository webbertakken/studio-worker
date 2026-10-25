//! The model manifest ESPHome and Home Assistant load beside the model: `<name>.json`
//! (`docs/engines/wake-word.md`, "Manifest").

use serde::Serialize;

use super::request::TrainingSpec;

/// The arena TensorFlow Lite Micro needed here, plus this share: the published `hey_jarvis` runs in
/// 20 320 bytes here and its manifest says 22 860 (+12.5 %); doubled for other builds of the
/// runtime.  Safe range 0.1 to 1.0.
pub const ARENA_MARGIN: f64 = 0.25;
/// microWakeWord's features: one slice every 10 ms.
pub const FEATURE_STEP_MS: u32 = 10;
/// ESPHome's v2 manifests.
pub const MINIMUM_ESPHOME_VERSION: &str = "2024.7.0";
const MANIFEST_VERSION: u32 = 2;
const ARENA_ROUNDING: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Micro {
    pub probability_cutoff: f64,
    pub feature_step_size: u32,
    pub sliding_window_size: u32,
    pub tensor_arena_size: u64,
    pub minimum_esphome_version: &'static str,
}

/// Field order as ESPHome's own manifests.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Manifest {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub wake_word: String,
    pub author: String,
    pub website: String,
    pub model: String,
    pub trained_languages: Vec<&'static str>,
    pub version: u32,
    pub micro: Micro,
}

/// The arena to ask for: the measured minimum plus [`ARENA_MARGIN`], rounded up to 1 KiB.
pub fn tensor_arena_size(measured_min_bytes: u64) -> u64 {
    let padded = (measured_min_bytes as f64 * (1.0 + ARENA_MARGIN)).ceil() as u64;
    padded.div_ceil(ARENA_ROUNDING) * ARENA_ROUNDING
}

pub fn manifest(spec: &TrainingSpec, cutoff: f64, arena_min_bytes: u64) -> Manifest {
    Manifest {
        kind: "micro",
        wake_word: spec.display_name.clone(),
        author: spec.author.clone(),
        website: spec.website.clone(),
        model: format!("{}.tflite", spec.name),
        trained_languages: spec.languages.iter().map(|l| l.code()).collect(),
        version: MANIFEST_VERSION,
        micro: Micro {
            probability_cutoff: (cutoff * 1000.0).round() / 1000.0,
            feature_step_size: FEATURE_STEP_MS,
            sliding_window_size: spec.settings.evaluation.sliding_window,
            tensor_arena_size: tensor_arena_size(arena_min_bytes),
            minimum_esphome_version: MINIMUM_ESPHOME_VERSION,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake_word::request;

    #[test]
    fn the_manifest_is_esphomes_v2_shape() {
        let spec = request::parse(
            br#"{"phrase":"hey runa","languages":["en","nl"],"author":"Webber Takken"}"#,
        )
        .unwrap();
        let json = serde_json::to_string_pretty(&manifest(&spec, 0.930_000_1, 25_609)).unwrap();
        let expected = r#"{
  "type": "micro",
  "wake_word": "Hey Runa",
  "author": "Webber Takken",
  "website": "https://github.com/webbertakken/studio-worker",
  "model": "hey_runa.tflite",
  "trained_languages": [
    "en",
    "nl"
  ],
  "version": 2,
  "micro": {
    "probability_cutoff": 0.93,
    "feature_step_size": 10,
    "sliding_window_size": 5,
    "tensor_arena_size": 32768,
    "minimum_esphome_version": "2024.7.0"
  }
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn the_arena_gets_its_margin_and_whole_kibibytes() {
        assert_eq!(tensor_arena_size(20_320), 25_600);
        assert_eq!(tensor_arena_size(1), 1024);
        assert_eq!(tensor_arena_size(0), 0);
    }
}
