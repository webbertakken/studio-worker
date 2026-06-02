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

/// Sentinel the studio's claim filter recognises as "any llama-cpp
/// model is fine" — mirrors the `sd-cpp:*` wildcard the image engine
/// advertises.  The model files arrive on the offer's `ModelSource`, so
/// the worker doesn't have to enumerate model ids up front; this lets a
/// freshly-installed worker claim llama jobs and download the GGUF on
/// demand.
const LLAMA_MODEL_WILDCARD: &str = "llama-cpp:*";

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}

/// Pick the GGUF to load from a set of downloaded model files: prefer
/// the explicit `Model`-role file, else the first `.gguf`.  Pure so the
/// selection contract is unit-tested without a download.
fn pick_gguf(files: &[(ModelFileRole, PathBuf)]) -> Option<PathBuf> {
    files
        .iter()
        .find(|(role, path)| matches!(role, ModelFileRole::Model) && is_gguf(path))
        .or_else(|| files.iter().find(|(_, path)| is_gguf(path)))
        .map(|(_, path)| path.clone())
}

/// Extract the LLM params from a task, rejecting any other kind with the
/// `cannot serve <kind>` shape the studio's claim loop recognises.
fn as_llm(task: Task, model: &str) -> Result<LlmParams> {
    match task {
        Task::Llm(p) => Ok(p),
        other => {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                kind = other.kind().as_str(),
                model,
                "unsupported task kind"
            );
            bail!("llama engine cannot serve {} tasks", other.kind().as_str())
        }
    }
}

impl LlamaEngine {
    /// Download every file the studio listed on the offer into
    /// `<root>/llm/`, returning the resolved (role, path) pairs.  Cached
    /// files are reused; a truncated download is rejected by the shared
    /// downloader rather than cached as a corrupt model.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_model_files(&self, source: &ModelSource) -> Result<Vec<(ModelFileRole, PathBuf)>> {
        let dir = self.llm_dir();
        let mut out = Vec::with_capacity(source.files.len());
        for file in &source.files {
            let local = crate::engine::download::ensure_file(&dir, &file.filename, &file.url)?;
            out.push((file.role, local));
        }
        Ok(out)
    }

    /// Load `path` (caching it) and run one chat completion, returning
    /// `chat.completion`-shaped JSON.  Shared by the plain `dispatch`
    /// (local model) and `dispatch_with_source` (downloaded model) paths.
    fn run_llm(&self, model: &str, path: &Path, llm: LlmParams) -> Result<TaskResult> {
        let loaded = self.load_or_get(model, path)?;
        let prompt = render_prompt(&llm.messages);
        debug!(
            target: TRACE_TARGET,
            op = "dispatch",
            kind = "llm",
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
                kind = "llm",
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
            kind = "llm",
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

impl Engine for LlamaEngine {
    fn name(&self) -> &'static str {
        "llama"
    }

    fn capabilities(&self) -> EngineCapabilities {
        // Advertise both any locally-present GGUF stems (pre-placed
        // models) and the wildcard sentinel so the studio can hand this
        // worker any llama-cpp model from its registry; the files come
        // down on the offer's `ModelSource`.
        let mut models: Vec<String> = self.list_models().into_iter().map(|(s, _)| s).collect();
        models.push(LLAMA_MODEL_WILDCARD.to_string());
        let mut map: BTreeMap<TaskKind, Vec<String>> = BTreeMap::new();
        map.insert(TaskKind::Llm, models);
        EngineCapabilities {
            supported_models_per_kind: map,
        }
    }

    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult> {
        let llm = as_llm(task, model)?;
        let path = self.resolve_path(model).ok_or_else(|| {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                model,
                models_root = %self.llm_dir().display(),
                "model not found"
            );
            anyhow!(
                "model `{model}` not found in {} and the offer carried no \
                 modelSource to download it from",
                self.llm_dir().display()
            )
        })?;
        self.run_llm(model, &path, llm)
    }

    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        let llm = as_llm(task, model)?;
        // Prefer the studio-provided files (download on demand); fall
        // back to a locally-present GGUF when the offer lists none.
        let path = if source.files.is_empty() {
            self.resolve_path(model).ok_or_else(|| {
                anyhow!(
                    "model `{model}` not found in {} and the offer's modelSource \
                     listed no files to download",
                    self.llm_dir().display()
                )
            })?
        } else {
            let resolved = self.ensure_model_files(source)?;
            pick_gguf(&resolved).ok_or_else(|| {
                anyhow!("llama modelSource for `{model}` contained no .gguf file")
            })?
        };
        self.run_llm(model, &path, llm)
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
    fn capabilities_advertise_wildcard_even_with_no_local_models() {
        // A fresh worker has no local GGUFs but must still advertise the
        // wildcard so the studio can hand it a llama job (files arrive on
        // the offer's modelSource).
        let tmp = tempfile::tempdir().unwrap();
        let engine = LlamaEngine::new(tmp.path().to_path_buf()).expect("init backend");
        let caps = engine.capabilities();
        let models = &caps.supported_models_per_kind[&TaskKind::Llm];
        assert_eq!(models, &vec![LLAMA_MODEL_WILDCARD.to_string()]);
        assert!(caps.supports(TaskKind::Llm, LLAMA_MODEL_WILDCARD));
    }

    #[test]
    fn capabilities_picks_up_gguf_files_and_keeps_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let llm_dir = tmp.path().join("llm");
        std::fs::create_dir_all(&llm_dir).unwrap();
        // Just touch a file; we never load it.
        std::fs::write(llm_dir.join("smollm-135m-q8.gguf"), b"not-real").unwrap();
        std::fs::write(llm_dir.join("ignored.txt"), b"x").unwrap();
        let engine = LlamaEngine::new(tmp.path().to_path_buf()).expect("init backend");
        let caps = engine.capabilities();
        let models = &caps.supported_models_per_kind[&TaskKind::Llm];
        assert_eq!(
            models,
            &vec![
                "smollm-135m-q8".to_string(),
                LLAMA_MODEL_WILDCARD.to_string()
            ]
        );
    }

    #[test]
    fn is_gguf_matches_extension_case_insensitively() {
        assert!(is_gguf(Path::new("/m/model.gguf")));
        assert!(is_gguf(Path::new("/m/model.GGUF")));
        assert!(!is_gguf(Path::new("/m/model.safetensors")));
        assert!(!is_gguf(Path::new("/m/model")));
    }

    #[test]
    fn pick_gguf_prefers_model_role_then_first_gguf() {
        // Model-role gguf wins even when listed after another gguf.
        let files = vec![
            (ModelFileRole::TextEncoder, PathBuf::from("/m/clip.gguf")),
            (ModelFileRole::Model, PathBuf::from("/m/weights.gguf")),
        ];
        assert_eq!(pick_gguf(&files), Some(PathBuf::from("/m/weights.gguf")));
        // No Model role: fall back to the first gguf.
        let files = vec![
            (ModelFileRole::Vae, PathBuf::from("/m/vae.safetensors")),
            (ModelFileRole::TextEncoder, PathBuf::from("/m/first.gguf")),
            (ModelFileRole::Lora, PathBuf::from("/m/second.gguf")),
        ];
        assert_eq!(pick_gguf(&files), Some(PathBuf::from("/m/first.gguf")));
        // Nothing gguf at all.
        let files = vec![(ModelFileRole::Vae, PathBuf::from("/m/vae.safetensors"))];
        assert_eq!(pick_gguf(&files), None);
    }

    #[test]
    fn as_llm_extracts_llm_params_and_rejects_other_kinds() {
        let llm = Task::Llm(LlmParams {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: 8,
            temperature: 0.1,
            ..Default::default()
        });
        assert!(as_llm(llm, "m").is_ok());
        let image = Task::Image(ImageParams {
            prompt: "x".into(),
            ..Default::default()
        });
        let err = as_llm(image, "m").unwrap_err().to_string();
        assert!(err.contains("cannot serve image"), "got: {err}");
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
