//! Real model conversion E2E: the spec's `model_convert` task through the convert engine on this
//! machine, provisioning the environment on first use (`docs/engines/convert.md`).
//!
//! Skipped unless `RUN_REAL_ENGINE_TESTS=1`.  Everything lands under `STUDIO_WORKER_MODELS_ROOT`
//! (else `~/models`).  Nothing is uploaded:
//!
//! - `CONVERT_PER_WORD`: drawings per label (default 5; the spec's gate is 100);
//! - `CONVERT_OUT`: a directory the `.onnx` and its report are written to (optional).
//!
//! Run it through PM2: the first run builds the environment (about a minute) and a full gate
//! reads 34 500 drawings.

use std::path::PathBuf;

use studio_worker::engine::convert::ConvertEngine;
use studio_worker::engine::Engine;
use studio_worker::types::{Task, TaskResult};

#[test]
fn the_specs_task_converts_and_scores() {
    if std::env::var_os("RUN_REAL_ENGINE_TESTS").is_none() {
        eprintln!("[real_convert] skipped: set RUN_REAL_ENGINE_TESTS=1");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "studio_worker=info".into()))
        .with_ansi(false)
        .with_test_writer()
        .try_init();
    let models_root = std::env::var("STUDIO_WORKER_MODELS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| studio_worker::config::default_models_root());
    let per_word: u32 = std::env::var("CONVERT_PER_WORD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let task: Task = serde_json::from_value(serde_json::json!({
        "kind": "model_convert",
        "source": "hf:zarqankhn/quickdraw-345-tflite@f978d83cfc41fa3c76435a4c96417e055220c735",
        "file": "quickdraw_model_int8.tflite",
        "labels": "labels.txt",
        "converter": "tf2onnx",
        "opset": 13,
        "gateSet": { "dataset": "quickdraw-raw", "drawingsPerWord": per_word, "seed": 3577 },
        "recipe": "quickdraw-issue-19",
        "polarities": ["inkOnDark", "darkOnLight"],
        "referenceCount": 20,
    }))
    .unwrap();
    let started = std::time::Instant::now();
    let engine = ConvertEngine::new(&models_root);
    let TaskResult::ModelConvert { bytes, ext, report } = engine
        .dispatch("tf2onnx", task)
        .unwrap_or_else(|e| panic!("{e:#}"))
    else {
        panic!("expected a converted model")
    };
    assert_eq!(ext, "onnx");
    assert_eq!(report["labelsMatchCategories"], true);
    assert_eq!(report["input"]["shape"], serde_json::json!([1, 28, 28, 1]));
    assert_eq!(report["output"]["shape"], serde_json::json!([1, 345]));
    assert_eq!(report["gate"]["drawings"], 345 * u64::from(per_word));
    eprintln!(
        "[real_convert] {} bytes in {:.0} s; durations {}; accuracy {}",
        bytes.len(),
        started.elapsed().as_secs_f32(),
        report["durationsMs"],
        report["gate"]["accuracy"]
    );
    if let Ok(out) = std::env::var("CONVERT_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("model.onnx"), &bytes).unwrap();
        std::fs::write(
            out.join("report.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
