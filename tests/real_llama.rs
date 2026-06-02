//! Real LLM E2E: download a tiny GGUF on first run, then exercise
//! the [`LlamaEngine`] all the way through to `chat.completion` JSON.
//!
//! Skipped by default — set `RUN_REAL_ENGINE_TESTS=1` to enable.  When
//! the model is already cached at `<models>/llm/<id>.gguf`, the
//! download is skipped and the test runs offline.
#![cfg(all(feature = "llama", not(target_os = "windows")))]

use std::path::{Path, PathBuf};
use std::time::Duration;
use studio_worker::engine::llama::LlamaEngine;
use studio_worker::engine::Engine;
use studio_worker::types::*;

/// SmolLM-135M-Instruct Q8 — ~145 MB.  One of the smallest open-source
/// chat models that still produces coherent short completions.
const MODEL_ID: &str = "smollm-135m-instruct-q8";
const MODEL_URL: &str =
    "https://huggingface.co/HuggingFaceTB/smollm-135M-instruct-v0.2-Q8_0-GGUF/resolve/main/smollm-135m-instruct-add-basics-q8_0.gguf";

fn cache_root() -> PathBuf {
    if let Ok(p) = std::env::var("STUDIO_WORKER_MODELS_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(dirs) = directories::ProjectDirs::from("gg", "minis", "minis-studio-worker") {
        return dirs.cache_dir().to_path_buf();
    }
    std::env::temp_dir().join("studio-worker-models")
}

fn ensure_model(id: &str, url: &str) -> Option<PathBuf> {
    let dir = cache_root().join("llm");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{id}.gguf"));
    if path.exists() {
        return Some(path);
    }
    std::env::var_os("RUN_REAL_ENGINE_TESTS")?;
    eprintln!("[real_llama] downloading {url} -> {}", path.display());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .ok()?;
    let mut response = client.get(url).send().ok()?.error_for_status().ok()?;
    let tmp = path.with_extension("gguf.partial");
    let mut file = std::fs::File::create(&tmp).ok()?;
    std::io::copy(&mut response, &mut file).ok()?;
    drop(file);
    std::fs::rename(&tmp, &path).ok()?;
    Some(path)
}

fn engine_with_cache(_path: &Path) -> LlamaEngine {
    LlamaEngine::new(cache_root()).expect("llama backend")
}

#[test]
fn end_to_end_inference_with_smollm() {
    let Some(path) = ensure_model(MODEL_ID, MODEL_URL) else {
        eprintln!(
            "[real_llama] skipped: set RUN_REAL_ENGINE_TESTS=1 to download \\\n  {} (~145 MB) and exercise real inference.",
            MODEL_URL
        );
        return;
    };
    assert!(
        path.exists(),
        "model should be cached at {}",
        path.display()
    );

    let engine = engine_with_cache(&path);
    let caps = engine.capabilities();
    let llm_models = &caps.supported_models_per_kind[&TaskKind::Llm];
    assert!(
        llm_models.iter().any(|m| m == MODEL_ID),
        "{} should be advertised by the engine, got {:?}",
        MODEL_ID,
        llm_models
    );

    let task = Task::Llm(LlmParams {
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "Say hello in one short sentence.".into(),
        }],
        max_tokens: 24,
        temperature: 0.0,
        ..Default::default()
    });
    let started = std::time::Instant::now();
    let result = engine
        .dispatch(MODEL_ID, task)
        .expect("inference should succeed");
    let elapsed = started.elapsed();
    eprintln!("[real_llama] inference took {elapsed:?}");

    let json = match result {
        TaskResult::Llm { json } => json,
        other => panic!("expected llm result, got {:?}", other.kind()),
    };
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["model"], MODEL_ID);
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .expect("content should be a string");
    assert!(
        !content.trim().is_empty(),
        "real LLM should produce a non-empty completion, got {content:?}"
    );
    let total_tokens = json["usage"]["total_tokens"].as_u64().unwrap_or(0);
    assert!(total_tokens > 0, "usage.total_tokens should be > 0");
}
