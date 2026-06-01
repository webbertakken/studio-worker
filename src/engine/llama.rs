//! Real LLM inference via [`llama-cpp-2`].
//!
//! Compiled in via `--features llama`.  Reads every `*.gguf` it can
//! find under `<models_root>/llm/` and exposes the filename stem as a
//! model id.  When a job's `model` matches, the engine loads it on
//! demand (with an LRU of size 1 — keep the most recently used model
//! resident in VRAM/RAM), runs the generation, and returns
//! `chat.completion`-shaped JSON.
use crate::engine::{Engine, EngineCapabilities};
use crate::types::*;
use anyhow::{anyhow, bail, Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Tracing target for the llama engine.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::engine::llama=debug`.
const TRACE_TARGET: &str = "studio_worker::engine::llama";

pub struct LlamaEngine {
    backend: Arc<LlamaBackend>,
    models_root: PathBuf,
    cached: Mutex<Option<CachedModel>>,
}

// `LlamaBackend::init()` can only run once per process; subsequent calls
// return `BackendAlreadyInitialized`.  We cache a single global handle so
// multiple `LlamaEngine` constructions in the same binary share it.
static GLOBAL_BACKEND: std::sync::OnceLock<Arc<LlamaBackend>> = std::sync::OnceLock::new();

fn global_backend() -> Result<Arc<LlamaBackend>> {
    if let Some(b) = GLOBAL_BACKEND.get() {
        return Ok(b.clone());
    }
    match LlamaBackend::init() {
        Ok(backend) => {
            let arc = Arc::new(backend);
            let _ = GLOBAL_BACKEND.set(arc.clone());
            Ok(arc)
        }
        Err(llama_cpp_2::LlamaCppError::BackendAlreadyInitialized) => {
            // Race: someone else got the global init in between.  Wait for
            // them to publish their handle.  In practice this is
            // extremely brief (microseconds).
            for _ in 0..1_000 {
                if let Some(b) = GLOBAL_BACKEND.get() {
                    return Ok(b.clone());
                }
                std::thread::yield_now();
            }
            Err(anyhow!(
                "llama backend already initialised but the global handle never published"
            ))
        }
        Err(e) => Err(e.into()),
    }
}

struct CachedModel {
    id: String,
    model: Arc<LlamaModel>,
}

impl LlamaEngine {
    pub fn new(models_root: PathBuf) -> Result<Self> {
        let backend = global_backend().context("initialising llama backend")?;
        Ok(Self {
            backend,
            models_root,
            cached: Mutex::new(None),
        })
    }

    fn llm_dir(&self) -> PathBuf {
        self.models_root.join("llm")
    }

    fn list_models(&self) -> Vec<(String, PathBuf)> {
        let dir = self.llm_dir();
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in read.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("gguf") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push((stem.to_string(), p));
                }
            }
        }
        out
    }

    fn resolve_path(&self, model: &str) -> Option<PathBuf> {
        self.list_models()
            .into_iter()
            .find(|(stem, _)| stem == model)
            .map(|(_, p)| p)
    }

    fn load_or_get(&self, model: &str, path: &Path) -> Result<Arc<LlamaModel>> {
        let mut guard = self.cached.lock();
        if let Some(c) = &*guard {
            if c.id == model {
                debug!(
                    target: TRACE_TARGET,
                    op = "load",
                    model,
                    cache = "hit",
                    "reusing cached model"
                );
                return Ok(c.model.clone());
            }
        }
        info!(
            target: TRACE_TARGET,
            op = "load",
            model,
            path = %path.display(),
            "loading model"
        );
        let started = Instant::now();
        let params = LlamaModelParams::default();
        let loaded = LlamaModel::load_from_file(&self.backend, path, &params)
            .with_context(|| format!("loading model {} from {}", model, path.display()))
            .inspect_err(|e| {
                warn!(
                    target: TRACE_TARGET,
                    op = "load",
                    model,
                    path = %path.display(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %e,
                    "failed to load model"
                );
            })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let arc = Arc::new(loaded);
        *guard = Some(CachedModel {
            id: model.to_string(),
            model: arc.clone(),
        });
        info!(
            target: TRACE_TARGET,
            op = "load",
            model,
            elapsed_ms,
            "model loaded"
        );
        Ok(arc)
    }
}

fn render_prompt(messages: &[ChatMessage]) -> String {
    // Minimal chat template: <role>: <content>\n…\nassistant:
    let mut out = String::new();
    for m in messages {
        out.push_str(&format!("<|{}|>\n{}\n", m.role, m.content));
    }
    out.push_str("<|assistant|>\n");
    out
}

/// Whether a prompt of `prompt_tokens` plus a `max_tokens` generation
/// budget overflows the context window `n_ctx` (the KV-cache size).
/// Pure so the over-budget guard is unit-tested without a loaded model.
/// Saturating arithmetic keeps a pathological `max_tokens` from wrapping.
fn exceeds_context_window(prompt_tokens: usize, max_tokens: u32, n_ctx: u32) -> bool {
    prompt_tokens.saturating_add(max_tokens as usize) > n_ctx as usize
}

fn run_generation(
    model: &LlamaModel,
    backend: &LlamaBackend,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    let ctx_size = NonZeroU32::new(2048).expect("non-zero");
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(Some(ctx_size))
        .with_n_batch(512);
    let mut ctx = model
        .new_context(backend, ctx_params)
        .context("creating llama context")?;

    let tokens = model
        .str_to_token(prompt, AddBos::Always)
        .map_err(|e| anyhow!("tokenize prompt: {e:?}"))?;
    if tokens.is_empty() {
        bail!("prompt tokenised to zero tokens");
    }

    // A prompt plus its generation budget larger than the context
    // window overflows the KV cache: later decode steps run past what
    // this context was sized for and the output silently truncates.
    // We deliberately don't trim the prompt here (that would mangle the
    // operator's input) — instead we surface the condition so "why was
    // my long chat cut off" is answerable from the logs.  Raise n_ctx
    // for longer chats.
    if exceeds_context_window(tokens.len(), max_tokens, ctx_size.get()) {
        warn!(
            target: TRACE_TARGET,
            op = "generate",
            prompt_tokens = tokens.len(),
            max_tokens,
            n_ctx = ctx_size.get(),
            "prompt + max_tokens exceeds the context window; output may be \
             truncated — raise n_ctx for longer chats"
        );
    }

    let mut batch = LlamaBatch::new(2048, 1);
    let last_index = tokens.len() as i32 - 1;
    for (i, token) in (0_i32..).zip(tokens.iter().copied()) {
        let is_last = i == last_index;
        batch
            .add(token, i, &[0], is_last)
            .map_err(|e| anyhow!("batch add: {e:?}"))?;
    }
    ctx.decode(&mut batch).context("decoding prompt")?;

    let mut sampler = LlamaSampler::chain_simple(if temperature <= 0.0 {
        vec![LlamaSampler::greedy()]
    } else {
        vec![
            LlamaSampler::temp(temperature),
            LlamaSampler::dist(/* seed */ 1234),
        ]
    });

    let mut out = String::new();
    let mut cursor = batch.n_tokens();
    #[allow(clippy::explicit_counter_loop)]
    for _step in 0..max_tokens {
        let new_token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(new_token);
        if model.is_eog_token(new_token) {
            break;
        }
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        if let Ok(piece) = model.token_to_piece(new_token, &mut decoder, false, None) {
            out.push_str(&piece);
        }
        batch.clear();
        batch
            .add(new_token, cursor, &[0], true)
            .map_err(|e| anyhow!("batch add (token): {e:?}"))?;
        cursor += 1;
        ctx.decode(&mut batch).context("decoding token")?;
    }
    Ok(out)
}

impl Engine for LlamaEngine {
    fn name(&self) -> &'static str {
        "llama"
    }

    fn capabilities(&self) -> EngineCapabilities {
        let models: Vec<String> = self.list_models().into_iter().map(|(s, _)| s).collect();
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Llm, models);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let kind = task.kind();
        let llm = match task {
            Task::Llm(p) => p,
            other => {
                warn!(
                    target: TRACE_TARGET,
                    op = "dispatch",
                    kind = kind.as_str(),
                    model,
                    "unsupported task kind"
                );
                bail!("llama engine cannot serve {} tasks", other.kind().as_str());
            }
        };
        let path = self.resolve_path(model).ok_or_else(|| {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                model,
                models_root = %self.llm_dir().display(),
                "model not found"
            );
            anyhow!("model `{model}` not found in {}", self.llm_dir().display())
        })?;
        let loaded = self.load_or_get(model, &path)?;
        let prompt = render_prompt(&llm.messages);
        debug!(
            target: TRACE_TARGET,
            op = "dispatch",
            kind = kind.as_str(),
            model,
            max_tokens = llm.max_tokens,
            temperature = llm.temperature,
            messages = llm.messages.len(),
            "starting generation"
        );
        let started = Instant::now();
        let content = run_generation(
            &loaded,
            &self.backend,
            &prompt,
            llm.max_tokens.max(1),
            llm.temperature.max(0.0),
        )
        .inspect_err(|e| {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                kind = kind.as_str(),
                model,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "generation failed"
            );
        })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        info!(
            target: TRACE_TARGET,
            op = "dispatch",
            kind = kind.as_str(),
            model,
            elapsed_ms,
            completion_chars = content.len(),
            "generation complete"
        );

        let prompt_tokens = prompt.split_whitespace().count();
        let completion_tokens = content.split_whitespace().count();
        let json = serde_json::json!({
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content.trim(),
                },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
            "elapsed_ms": elapsed_ms,
        });
        Ok(TaskResult::Llm { json })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exceeds_context_window_false_when_within_window() {
        // 100 prompt + 50 budget = 150 <= 2048.
        assert!(!exceeds_context_window(100, 50, 2048));
    }

    #[test]
    fn exceeds_context_window_true_when_over_window() {
        // 2000 prompt + 100 budget = 2100 > 2048.
        assert!(exceeds_context_window(2000, 100, 2048));
    }

    #[test]
    fn exceeds_context_window_false_at_exact_window() {
        // Filling the window exactly is not yet an overflow.
        assert!(!exceeds_context_window(1998, 50, 2048));
    }

    #[test]
    fn exceeds_context_window_saturates_on_huge_budget() {
        // A pathological max_tokens must not wrap to a small sum.
        assert!(exceeds_context_window(1, u32::MAX, 2048));
    }

    #[test]
    fn render_prompt_concatenates_messages_with_assistant_marker() {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "be helpful".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            },
        ];
        let rendered = render_prompt(&messages);
        assert!(rendered.contains("<|system|>"));
        assert!(rendered.contains("be helpful"));
        assert!(rendered.contains("<|user|>"));
        assert!(rendered.contains("hi"));
        assert!(rendered.ends_with("<|assistant|>\n"));
    }

    #[test]
    fn capabilities_lists_no_models_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = LlamaEngine::new(tmp.path().to_path_buf()).expect("init backend");
        let caps = engine.capabilities();
        assert!(caps.supported_models_per_kind[&TaskKind::Llm].is_empty());
    }

    #[test]
    fn capabilities_picks_up_gguf_files() {
        let tmp = tempfile::tempdir().unwrap();
        let llm_dir = tmp.path().join("llm");
        std::fs::create_dir_all(&llm_dir).unwrap();
        // Just touch a file; we never load it.
        std::fs::write(llm_dir.join("smollm-135m-q8.gguf"), b"not-real").unwrap();
        std::fs::write(llm_dir.join("ignored.txt"), b"x").unwrap();
        let engine = LlamaEngine::new(tmp.path().to_path_buf()).expect("init backend");
        let caps = engine.capabilities();
        let models = &caps.supported_models_per_kind[&TaskKind::Llm];
        assert_eq!(models, &vec!["smollm-135m-q8".to_string()]);
    }

    #[test]
    fn dispatch_returns_error_when_model_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = LlamaEngine::new(tmp.path().to_path_buf()).expect("init backend");
        let task = Task::Llm(LlmParams {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: 1,
            temperature: 0.0,
            ..Default::default()
        });
        let err = engine.dispatch("no-such-model", task).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn dispatch_rejects_non_llm_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = LlamaEngine::new(tmp.path().to_path_buf()).expect("init backend");
        let task = Task::Image(ImageParams {
            prompt: "x".into(),
            width: 64,
            height: 64,
            steps: 1,
            seed: None,
            ext: "webp".into(),
            ..Default::default()
        });
        let err = engine.dispatch("anything", task).unwrap_err();
        assert!(err.to_string().contains("cannot serve image"));
    }
}
