//! The trainer's side of a training: its embedded Python files, where everything lives on disk,
//! the `request.json` every stage reads, and the variables every stage sees
//! (`docs/engines/wake-word.md`, "Layout").

use std::path::{Path, PathBuf};

use serde_json::json;

use super::recipe::{
    Download, Language, Role, AMBIENT_SETS, MICRO_WAKE_WORD, PIPER_SAMPLE_GENERATOR,
};
use super::request::{slug, TrainingSpec};
use crate::engine::torch::provision::Environment;

/// The trainer's Python files, embedded so a worker carries the version it was built with.
pub const FILES: [(&str, &str); 8] = [
    (
        "wakeword/__init__.py",
        include_str!("../../python/wakeword/__init__.py"),
    ),
    (
        "wakeword/__main__.py",
        include_str!("../../python/wakeword/__main__.py"),
    ),
    (
        "wakeword/common.py",
        include_str!("../../python/wakeword/common.py"),
    ),
    (
        "wakeword/prepare.py",
        include_str!("../../python/wakeword/prepare.py"),
    ),
    (
        "wakeword/samples.py",
        include_str!("../../python/wakeword/samples.py"),
    ),
    (
        "wakeword/features.py",
        include_str!("../../python/wakeword/features.py"),
    ),
    (
        "wakeword/train.py",
        include_str!("../../python/wakeword/train.py"),
    ),
    (
        "wakeword/evaluate.py",
        include_str!("../../python/wakeword/evaluate.py"),
    ),
];

/// Speech clips are joined into tracks this long before they are featurised, so an ambient set
/// streams like a device listens.  Safe range 60 to 3600 s.
pub const SPEECH_TRACK_SECONDS: u32 = 600;

/// Where wake word training keeps everything: under `<models_root>/wake-word/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn new(models_root: &Path) -> Self {
        Self {
            root: models_root.join("wake-word"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn download(&self, download: &Download) -> PathBuf {
        self.root
            .join("downloads")
            .join(&download.name)
            .join(&download.file)
    }

    pub fn datasets(&self) -> PathBuf {
        self.root.join("datasets")
    }

    /// The marker the prepare stage leaves beside a prepared dataset.
    pub fn prepared_marker(&self, download: &Download) -> Option<PathBuf> {
        let datasets = self.datasets();
        let dir = match &download.role {
            Role::NegativeFeatures => datasets.join("negative-features").join(&download.name),
            Role::BackgroundAudio { .. } | Role::ImpulseResponses => {
                datasets.join(format!("{}-16k", download.name))
            }
            Role::Speech { group, split } => datasets
                .join(format!("{group}-features"))
                .join(split)
                .join(&download.name),
            Role::Generator => return None,
        };
        let mut marker = dir.into_os_string();
        marker.push(".ready");
        Some(PathBuf::from(marker))
    }

    pub fn trainer(&self) -> PathBuf {
        self.root.join("trainer")
    }

    pub fn run(&self, id: &str) -> PathBuf {
        self.root.join("runs").join(id)
    }

    pub fn output(&self, name: &str, id: &str) -> PathBuf {
        self.root.join("models").join(name).join(id)
    }
}

fn path_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A source's checkout in the environment.
pub fn source_dir(environment: &Environment, name: &str) -> PathBuf {
    environment
        .sources
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, dir)| dir.clone())
        .unwrap_or_else(|| panic!("the wakeword environment checks {name} out"))
}

/// `request.json`: everything the stages read, with every path spelled out.
pub fn request_json(
    spec: &TrainingSpec,
    paths: &Paths,
    run_dir: &Path,
    environment: &Environment,
    datasets: &[Download],
) -> serde_json::Value {
    let piper = source_dir(environment, PIPER_SAMPLE_GENERATOR.name);
    let languages: Vec<_> = spec
        .languages
        .iter()
        .map(|language| {
            let generator = language.generator();
            json!({
                "code": language.code(),
                "generator": path_of(&paths.download(&generator)),
                "config": path_of(&piper.join("models").join(format!("{}.json", generator.file))),
                "maxSpeakers": language.max_speakers(),
                "spellings": spec.spellings[language],
                "prosody": spec.settings.prosody[language.code()],
            })
        })
        .collect();
    let entry = |d: &Download| {
        let mut entry =
            json!({"name": d.name, "path": path_of(&paths.download(d)), "sha256": d.sha256});
        match &d.role {
            Role::BackgroundAudio { kind } => entry["kind"] = json!(kind),
            Role::ImpulseResponses => entry["kind"] = json!("parquet"),
            Role::Speech { group, split } => {
                entry["group"] = json!(group);
                entry["split"] = json!(split);
                entry["trackSeconds"] = json!(SPEECH_TRACK_SECONDS);
            }
            Role::NegativeFeatures | Role::Generator => {}
        }
        entry
    };
    let of = |want: fn(&Role) -> bool| {
        datasets
            .iter()
            .filter(|d| want(&d.role))
            .map(entry)
            .collect::<Vec<_>>()
    };
    let impulse = of(|r| matches!(r, Role::ImpulseResponses));
    let datasets_root = paths.datasets();
    json!({
        "name": spec.name,
        "phrase": spec.phrase,
        "seed": spec.settings.seed,
        "workDir": path_of(run_dir),
        "languages": languages,
        "negatives": spec.negatives.iter().map(|text| json!({"text": text, "slug": slug(text)})).collect::<Vec<_>>(),
        "datasets": {
            "root": path_of(&datasets_root),
            "negativeFeatures": of(|r| matches!(r, Role::NegativeFeatures)),
            "backgroundAudio": of(|r| matches!(r, Role::BackgroundAudio { .. })),
            "impulseResponses": impulse.first().cloned().unwrap_or(serde_json::Value::Null),
            "speech": of(|r| matches!(r, Role::Speech { .. })),
        },
        "evaluation": {
            "ambient": AMBIENT_SETS.iter().map(|(name, split, dir)| json!({
                "name": name, "split": split, "dir": path_of(&datasets_root.join(dir)),
            })).collect::<Vec<_>>(),
        },
        "settings": spec.settings,
    })
}

/// The `lib` directories of the CUDA wheels in `site_packages`: TensorFlow finds them only on
/// `LD_LIBRARY_PATH`.
pub fn cuda_wheel_libraries(site_packages: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(site_packages.join("nvidia")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("lib"))
        .filter(|lib| lib.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The variables every stage sees besides the provisioner's.
pub fn stage_env(
    environment: &Environment,
    trainer_dir: &Path,
    python: &str,
) -> Vec<(String, String)> {
    let site_packages = crate::engine::torch::cuda::site_packages(&environment.venv(), python);
    let mut libraries: Vec<String> = cuda_wheel_libraries(&site_packages)
        .iter()
        .map(|p| path_of(p))
        .collect();
    if let Some(inherited) = std::env::var("LD_LIBRARY_PATH")
        .ok()
        .filter(|v| !v.is_empty())
    {
        libraries.push(inherited);
    }
    let python_path = [
        path_of(trainer_dir),
        path_of(&source_dir(environment, MICRO_WAKE_WORD.name)),
        path_of(&source_dir(environment, PIPER_SAMPLE_GENERATOR.name)),
    ]
    .join(":");
    vec![
        ("PYTHONPATH".into(), python_path),
        ("LD_LIBRARY_PATH".into(), libraries.join(":")),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("TQDM_DISABLE".into(), "1".into()),
        ("HF_HUB_DISABLE_TELEMETRY".into(), "1".into()),
        ("HF_DATASETS_OFFLINE".into(), "1".into()),
        ("TF_FORCE_GPU_ALLOW_GROWTH".into(), "true".into()),
        ("TF_CPP_MIN_LOG_LEVEL".into(), "1".into()),
        (
            "PYTORCH_CUDA_ALLOC_CONF".into(),
            "expandable_segments:True".into(),
        ),
    ]
}

/// The generators of `spec`'s languages.
pub fn generators(spec: &TrainingSpec) -> Vec<Download> {
    spec.languages
        .iter()
        .map(|l: &Language| l.generator())
        .collect()
}

// The trainer runs on Linux only: its paths and `PYTHONPATH` are spelled the Unix way.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::wake_word::recipe::datasets;
    use crate::wake_word::request;

    fn environment(dir: &Path) -> Environment {
        Environment {
            dir: dir.join("env"),
            python: dir.join("env/.venv/bin/python"),
            cuda_home: dir.join("env/cuda"),
            sources: vec![
                ("microWakeWord".into(), dir.join("src/mww")),
                ("piper-sample-generator".into(), dir.join("src/psg")),
            ],
        }
    }

    #[test]
    fn the_request_spells_out_every_path_a_stage_reads() {
        let dir = tempfile::tempdir().unwrap();
        let spec = request::parse(
            br#"{"phrase":"hey runa","languages":["en","nl"],"spellings":{"en":["hey roona"]},"negatives":["hey luna"]}"#,
        )
        .unwrap();
        let paths = Paths::new(dir.path());
        let run = paths.run("local-1");
        let json = request_json(&spec, &paths, &run, &environment(dir.path()), &datasets());
        let root = dir.path().join("wake-word");
        assert_eq!(json["workDir"], path_of(&root.join("runs/local-1")));
        assert_eq!(json["languages"][0]["code"], "en");
        assert_eq!(json["languages"][0]["spellings"], json!(["hey roona"]));
        assert_eq!(
            json["languages"][1]["generator"],
            path_of(&root.join("downloads/piper-generators/nl_NL-mls-medium.pt"))
        );
        assert_eq!(
            json["languages"][1]["config"],
            path_of(&dir.path().join("src/psg/models/nl_NL-mls-medium.pt.json"))
        );
        assert_eq!(
            json["languages"][1]["prosody"]["lengthScales"],
            json!([0.6, 0.7, 0.85])
        );
        assert_eq!(
            json["negatives"],
            json!([{"text": "hey luna", "slug": "hey_luna"}])
        );
        assert_eq!(
            json["datasets"]["negativeFeatures"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(json["datasets"]["backgroundAudio"][1]["kind"], "zip");
        assert_eq!(json["datasets"]["impulseResponses"]["kind"], "parquet");
        assert_eq!(json["datasets"]["speech"][0]["group"], "mls-dutch");
        assert_eq!(json["datasets"]["speech"][2]["split"], "testing_ambient");
        assert_eq!(json["evaluation"]["ambient"][2]["name"], "dipco");
        assert_eq!(json["settings"]["samples"]["positivePerLanguage"], 10_000);
    }

    #[test]
    fn every_dataset_but_a_generator_has_a_prepared_marker() {
        let paths = Paths::new(Path::new("/m"));
        let markers: Vec<_> = datasets()
            .iter()
            .filter_map(|d| paths.prepared_marker(d))
            .collect();
        assert_eq!(markers.len(), datasets().len());
        assert!(markers.contains(&PathBuf::from(
            "/m/wake-word/datasets/negative-features/speech.ready"
        )));
        assert!(markers.contains(&PathBuf::from("/m/wake-word/datasets/fma-16k.ready")));
        assert!(markers.contains(&PathBuf::from(
            "/m/wake-word/datasets/mls-dutch-eval-features/testing_ambient/mls-dutch-test.ready"
        )));
        assert_eq!(paths.prepared_marker(&Language::En.generator()), None);
        assert_eq!(
            paths.output("hey_runa", "id"),
            PathBuf::from("/m/wake-word/models/hey_runa/id")
        );
        assert_eq!(paths.trainer(), PathBuf::from("/m/wake-word/trainer"));
    }

    #[test]
    fn stages_find_the_trainer_both_sources_and_the_cuda_wheels() {
        let dir = tempfile::tempdir().unwrap();
        let env = environment(dir.path());
        let site = crate::engine::torch::cuda::site_packages(&env.venv(), "3.12.15");
        for lib in ["cudnn", "cublas"] {
            std::fs::create_dir_all(site.join("nvidia").join(lib).join("lib")).unwrap();
        }
        std::fs::create_dir_all(site.join("nvidia").join("not-a-lib")).unwrap();
        let vars = stage_env(&env, &dir.path().join("trainer"), "3.12.15");
        let var = |name: &str| vars.iter().find(|(k, _)| k == name).unwrap().1.clone();
        let python_path = var("PYTHONPATH");
        assert!(python_path.starts_with(&path_of(&dir.path().join("trainer"))));
        assert!(python_path.contains("src/mww") && python_path.contains("src/psg"));
        let ld = var("LD_LIBRARY_PATH");
        assert!(
            ld.contains("nvidia/cublas/lib") && ld.contains("nvidia/cudnn/lib"),
            "{ld}"
        );
        assert!(!ld.contains("not-a-lib"), "{ld}");
        assert_eq!(var("TF_FORCE_GPU_ALLOW_GROWTH"), "true");
        assert!(cuda_wheel_libraries(&dir.path().join("nowhere")).is_empty());
    }

    #[test]
    fn the_embedded_files_install_into_the_trainer_directory() {
        let dir = tempfile::tempdir().unwrap();
        crate::engine::torch::runner::install_files(dir.path(), &FILES).unwrap();
        let main = std::fs::read_to_string(dir.path().join("wakeword/__main__.py")).unwrap();
        assert!(main.contains(
            "STAGES = (\"prepare\", \"samples\", \"features\", \"train\", \"evaluate\")"
        ));
    }
}
