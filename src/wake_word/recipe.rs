//! Every pinned input of a training (`docs/engines/wake-word.md`, "Recipe").  Latest on 2026-10-25
//! unless a pin says why not; `DECISIONS.md` ("Wake word training") has the rest.

use std::collections::BTreeMap;

use serde::Serialize;

use super::settings::{
    AugmentationSettings, EvaluationSettings, FeatureSettings, FeatureWeight, ModelShape, Prosody,
    SampleSettings, Settings, TrainingSettings,
};
use crate::engine::torch::recipe::{Recipe, Source, SourceHome};

/// microWakeWord at its latest commit ("Pin dependencies", 2026-07-06).
pub const MICRO_WAKE_WORD: Source = Source {
    name: "microWakeWord",
    repo: "https://github.com/kahrendt/microWakeWord.git",
    rev: "4665173cd35f1cff9a61e06fc427f124766c488e",
    submodules: false,
    home: SourceHome::Sources,
};

/// piper-sample-generator 3.2.0 at its latest commit (2026-03-12).
pub const PIPER_SAMPLE_GENERATOR: Source = Source {
    name: "piper-sample-generator",
    repo: "https://github.com/rhasspy/piper-sample-generator.git",
    rev: "2971426a55072f7d22fec416ca7800df8bd23207",
    submodules: false,
    home: SourceHome::Sources,
};

/// The `wakeword` environment: torch for the generators, TensorFlow for microWakeWord, in one.
/// Both bring their CUDA libraries as wheels, so nothing is built against a toolkit.
pub const ENVIRONMENT: Recipe = Recipe {
    name: "wakeword",
    // TensorFlow 2.21 supports 3.10 to 3.13; the newest 3.12 `uv` 0.13.3 installs.
    python: "3.12.15",
    cuda: None,
    // cu126: the newest torch's CUDA 12 build, which TensorFlow's cu12 wheels sit beside.
    torch: &["torch==2.14.1"],
    torch_index: "https://download.pytorch.org/whl/cu126",
    packages: &[
        "tensorflow[and-cuda]==2.21.0",
        // TensorFlow 2.21 no longer pulls TensorBoard; upstream logs through `tf.summary`.
        "tensorboard==2.21.0",
        // 4.0 and later decode audio through torchcodec; 3.6.0 decodes with soundfile.
        "datasets==3.6.0",
        // microWakeWord needs AddColorNoise (0.35 and later); the generator's pin (0.33.0) is for
        // its own augment module, which the trainer does not use.
        "audiomentations==0.43.1",
        // piper-sample-generator's pin.
        "piper-tts==1.3.0",
        "webrtcvad-wheels==2.0.14.post1",
        "audio-metadata==0.11.1",
        "mmap-ninja==0.9.0",
        "pymicro-features==2.0.2",
        "pyyaml==6.0.3",
        "ai-edge-litert==2.3.0",
        "pyarrow==26.0.0",
        "soundfile==0.14.0",
        "scipy==1.18.1",
        // TensorFlow Lite Micro publishes nightly builds only.
        "tflite-micro==0.dev20261023182759",
    ],
    no_deps: &[],
    sources: &[MICRO_WAKE_WORD, PIPER_SAMPLE_GENERATOR],
    builds: &[],
    imports: &[
        "torch",
        "tensorflow",
        "piper",
        "mmap_ninja",
        "audiomentations",
        "datasets",
        "pymicro_features",
        "webrtcvad",
        "ai_edge_litert",
        "tflite_micro",
        "tensorboard",
        "soundfile",
        "pyarrow",
    ],
};

/// What a download is for, and so how the trainer prepares it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Role {
    /// microWakeWord's spectrogram features, zipped.
    NegativeFeatures,
    /// Audio mixed under the samples (`kind`: `parquet` or `zip`).
    BackgroundAudio { kind: &'static str },
    /// Room impulse responses, a parquet of audio.
    ImpulseResponses,
    /// A parquet of speech turned into features: a `group` of sets, each in its `split`.
    Speech {
        group: &'static str,
        split: &'static str,
        /// Hours of a validation set upstream's training weighs checkpoints on; the rest still
        /// chooses the cutoff.  `None`: training never sees it.
        #[serde(skip_serializing_if = "Option::is_none")]
        selection_hours: Option<f64>,
    },
    /// A multi-speaker piper generator for one language.
    Generator,
}

/// One pinned file.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    /// The dataset's name: its directory under `downloads/`, and the prepared set's name.
    pub name: String,
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
    pub licence: String,
    #[serde(flatten)]
    pub role: Role,
    /// What preparing it takes on disk, measured.
    pub prepared_bytes: u64,
}

const FEATURES: &str = "https://huggingface.co/datasets/kahrendt/microwakeword/resolve/0da95f94302ca2f4aae3b18fc6560fa6d2bba3d1";
const MLS_DUTCH: &str = "https://huggingface.co/datasets/facebook/multilingual_librispeech/resolve/2e83e61823b4c47dcbcb1980bb88601274127609/dutch";
const GENERATORS: &str =
    "https://github.com/rhasspy/piper-sample-generator/releases/download/v2.0.0";
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// Hours of MLS Dutch dev that upstream's training weighs checkpoints on.  Its validation holds
/// every ambient window in host memory twice: 22 h (all of Dutch dev beside CHiME-6) peaked at
/// 26 GB and the system's out-of-memory killer ended it (2026-10-25).  Safe range 1 to 6 h.
pub const SELECTION_HOURS: f64 = 3.2;

#[allow(clippy::too_many_arguments)]
fn download(
    name: &str,
    file: &str,
    url: String,
    sha256: &str,
    bytes: u64,
    licence: &str,
    role: Role,
    prepared_bytes: u64,
) -> Download {
    Download {
        name: name.into(),
        file: file.into(),
        url,
        sha256: sha256.into(),
        bytes,
        licence: licence.into(),
        role,
        prepared_bytes,
    }
}

/// Every dataset a training reads.  Prepared sizes measured on 2026-10-25.
pub fn datasets() -> Vec<Download> {
    let features = |name: &str, sha: &str, bytes: u64, prepared: u64| {
        download(
            name,
            &format!("{name}.zip"),
            format!("{FEATURES}/{name}.zip"),
            sha,
            bytes,
            "CC BY-NC 4.0",
            Role::NegativeFeatures,
            prepared,
        )
    };
    let mls = |name: &str, file: &str, sha: &str, bytes: u64, role: Role, prepared: u64| {
        download(
            name,
            file,
            format!("{MLS_DUTCH}/{file}"),
            sha,
            bytes,
            "CC BY 4.0",
            role,
            prepared,
        )
    };
    let speech = |group, split, selection_hours| Role::Speech {
        group,
        split,
        selection_hours,
    };
    let training = || speech("mls-dutch", "training", None);
    vec![
        features("speech", "68b6b811b347b8321169f9e75ea2b9be73245943b4470af9532ca288d9f1b3cc", 3_183_001_091, 14 * GIB),
        features("dinner_party", "18a0885d595ced7faa73736a8680206f5ba6e80113ca3e6ce43130e510aac18f", 444_310_142, 2_300 * MIB),
        features("no_speech", "722dbcc967275c64c9581ecb85729549830a9094ce605ee22b2ddab9dbabf3c8", 2_000_317_854, 9 * GIB),
        features("dinner_party_eval", "e08e23a5e654dd4415a1c41a4b6cd02d1f983c821512367ad937bac1aeed8299", 82_329_019, 420 * MIB),
        download(
            "audioset",
            "bal_train-09.parquet",
            "https://huggingface.co/datasets/agkphysics/AudioSet/resolve/0c609e8302cf139307f639c57652032af0a88041/data/bal_train/09.parquet".into(),
            "1121a990b52fdc9805adf9e6d704cc053c7fed02b5ce6f5b83cf73d3e9eccad7",
            692_591_080,
            "CC BY 4.0",
            Role::BackgroundAudio { kind: "parquet" },
            160 * MIB,
        ),
        download(
            "fma",
            "fma_xs.zip",
            "https://huggingface.co/datasets/mchl914/fma_xsmall/resolve/bfef24fd1e1334e4631b917c98e0dbc3dd653ab2/fma_xs.zip".into(),
            "e5876c13cfb0f7ef668327c75d1c40bc4a2ed9d5b8e62ce383d093319c9ff663",
            182_339_695,
            "MIT (dataset); tracks CC",
            Role::BackgroundAudio { kind: "zip" },
            200 * MIB,
        ),
        download(
            "mit-rirs",
            "mit_rirs.parquet",
            "https://huggingface.co/datasets/davidscripka/MIT_environmental_impulse_responses/resolve/08b8fcb13902c0f20137f62318eb719677b53f58/default/train/0000.parquet".into(),
            "5fd1752cf5677a7d2f0148da8c7946a5c635b9740782489fe518c2c27b98d41e",
            8_110_043,
            "unknown",
            Role::ImpulseResponses,
            6 * MIB,
        ),
        mls("mls-dutch-9h", "9_hours-00000-of-00001.parquet", "173586e5abe761359d5d6906d630f6826a66e41a588945c0add7a8ecdb883c74", 139_332_979, training(), 500 * MIB),
        // Two of the 48 training shards (about 32 h each), from both ends for different speakers.
        mls("mls-dutch-train-00", "train-00000-of-00048.parquet", "d5197c6697455438f0f22a32e8108fd9295b8f21071d46c06f8ef5822e879b1b", 498_278_678, training(), 2 * GIB),
        mls("mls-dutch-train-24", "train-00024-of-00048.parquet", "f55d61f3301ef7f59463129a13b2b4bb0e969d616f3250be621a8024a515bc10", 496_357_824, training(), 2 * GIB),
        mls("mls-dutch-dev", "dev-00000-of-00001.parquet", "0c40d8db30f3cc86041430fb43f091768cba1e65b0b27d8cc3cda02e28571881", 198_983_422, speech("mls-dutch-eval", "validation_ambient", Some(SELECTION_HOURS)), 900 * MIB),
        mls("mls-dutch-test", "test-00000-of-00001.parquet", "feb88a282be746e7c14e1f09c017b4f69b07df9c25c94a3943eaac5ca7794ad5", 198_390_898, speech("mls-dutch-eval", "testing_ambient", None), 700 * MIB),
    ]
}

/// A language the synthetic voices speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
    Nl,
    De,
    Fr,
}

impl Language {
    pub const ALL: [Language; 4] = [Language::En, Language::Nl, Language::De, Language::Fr];

    pub fn code(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::Nl => "nl",
            Language::De => "de",
            Language::Fr => "fr",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.code() == code)
    }

    /// The generator's file: its download, and its config in piper-sample-generator's `models/`.
    pub fn generator_file(self) -> &'static str {
        match self {
            Language::En => "en_US-libritts_r-medium.pt",
            Language::Nl => "nl_NL-mls-medium.pt",
            Language::De => "de_DE-mls-medium.pt",
            Language::Fr => "fr_FR-mls-medium.pt",
        }
    }

    /// Speakers used.  LibriTTS-R's last speakers had few samples and carry artefacts
    /// (piper-sample-generator's README); the MLS generators have fewer, all usable.
    pub fn max_speakers(self) -> u32 {
        match self {
            Language::En => 800,
            Language::Nl | Language::De | Language::Fr => 1000,
        }
    }

    pub fn generator(self) -> Download {
        let (sha, bytes) = match self {
            Language::En => (
                "e95ee53770bf598c354a6e6dbfc95ccb259aeeb501d35a86be8a767429ab0ff6",
                204_089_915,
            ),
            Language::Nl => (
                "3241a3a9f2abc675b477547991989b6bdcc4d42b476a0fe754bba180b681d0d9",
                202_303_019,
            ),
            Language::De => (
                "67c08d2f76a424b2e1a514e316c6fb5959cc35f168c0ddf6da7cecb7456f79c7",
                202_679_851,
            ),
            Language::Fr => (
                "b3311993554e771d9b3a13630ffa82c0649c7dafcfd05f3425595d491b06dd0a",
                202_452_523,
            ),
        };
        let file = self.generator_file();
        download(
            "piper-generators",
            file,
            format!("{GENERATORS}/{file}"),
            sha,
            bytes,
            "MIT",
            Role::Generator,
            0,
        )
    }

    /// How the language's voices speak by default (measured 2026-10-25: these speeds put a
    /// two-word phrase at a median 0.6 to 1.1 s voiced).
    fn prosody(self) -> Prosody {
        let (length_scales, noise_scale_w) = match self {
            // upstream's defaults
            Language::En => (vec![0.75, 1.0, 1.25], 0.8),
            // MLS audiobook readers: slow, with long pauses at upstream's duration noise
            Language::Nl => (vec![0.6, 0.7, 0.85], 0.4),
            Language::De => (vec![0.35, 0.45, 0.55], 0.4),
            Language::Fr => (vec![0.45, 0.55, 0.7], 0.4),
        };
        Prosody {
            length_scales,
            noise_scale_ws: vec![noise_scale_w],
            noise_scales: vec![0.667],
            slerp_weights: vec![0.5],
        }
    }
}

/// The ambient sets the evaluate stage streams through the model: `(name, split, directory under
/// the datasets root)`.  Validation chooses the cutoff; testing reports.
pub const AMBIENT_SETS: [(&str, &str, &str); 4] = [
    (
        "chime6",
        "validation",
        "negative-features/dinner_party_eval/validation_ambient",
    ),
    (
        "mls-dutch-dev",
        "validation",
        "mls-dutch-eval-features/validation_ambient",
    ),
    (
        "dipco",
        "testing",
        "negative-features/dinner_party_eval/testing_ambient",
    ),
    (
        "mls-dutch-test",
        "testing",
        "mls-dutch-eval-features/testing_ambient",
    ),
];

/// microWakeWord's notebook settings, with the sample counts, phases and Dutch speech of this
/// recipe.
pub fn default_settings() -> Settings {
    let weight = |sampling_weight, penalty_weight| FeatureWeight {
        sampling_weight,
        penalty_weight,
    };
    Settings {
        seed: 1,
        // Training validates on 22 h of ambient windows in one GPU tensor (CHiME-6 + MLS Dutch dev).
        device_memory_budget_gib: 10.0,
        samples: SampleSettings {
            positive_per_language: 10_000,
            near_miss_per_phrase_per_language: 1_000,
            batch_size: 32,
        },
        prosody: Language::ALL
            .into_iter()
            .map(|l| (l.code().to_string(), l.prosody()))
            .collect(),
        augmentation: AugmentationSettings {
            duration_seconds: 3.2,
            background_min_snr_db: -5.0,
            background_max_snr_db: 10.0,
            min_jitter_seconds: 0.195,
            max_jitter_seconds: 0.205,
            probabilities: BTreeMap::from(
                [
                    ("SevenBandParametricEQ", 0.1),
                    ("TanhDistortion", 0.1),
                    ("PitchShift", 0.1),
                    ("BandStopFilter", 0.1),
                    ("AddColorNoise", 0.1),
                    ("AddBackgroundNoise", 0.75),
                    ("Gain", 1.0),
                    ("RIR", 0.5),
                ]
                .map(|(k, v)| (k.to_string(), v)),
            ),
        },
        features: FeatureSettings {
            positive_repetitions: 2,
            near_miss_repetitions: 1,
        },
        training: TrainingSettings {
            // Both full runs kept a checkpoint from before step 6 500 of 30 000 (2026-10-25).
            steps: vec![10_000, 5_000],
            learning_rates: vec![0.001, 0.0001],
            positive_class_weight: vec![1.0, 1.0],
            negative_class_weight: vec![20.0, 20.0],
            time_mask_max_size: vec![0, 0],
            time_mask_count: vec![0, 0],
            freq_mask_max_size: vec![0, 0],
            freq_mask_count: vec![0, 0],
            batch_size: 128,
            eval_step_interval: 500,
            clip_duration_ms: 1500,
            target_minimization: 0.9,
            minimization_metric: None,
            maximization_metric: "average_viable_recall".into(),
            feature_weights: BTreeMap::from(
                [
                    ("positive", weight(2.0, 1.0)),
                    ("nearMiss", weight(2.0, 1.0)),
                    ("speech", weight(10.0, 1.0)),
                    ("dinner_party", weight(10.0, 1.0)),
                    ("no_speech", weight(5.0, 1.0)),
                    ("dinner_party_eval", weight(0.0, 1.0)),
                    // Dutch speech gave 11 of 13 testing false accepts at 3 (2026-10-25).
                    ("mls-dutch", weight(6.0, 1.0)),
                    // Never sampled: its validation audio only weighs which checkpoint is kept.
                    ("mls-dutch-eval", weight(0.0, 1.0)),
                ]
                .map(|(k, v)| (k.to_string(), v)),
            ),
            model: ModelShape {
                pointwise_filters: "64,64,64,64".into(),
                repeat_in_block: "1, 1, 1, 1".into(),
                mixconv_kernel_sizes: "[5], [7,11], [9,15], [23]".into(),
                residual_connection: "0,0,0,0".into(),
                first_conv_filters: 32,
                first_conv_kernel_size: 5,
                stride: 3,
            },
        },
        evaluation: EvaluationSettings {
            sliding_window: 5,
            cooldown_slices: 25,
            cutoff_step: 0.01,
        },
    }
}

/// The feature sets the training's weights must name: positives, near misses, each negative
/// feature set and each speech group (a group weighed 0 is never sampled: its validation audio only
/// counts when upstream picks the checkpoint to keep).
pub fn weighted_sets(datasets: &[Download]) -> Vec<String> {
    let mut names = vec!["positive".to_string(), "nearMiss".to_string()];
    for d in datasets {
        match &d.role {
            Role::NegativeFeatures => names.push(d.name.clone()),
            Role::Speech { group, .. } if !names.iter().any(|n| n == group) => {
                names.push((*group).to_string());
            }
            _ => {}
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pin_is_exact_and_every_download_is_https_with_a_sha256() {
        for package in ENVIRONMENT.packages.iter().chain(ENVIRONMENT.torch) {
            assert!(package.contains("=="), "{package} is pinned");
        }
        let mut all = datasets();
        all.extend(Language::ALL.map(Language::generator));
        for d in all {
            assert!(d.url.starts_with("https://"), "{}", d.url);
            assert_eq!(d.sha256.len(), 64, "{}", d.name);
            assert!(d.bytes > 0, "{}", d.name);
        }
        for source in ENVIRONMENT.sources {
            assert_eq!(
                source.rev.len(),
                40,
                "{} is pinned to a commit",
                source.name
            );
        }
    }

    #[test]
    fn dutch_training_speech_spans_the_9_hour_set_and_two_full_shards() {
        let training: Vec<String> = datasets()
            .into_iter()
            .filter(|d| {
                matches!(
                    d.role,
                    Role::Speech {
                        split: "training",
                        ..
                    }
                )
            })
            .map(|d| d.name)
            .collect();
        assert_eq!(
            training,
            ["mls-dutch-9h", "mls-dutch-train-00", "mls-dutch-train-24"]
        );
    }

    #[test]
    fn only_the_dutch_dev_audio_is_capped_for_checkpoint_selection() {
        let capped: Vec<(String, f64)> = datasets()
            .into_iter()
            .filter_map(|d| match d.role {
                Role::Speech {
                    selection_hours: Some(hours),
                    ..
                } => Some((d.name, hours)),
                _ => None,
            })
            .collect();
        assert_eq!(capped, [("mls-dutch-dev".to_string(), SELECTION_HOURS)]);
    }

    #[test]
    fn the_dutch_validation_audio_is_weighed_but_never_trained_on() {
        let sets = weighted_sets(&datasets());
        assert!(sets.contains(&"mls-dutch-eval".to_string()), "{sets:?}");
        let weight = default_settings().training.feature_weights["mls-dutch-eval"];
        assert_eq!(weight.sampling_weight, 0.0);
    }

    #[test]
    fn the_default_weights_name_every_trained_set() {
        let weights = default_settings().training.feature_weights;
        for name in weighted_sets(&datasets()) {
            assert!(weights.contains_key(&name), "{name} has a weight");
        }
    }

    #[test]
    fn every_language_has_a_default_prosody_and_parses_back() {
        let settings = default_settings();
        for language in Language::ALL {
            assert!(settings.prosody.contains_key(language.code()));
            assert_eq!(Language::parse(language.code()), Some(language));
            assert_eq!(language.generator().file, language.generator_file());
        }
        assert_eq!(Language::parse("xx"), None);
    }

    #[test]
    fn the_environment_builds_nothing_against_a_toolkit() {
        assert_eq!(ENVIRONMENT.cuda, None);
        assert!(ENVIRONMENT.builds.is_empty());
    }
}
