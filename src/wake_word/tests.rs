//! A whole training, end to end, with a fake Python standing in for the environment: every stage
//! answers as the trainer does, so every path of the job runs without a GPU.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::recipe::{Download, Language, Role};
use super::stage::Phase;
use super::training::{Backend, Provisioned, Refusal, State, Training, WakeWords};
use super::{request, TrainingError};
use crate::engine::torch::platform::ComputeCapability;
use crate::engine::torch::provision::Environment;
use crate::job_gate::JobGate;
use crate::runtime::{JobOutcome, WorkerObservers};
use crate::types::TaskKind;

/// The trainer's stages, faked: `FAKE_FAIL=<stage>:<class>:<exit>` fails a stage,
/// `FAKE_SLEEP=<stage>` hangs one, `FAKE_FA=<n>` sets validation's false accepts at the middle
/// cutoff, `FAKE_NO_EVALUATION=1` leaves no evaluation.
const FAKE_PYTHON: &str = r#"#!/bin/sh
stage="$3"; req="$4"
work=$(sed -n 's/^ *"workDir": "\([^"]*\)".*$/\1/p' "$req" | head -1)
echo "{\"event\":\"stage\",\"name\":\"$stage\"}"
echo "[wakeword] fake $stage" >&2
case "$FAKE_FAIL" in
  "$stage":*)
    class=$(echo "$FAKE_FAIL" | cut -d: -f2); code=$(echo "$FAKE_FAIL" | cut -d: -f3)
    echo "{\"event\":\"failed\",\"class\":\"$class\",\"message\":\"fake $class\"}"
    exit "$code";;
esac
[ "$FAKE_SLEEP" = "$stage" ] && sleep 30
case "$stage" in
  samples)
    echo '{"event":"phonemes","language":"en","text":"hey roona","role":"positive","phonemes":"hˈeɪ ɹˈuːnə"}'
    echo '{"event":"progress","stage":"samples positive en","done":1,"total":1}'
    mkdir -p "$work/samples";;
  features) mkdir -p "$work/features/positive"; [ -n "$FAKE_OWN" ] && mkdir -p "$work/features/own_positive" "$work/own_voice";;
  train) mkdir -p "$work/train"; printf 'TFL3' > "$work/train/model.tflite";;
  evaluate)
    own=""; [ -n "$FAKE_OWN" ] && own=",\"ownVoice\":{\"heldOut\":[$FAKE_OWN]}"
    [ -n "$FAKE_NO_EVALUATION" ] || cat > "$work/evaluation.json" <<EOF
{"cutoffs":[0.0,0.5,1.0],"slidingWindow":5,"cooldownSlices":25,"stride":3,
 "ambient":[{"name":"chime6","split":"validation","hours":10,"falseAccepts":[50,${FAKE_FA:-0},${FAKE_FA_TOP:-0}]},
            {"name":"dipco","split":"testing","hours":5,"falseAccepts":[20,1,0]}],
 "positive":[{"language":"en","split":"validation","scores":[0.9,0.8,0.3]},
             {"language":"en","split":"testing","scores":[0.95,0.4]}],
 "nearMiss":[{"phrase":"hey_luna","split":"testing","scores":[0.7,0.1]}],
 "model":{"path":"$work/train/model.tflite","bytes":4,"sha256":"ab","tensorArenaMinBytes":25609}$own}
EOF
    ;;
esac
echo "{\"event\":\"done\",\"stage\":\"$stage\",\"seconds\":1.5,\"deviceMemoryPeakMib\":100,\"summary\":{}}"
"#;

struct Harness {
    _dir: tempfile::TempDir,
    store: crate::voice_samples::VoiceSamples,
    root: PathBuf,
    observers: WorkerObservers,
    gate: JobGate,
    service: WakeWords,
}

fn environment(root: &Path) -> Environment {
    let dir = root.join("env");
    let python = dir.join(".venv/bin/python");
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    std::fs::write(&python, FAKE_PYTHON).unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    Environment {
        python,
        cuda_home: dir.join("cuda"),
        sources: vec![
            ("microWakeWord".into(), root.join("src/mww")),
            ("piper-sample-generator".into(), root.join("src/psg")),
        ],
        dir,
    }
}

/// One dataset, already on disk, so no test downloads anything it does not mean to.
fn dataset(name: &str, url: &str) -> Download {
    Download {
        name: name.into(),
        file: format!("{name}.zip"),
        url: url.into(),
        sha256: "0".repeat(64),
        bytes: 10,
        licence: "test".into(),
        role: Role::NegativeFeatures,
        prepared_bytes: 10,
    }
}

struct Options {
    env: Vec<(&'static str, &'static str)>,
    gpu: Result<ComputeCapability, String>,
    free: Option<u64>,
    datasets: Vec<Download>,
    provision_fails: bool,
    /// Keep recordings: the store, holding these takes.
    voice_samples: Option<Vec<(&'static str, crate::voice_samples::Take)>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            env: vec![],
            gpu: Ok(ComputeCapability { major: 8, minor: 9 }),
            free: None,
            datasets: vec![dataset("speech", "https://example.invalid/speech.zip")],
            provision_fails: false,
            voice_samples: None,
        }
    }
}

fn harness(options: Options) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("models");
    let paths = super::trainer::Paths::new(&root);
    for d in options
        .datasets
        .iter()
        .filter(|d| d.url.starts_with("https://example.invalid"))
    {
        let file = paths.download(d);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"x").unwrap();
    }
    let generator = paths.download(&Language::En.generator());
    std::fs::create_dir_all(generator.parent().unwrap()).unwrap();
    std::fs::write(generator, b"x").unwrap();
    let env_root = dir.path().to_path_buf();
    let vars: Vec<(String, String)> = options
        .env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let gpu = options.gpu.clone();
    let free = options.free;
    let provision_fails = options.provision_fails;
    let backend = Backend {
        gpu: Box::new(move || gpu.clone()),
        provision: Box::new(move |_, _| {
            if provision_fails {
                return Err(TrainingError::ProvisionFailed("packages: exit 1".into()));
            }
            Ok(Provisioned {
                environment: environment(&env_root),
                env: vars.clone(),
                freeze: vec!["torch==2.14.1+cu126".into()],
            })
        }),
        describe_gpu: Box::new(|| Some("NVIDIA GeForce RTX 4090, 595.99.02, 24564 MiB".into())),
        free_space: Box::new(move |_| free),
        datasets: options.datasets,
        stall: Duration::from_secs(2),
    };
    let observers = WorkerObservers::default();
    let gate = JobGate::new();
    let mut service =
        WakeWords::with_backend(root.clone(), observers.clone(), gate.clone(), backend);
    let store = crate::voice_samples::VoiceSamples::new(dir.path().join("config/voice-samples"));
    if let Some(takes) = options.voice_samples {
        for (set, take) in takes {
            let audio = crate::voice_samples::take::tests::wav(2.0);
            store.put(set, &take, &audio).unwrap();
        }
        service = service.with_voice_samples(store.clone());
    }
    Harness {
        _dir: dir,
        store,
        root,
        observers,
        gate,
        service,
    }
}

fn spec() -> request::TrainingSpec {
    request::parse(
        br#"{"phrase":"hey runa","spellings":{"en":["hey roona"]},"negatives":["hey luna"],
             "author":"Webber Takken"}"#,
    )
    .unwrap()
}

fn wait_until_ended(h: &Harness, id: &str) -> Training {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let training = h.service.get(id).expect("the training is listed");
        if training.state != State::Running {
            // The job is recorded right after the state ends; give it the moment.
            let recorded = Instant::now() + Duration::from_secs(2);
            while h.observers.local_jobs.lock().iter().all(|j| j.job_id != id)
                && Instant::now() < recorded
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            return training;
        }
        assert!(
            Instant::now() < deadline,
            "still {:?} at {:?}",
            training.state,
            training.stage
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_training_runs_every_stage_and_writes_the_model_its_manifest_and_report() {
    let h = harness(Options::default());
    let started = h.service.start(spec()).unwrap();
    assert_eq!(started.state, State::Running);
    assert!(h.gate.is_busy(), "the training holds the worker");

    let training = wait_until_ended(&h, &started.id);
    assert_eq!(training.state, State::Completed, "{:?}", training.failure);
    let result = training.result.unwrap();
    assert_eq!(result.cutoff, 0.5);
    assert_eq!(result.recall, 0.5);
    assert_eq!(result.false_accepts_per_hour, 0.2);
    assert!(result.target_met);
    let out = h.root.join("wake-word/models/hey_runa").join(&started.id);
    assert_eq!(training.output_dir, out);
    assert_eq!(result.model, out.join("hey_runa.tflite"));
    assert_eq!(std::fs::read(&result.model).unwrap(), b"TFL3");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&result.manifest).unwrap()).unwrap();
    assert_eq!(manifest["wake_word"], "Hey Runa");
    assert_eq!(manifest["author"], "Webber Takken");
    assert_eq!(manifest["micro"]["probability_cutoff"], 0.5);
    assert_eq!(manifest["micro"]["tensor_arena_size"], 32768);
    let report = std::fs::read_to_string(&result.report_markdown).unwrap();
    assert!(report.contains("hey luna: 50.0% of 2 clips"), "{report}");
    assert!(report.contains("/hˈeɪ ɹˈuːnə/"), "{report}");
    assert!(report.contains("| samples | 2 | 100 | 0 |"), "{report}");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&result.report).unwrap()).unwrap();
    assert_eq!(
        report["gpu"],
        "NVIDIA GeForce RTX 4090, 595.99.02, 24564 MiB"
    );
    assert_eq!(
        report["recipe"]["environment"]["freeze"][0],
        "torch==2.14.1+cu126"
    );

    let run = h.root.join("wake-word/runs").join(&started.id);
    assert!(
        !run.join("samples").exists() && !run.join("features").exists(),
        "the bulk is removed"
    );
    let log = std::fs::read_to_string(run.join("logs/samples.log")).unwrap();
    assert!(log.contains("[wakeword] fake samples"), "{log}");
    assert!(h
        .root
        .join("wake-word/trainer/wakeword/__main__.py")
        .is_file());

    assert!(!h.gate.is_busy(), "the worker is free again");
    let jobs = h.observers.local_jobs.lock();
    let job = jobs
        .iter()
        .find(|j| j.job_id == started.id)
        .expect("recorded as a local job");
    assert_eq!(job.kind, TaskKind::WakeWord);
    assert_eq!(job.model, "microwakeword");
    assert_eq!(job.prompt, "hey runa");
    assert_eq!(job.outcome, JobOutcome::Completed);
}

#[test]
fn a_stage_that_runs_out_of_memory_fails_the_training_by_name() {
    let h = harness(Options {
        env: vec![("FAKE_FAIL", "samples:out_of_memory:2")],
        ..Default::default()
    });
    let started = h.service.start(spec()).unwrap();
    let training = wait_until_ended(&h, &started.id);
    assert_eq!(training.state, State::Failed);
    let failure = training.failure.unwrap();
    assert_eq!(failure.reason, "out_of_memory");
    assert_eq!(
        failure.message,
        "ran out of memory during samples: fake out_of_memory"
    );
    assert_eq!(training.stage, Phase::Samples);
    let jobs = h.observers.local_jobs.lock();
    let job = jobs.iter().find(|j| j.job_id == started.id).unwrap();
    assert!(
        matches!(&job.outcome, JobOutcome::Failed { reason } if reason.starts_with("out_of_memory: "))
    );
    assert!(!h.gate.is_busy());
}

#[test]
fn every_named_stage_failure_reaches_the_training() {
    for (fail, reason) in [
        ("prepare:disk_full:4", "disk_full"),
        ("features:bad_input:3", "bad_input"),
        ("train:internal:1", "stage_failed"),
    ] {
        let h = harness(Options {
            env: vec![("FAKE_FAIL", fail)],
            ..Default::default()
        });
        let started = h.service.start(spec()).unwrap();
        let training = wait_until_ended(&h, &started.id);
        assert_eq!(training.failure.unwrap().reason, reason, "{fail}");
    }
}

#[test]
fn a_training_whose_evaluation_is_missing_fails_as_invalid_output() {
    let h = harness(Options {
        env: vec![("FAKE_NO_EVALUATION", "1")],
        ..Default::default()
    });
    let started = h.service.start(spec()).unwrap();
    let training = wait_until_ended(&h, &started.id);
    let failure = training.failure.unwrap();
    assert_eq!(failure.reason, "invalid_output");
    assert!(
        failure.message.contains("evaluation.json"),
        "{}",
        failure.message
    );
}

#[test]
fn a_target_no_cutoff_meets_still_completes_and_says_so() {
    let h = harness(Options {
        env: vec![("FAKE_FA", "50"), ("FAKE_FA_TOP", "50")],
        ..Default::default()
    });
    let started = h.service.start(spec()).unwrap();
    let training = wait_until_ended(&h, &started.id);
    assert_eq!(training.state, State::Completed);
    let result = training.result.unwrap();
    assert!(!result.target_met);
    assert_eq!(result.cutoff, 0.0);
}

#[test]
fn a_cancelled_training_stops_its_stage_and_ends_cancelled() {
    let h = harness(Options {
        env: vec![("FAKE_SLEEP", "train")],
        ..Default::default()
    });
    let started = h.service.start(spec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.service.get(&started.id).unwrap().stage != Phase::Train {
        assert!(Instant::now() < deadline, "never reached train");
        std::thread::sleep(Duration::from_millis(20));
    }
    let asked = Instant::now();
    assert_eq!(h.service.cancel(&started.id).unwrap().id, started.id);
    let training = wait_until_ended(&h, &started.id);
    assert_eq!(training.state, State::Cancelled);
    assert_eq!(training.failure.unwrap().reason, "cancelled");
    assert!(asked.elapsed() < Duration::from_secs(10));
    assert_eq!(
        h.service.cancel(&started.id),
        Err(super::CancelRefusal::NotRunning)
    );
    assert_eq!(h.service.cancel("nope"), Err(super::CancelRefusal::Unknown));
}

#[test]
fn a_failed_download_and_a_failed_provision_are_named() {
    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}/speech.zip")
    };
    let h = harness(Options {
        datasets: vec![dataset("speech", &refused)],
        ..Default::default()
    });
    let started = h.service.start(spec()).unwrap();
    let training = wait_until_ended(&h, &started.id);
    assert_eq!(training.stage, Phase::Download);
    let failure = training.failure.unwrap();
    assert_eq!(failure.reason, "download_failed");
    assert!(
        failure.message.contains("speech (speech.zip)"),
        "{}",
        failure.message
    );

    let h = harness(Options {
        provision_fails: true,
        ..Default::default()
    });
    let started = h.service.start(spec()).unwrap();
    assert_eq!(
        wait_until_ended(&h, &started.id).failure.unwrap().reason,
        "provision_failed"
    );
}

#[test]
fn a_training_is_refused_when_it_cannot_run_here() {
    let h = harness(Options {
        gpu: Err("no NVIDIA GPU: nvidia-smi did not answer".into()),
        ..Default::default()
    });
    assert_eq!(
        h.service.start(spec()),
        Err(Refusal::UnsupportedPlatform(
            "no NVIDIA GPU: nvidia-smi did not answer".into()
        ))
    );

    let h = harness(Options {
        free: Some(1024),
        ..Default::default()
    });
    let Err(Refusal::InsufficientDisk {
        needed_bytes,
        free_bytes,
    }) = h.service.start(spec())
    else {
        panic!("refused for disk")
    };
    assert_eq!(free_bytes, 1024);
    assert!(needed_bytes > 10 * 1024 * 1024 * 1024, "{needed_bytes}");

    let h = harness(Options::default());
    let _held = h.gate.try_reserve().unwrap();
    assert_eq!(h.service.start(spec()), Err(Refusal::Busy));
    assert!(
        h.service.list().is_empty(),
        "a refused training is never listed"
    );
}

#[test]
fn an_ended_training_has_already_freed_the_worker() {
    for _ in 0..20 {
        let h = harness(Options::default());
        let training = h.service.start(spec()).unwrap();
        while h.service.get(&training.id).unwrap().state == State::Running {
            std::hint::spin_loop();
        }
        assert!(
            !h.gate.is_busy(),
            "the worker is free once the training reads ended"
        );
    }
}

#[test]
fn trainings_are_listed_newest_first() {
    let h = harness(Options::default());
    let first = h.service.start(spec()).unwrap();
    wait_until_ended(&h, &first.id);
    let second = h.service.start(spec()).unwrap();
    wait_until_ended(&h, &second.id);
    let ids: Vec<String> = h.service.list().into_iter().map(|t| t.id).collect();
    assert_eq!(ids, [second.id, first.id]);
}

#[test]
fn the_disk_estimate_counts_only_what_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = super::trainer::Paths::new(dir.path());
    let spec = spec();
    let speech = dataset("speech", "https://example.invalid/speech.zip");
    let before = super::training::disk_needed(&spec, &paths, std::slice::from_ref(&speech));
    let file = paths.download(&speech);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"x").unwrap();
    let marker = paths.prepared_marker(&speech).unwrap();
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"{}").unwrap();
    let after = super::training::disk_needed(&spec, &paths, std::slice::from_ref(&speech));
    assert_eq!(before - after, 20, "the download and its prepared set");
}

fn recordings() -> Vec<(&'static str, crate::voice_samples::Take)> {
    use crate::voice_samples::{Kind, Take};
    let mut takes: Vec<_> = (1..=8)
        .map(|n| {
            (
                "s1",
                Take {
                    kind: Kind::Positive,
                    number: n,
                    phrase: None,
                },
            )
        })
        .collect();
    takes.push((
        "s1",
        Take {
            kind: Kind::NearMiss,
            number: 1,
            phrase: Some("hey_luna".into()),
        },
    ));
    takes.push((
        "s1",
        Take {
            kind: Kind::Speech,
            number: 1,
            phrase: None,
        },
    ));
    takes
}

fn spec_with_sets(sets: &str) -> request::TrainingSpec {
    request::parse(format!(r#"{{"phrase":"hey runa","voiceSamples":{sets}}}"#).as_bytes()).unwrap()
}

#[test]
fn a_training_with_a_recording_set_reports_own_voice_recall_apart() {
    let h = harness(Options {
        env: vec![("FAKE_OWN", "0.95,0.4")],
        voice_samples: Some(recordings()),
        ..Default::default()
    });
    let started = h.service.start(spec_with_sets(r#"["s1"]"#)).unwrap();
    assert!(
        h.store.list().unwrap()[0].in_training,
        "the set is held while it trains"
    );
    let training = wait_until_ended(&h, &started.id);
    assert_eq!(training.state, State::Completed, "{:?}", training.failure);
    let own = training.result.unwrap().own_voice_recall.unwrap();
    assert_eq!((own.accepted, own.held_out, own.recall), (1, 2, 0.5));
    let request: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            h.root
                .join("wake-word/runs")
                .join(&started.id)
                .join("request.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(request["ownVoice"]["heldOut"].as_array().unwrap().len(), 2);
    assert_eq!(
        request["ownVoice"]["trainPositive"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let run = h.root.join("wake-word/runs").join(&started.id);
    assert!(!run.join("own_voice").exists() && !run.join("features").exists());
    assert!(
        !h.store.list().unwrap()[0].in_training,
        "released when it ends"
    );
}

#[test]
fn a_failed_training_removes_what_it_derived_from_the_takes() {
    let h = harness(Options {
        env: vec![("FAKE_OWN", "0.95,0.4"), ("FAKE_FAIL", "train:internal:1")],
        voice_samples: Some(recordings()),
        ..Default::default()
    });
    let started = h.service.start(spec_with_sets(r#"["s1"]"#)).unwrap();
    assert_eq!(wait_until_ended(&h, &started.id).state, State::Failed);
    let run = h.root.join("wake-word/runs").join(&started.id);
    assert!(
        run.join("features/positive").exists(),
        "the synthetic features are kept for a look"
    );
    assert!(!run.join("features/own_positive").exists() && !run.join("own_voice").exists());
}

#[test]
fn an_evaluation_without_the_held_out_takes_is_invalid_output() {
    let h = harness(Options {
        voice_samples: Some(recordings()),
        ..Default::default()
    });
    let started = h.service.start(spec_with_sets(r#"["s1"]"#)).unwrap();
    let failure = wait_until_ended(&h, &started.id).failure.unwrap();
    assert_eq!(failure.reason, "invalid_output");
    assert!(
        failure.message.contains("0 held-out own-voice takes of 2"),
        "{}",
        failure.message
    );
}

#[test]
fn recording_sets_that_cannot_train_are_refused_by_name() {
    let h = harness(Options {
        voice_samples: Some(recordings()),
        ..Default::default()
    });
    let Err(Refusal::InvalidVoiceSamples(why)) = h.service.start(spec_with_sets(r#"["s9"]"#))
    else {
        panic!("an unknown set is refused")
    };
    assert!(why.contains("no set named s9"), "{why}");
    h.store.delete(Some("s1")).unwrap();
    use crate::voice_samples::{Kind, Take};
    for n in 1..=3 {
        let audio = crate::voice_samples::take::tests::wav(2.0);
        h.store
            .put(
                "s2",
                &Take {
                    kind: Kind::Positive,
                    number: n,
                    phrase: None,
                },
                &audio,
            )
            .unwrap();
    }
    assert!(matches!(
        h.service.start(spec_with_sets(r#"["s2"]"#)),
        Err(Refusal::InvalidVoiceSamples(why)) if why.contains("3 positive takes, 0 of them")
    ));
    assert!(
        !h.store.list().unwrap()[0].in_training,
        "a refused training holds nothing"
    );
    let without_store = harness(Options::default());
    assert_eq!(
        without_store.service.start(spec_with_sets(r#"["s1"]"#)),
        Err(Refusal::VoiceSamplesUnavailable)
    );
}
