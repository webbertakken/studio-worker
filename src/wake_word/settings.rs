//! A training's settings: every number the trainer reads (`docs/engines/wake-word.md`, "Settings").
//!
//! The recipe's defaults ([`super::recipe::default_settings`]) take a request's overrides as a JSON
//! merge patch; the result is read back strictly and checked as a whole, so an override can never
//! leave a field unknown, mistyped or out of range.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a language's synthetic voices speak.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prosody {
    /// Speeds: below 1 is faster.
    pub length_scales: Vec<f64>,
    /// How much phoneme durations vary: pauses grow with it.
    pub noise_scale_ws: Vec<f64>,
    pub noise_scales: Vec<f64>,
    /// How two speakers are mixed into one voice.
    pub slerp_weights: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SampleSettings {
    pub positive_per_language: u32,
    pub near_miss_per_phrase_per_language: u32,
    pub batch_size: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AugmentationSettings {
    pub duration_seconds: f64,
    pub background_min_snr_db: f64,
    pub background_max_snr_db: f64,
    pub min_jitter_seconds: f64,
    pub max_jitter_seconds: f64,
    /// Upstream's augmentation names (`AddBackgroundNoise`, `RIR`, …) and their probabilities.
    pub probabilities: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeatureSettings {
    pub positive_repetitions: u32,
    pub near_miss_repetitions: u32,
}

/// How often a feature set is drawn into a batch, and how much a wrong answer on it costs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeatureWeight {
    pub sampling_weight: f64,
    pub penalty_weight: f64,
}

/// microWakeWord's mixednet, as its command line spells it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelShape {
    pub pointwise_filters: String,
    pub repeat_in_block: String,
    pub mixconv_kernel_sizes: String,
    pub residual_connection: String,
    pub first_conv_filters: u32,
    pub first_conv_kernel_size: u32,
    pub stride: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrainingSettings {
    /// Steps of each phase; every other per-phase list has one entry per phase.
    pub steps: Vec<u32>,
    pub learning_rates: Vec<f64>,
    pub positive_class_weight: Vec<f64>,
    pub negative_class_weight: Vec<f64>,
    pub time_mask_max_size: Vec<u32>,
    pub time_mask_count: Vec<u32>,
    pub freq_mask_max_size: Vec<u32>,
    pub freq_mask_count: Vec<u32>,
    pub batch_size: u32,
    pub eval_step_interval: u32,
    pub clip_duration_ms: u32,
    pub target_minimization: f64,
    pub minimization_metric: Option<String>,
    pub maximization_metric: String,
    /// Per feature set: `positive`, `nearMiss`, each negative feature set, each speech group.
    pub feature_weights: BTreeMap<String, FeatureWeight>,
    pub model: ModelShape,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationSettings {
    /// Slices the model's output is averaged over: the manifest's `sliding_window_size`.
    pub sliding_window: u32,
    /// Slices after an accept in which no other accept counts.
    pub cooldown_slices: u32,
    /// The grid of cutoffs measured.
    pub cutoff_step: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub seed: u64,
    /// The device memory each stage may take, enforced on torch's and TensorFlow's allocators.
    pub device_memory_budget_gib: f64,
    pub samples: SampleSettings,
    /// Per language code.
    pub prosody: BTreeMap<String, Prosody>,
    pub augmentation: AugmentationSettings,
    pub features: FeatureSettings,
    pub training: TrainingSettings,
    pub evaluation: EvaluationSettings,
}

/// Apply `patch` to `target` as a JSON merge patch (RFC 7386): objects merge, `null` removes, any
/// other value replaces.
pub fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    let serde_json::Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = serde_json::Value::Object(Default::default());
    }
    let target = target.as_object_mut().expect("made an object just above");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_patch(
                target.entry(key.clone()).or_insert(serde_json::Value::Null),
                value,
            );
        }
    }
}

/// `defaults` with `overrides` merged over them, read back and checked.
pub fn with_overrides(
    defaults: &Settings,
    overrides: Option<&serde_json::Value>,
) -> Result<Settings, String> {
    let Some(overrides) = overrides else {
        return Ok(defaults.clone());
    };
    if !overrides.is_object() {
        return Err("settings must be an object".into());
    }
    let mut merged = serde_json::to_value(defaults).expect("settings serialise");
    merge_patch(&mut merged, overrides);
    let settings: Settings =
        serde_json::from_value(merged).map_err(|e| format!("settings: {e}"))?;
    settings.check()?;
    Ok(settings)
}

fn within<T: PartialOrd + std::fmt::Display>(
    field: &str,
    value: T,
    low: T,
    high: T,
) -> Result<(), String> {
    if value < low || value > high {
        return Err(format!(
            "settings.{field} must be between {low} and {high}, not {value}"
        ));
    }
    Ok(())
}

fn finite_within(field: &str, value: f64, low: f64, high: f64) -> Result<(), String> {
    if !value.is_finite() {
        return Err(format!("settings.{field} must be a number"));
    }
    within(field, value, low, high)
}

fn non_empty<T>(field: &str, list: &[T]) -> Result<(), String> {
    if list.is_empty() {
        return Err(format!("settings.{field} must not be empty"));
    }
    Ok(())
}

/// Longest any list of training phases may be.
const MAX_PHASES: usize = 8;
/// Most training steps in all phases together.  Safe range: up to a few million (hours per 100 000).
pub const MAX_TOTAL_STEPS: u64 = 1_000_000;

impl Settings {
    /// Every value in its range, every per-phase list as long as `steps`.
    pub fn check(&self) -> Result<(), String> {
        finite_within(
            "deviceMemoryBudgetGib",
            self.device_memory_budget_gib,
            1.0,
            80.0,
        )?;
        let s = &self.samples;
        within(
            "samples.positivePerLanguage",
            s.positive_per_language,
            100,
            200_000,
        )?;
        within(
            "samples.nearMissPerPhrasePerLanguage",
            s.near_miss_per_phrase_per_language,
            10,
            50_000,
        )?;
        within("samples.batchSize", s.batch_size, 1, 256)?;
        for (code, prosody) in &self.prosody {
            let lists = [
                ("lengthScales", &prosody.length_scales, 0.1, 3.0),
                ("noiseScaleWs", &prosody.noise_scale_ws, 0.0, 2.0),
                ("noiseScales", &prosody.noise_scales, 0.0, 2.0),
                ("slerpWeights", &prosody.slerp_weights, 0.0, 1.0),
            ];
            for (name, list, low, high) in lists {
                let field = format!("prosody.{code}.{name}");
                non_empty(&field, list)?;
                for value in list {
                    finite_within(&field, *value, low, high)?;
                }
            }
        }
        let a = &self.augmentation;
        finite_within(
            "augmentation.durationSeconds",
            a.duration_seconds,
            1.0,
            10.0,
        )?;
        finite_within(
            "augmentation.backgroundMinSnrDb",
            a.background_min_snr_db,
            -30.0,
            a.background_max_snr_db,
        )?;
        finite_within(
            "augmentation.backgroundMaxSnrDb",
            a.background_max_snr_db,
            -30.0,
            60.0,
        )?;
        finite_within(
            "augmentation.minJitterSeconds",
            a.min_jitter_seconds,
            0.0,
            a.max_jitter_seconds,
        )?;
        finite_within(
            "augmentation.maxJitterSeconds",
            a.max_jitter_seconds,
            0.0,
            a.duration_seconds,
        )?;
        for (name, probability) in &a.probabilities {
            finite_within(
                &format!("augmentation.probabilities.{name}"),
                *probability,
                0.0,
                1.0,
            )?;
        }
        within(
            "features.positiveRepetitions",
            self.features.positive_repetitions,
            1,
            10,
        )?;
        within(
            "features.nearMissRepetitions",
            self.features.near_miss_repetitions,
            1,
            10,
        )?;
        self.check_training()?;
        let e = &self.evaluation;
        within("evaluation.slidingWindow", e.sliding_window, 1, 50)?;
        within("evaluation.cooldownSlices", e.cooldown_slices, 1, 200)?;
        finite_within("evaluation.cutoffStep", e.cutoff_step, 0.001, 0.1)?;
        let points = 1.0 / e.cutoff_step;
        if (points - points.round()).abs() > 1e-9 {
            return Err(format!(
                "settings.evaluation.cutoffStep must divide 1 evenly, not {}",
                e.cutoff_step
            ));
        }
        Ok(())
    }

    fn check_training(&self) -> Result<(), String> {
        let t = &self.training;
        non_empty("training.steps", &t.steps)?;
        within("training.steps.len", t.steps.len(), 1, MAX_PHASES)?;
        for steps in &t.steps {
            within("training.steps", *steps, 1, MAX_TOTAL_STEPS as u32)?;
        }
        let total: u64 = t.steps.iter().map(|s| u64::from(*s)).sum();
        within("training.steps (total)", total, 1, MAX_TOTAL_STEPS)?;
        let phases = t.steps.len();
        let lengths = [
            ("learningRates", t.learning_rates.len()),
            ("positiveClassWeight", t.positive_class_weight.len()),
            ("negativeClassWeight", t.negative_class_weight.len()),
            ("timeMaskMaxSize", t.time_mask_max_size.len()),
            ("timeMaskCount", t.time_mask_count.len()),
            ("freqMaskMaxSize", t.freq_mask_max_size.len()),
            ("freqMaskCount", t.freq_mask_count.len()),
        ];
        for (name, len) in lengths {
            if len != phases {
                return Err(format!(
                    "settings.training.{name} needs one entry per phase ({phases}), not {len}"
                ));
            }
        }
        for rate in &t.learning_rates {
            finite_within("training.learningRates", *rate, 1e-7, 1.0)?;
        }
        for weight in t
            .positive_class_weight
            .iter()
            .chain(&t.negative_class_weight)
        {
            finite_within("training.classWeight", *weight, 0.0, 1000.0)?;
        }
        within("training.batchSize", t.batch_size, 1, 4096)?;
        within(
            "training.evalStepInterval",
            t.eval_step_interval,
            10,
            100_000,
        )?;
        within("training.clipDurationMs", t.clip_duration_ms, 500, 5000)?;
        for (name, weight) in &t.feature_weights {
            let field = format!("training.featureWeights.{name}");
            finite_within(&field, weight.sampling_weight, 0.0, 1000.0)?;
            finite_within(&field, weight.penalty_weight, 0.0, 1000.0)?;
        }
        let m = &t.model;
        within(
            "training.model.firstConvFilters",
            m.first_conv_filters,
            0,
            1024,
        )?;
        within(
            "training.model.firstConvKernelSize",
            m.first_conv_kernel_size,
            1,
            64,
        )?;
        within("training.model.stride", m.stride, 1, 10)?;
        Ok(())
    }

    /// Every phase's steps together.
    pub fn total_steps(&self) -> u64 {
        self.training.steps.iter().map(|s| u64::from(*s)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake_word::recipe::default_settings;
    use serde_json::json;

    #[test]
    fn the_defaults_pass_their_own_checks() {
        default_settings().check().unwrap();
    }

    #[test]
    fn no_overrides_are_the_defaults() {
        let defaults = default_settings();
        assert_eq!(with_overrides(&defaults, None).unwrap(), defaults);
    }

    #[test]
    fn an_override_changes_only_what_it_names() {
        let defaults = default_settings();
        let settings = with_overrides(
            &defaults,
            Some(
                &json!({"training": {"steps": [500], "learningRates": [0.001],
                "positiveClassWeight": [1], "negativeClassWeight": [20],
                "timeMaskMaxSize": [0], "timeMaskCount": [0], "freqMaskMaxSize": [0],
                "freqMaskCount": [0]}, "seed": 7}),
            ),
        )
        .unwrap();
        assert_eq!(settings.training.steps, [500]);
        assert_eq!(settings.seed, 7);
        assert_eq!(settings.samples, defaults.samples);
        assert_eq!(settings.training.model, defaults.training.model);
    }

    #[test]
    fn unknown_mistyped_and_out_of_range_overrides_are_refused_by_name() {
        let defaults = default_settings();
        let refused =
            |patch: serde_json::Value| with_overrides(&defaults, Some(&patch)).unwrap_err();
        assert!(refused(json!({"sampels": {}})).contains("sampels"));
        assert!(refused(json!({"samples": {"batchSize": "big"}})).contains("settings"));
        assert!(refused(json!({"samples": {"batchSize": 0}})).contains("samples.batchSize"));
        assert!(refused(json!({"training": {"steps": [1000, 1000, 1000]}}))
            .contains("needs one entry per phase (3)"));
        assert!(refused(json!({"evaluation": {"cutoffStep": 0.03}})).contains("divide 1 evenly"));
        assert!(refused(json!({"prosody": {"nl": {"lengthScales": []}}}))
            .contains("prosody.nl.lengthScales must not be empty"));
        assert!(refused(json!([1])).contains("must be an object"));
        assert!(refused(json!({"training": {"steps": [900000, 900000],
            "learningRates": [0.001, 0.001], "positiveClassWeight": [1, 1],
            "negativeClassWeight": [20, 20], "timeMaskMaxSize": [0, 0], "timeMaskCount": [0, 0],
            "freqMaskMaxSize": [0, 0], "freqMaskCount": [0, 0]}}))
        .contains("total"));
    }

    #[test]
    fn a_merge_patch_merges_objects_removes_nulls_and_replaces_the_rest() {
        let mut target = json!({"a": {"b": 1, "c": 2}, "d": [1, 2], "e": 5});
        merge_patch(
            &mut target,
            &json!({"a": {"b": 3, "c": null}, "d": [9], "f": {"g": 1}}),
        );
        assert_eq!(
            target,
            json!({"a": {"b": 3}, "d": [9], "e": 5, "f": {"g": 1}})
        );
        let mut scalar = json!(1);
        merge_patch(&mut scalar, &json!({"x": 1}));
        assert_eq!(scalar, json!({"x": 1}));
    }

    #[test]
    fn total_steps_adds_every_phase() {
        assert_eq!(default_settings().total_steps(), 30_000);
    }
}
