//! Real LLM inference via [`llama-cpp-2`].
//!
//! Compiled in via `--features llama`.  Reads every `*.gguf` it can
//! find under `<models_root>/llm/` and exposes the filename stem as a
//! model id.  A job loads its model, runs the generation and frees it
//! again (transient); models kept warm are the model host's resident
//! models (`LoadedLlm`, see `docs/runtime/model-lifecycle.md`).  Both
//! paths share `complete`, returning `chat.completion`-shaped JSON.
use crate::catalog::CatalogModel;
use crate::engine::chat_template::{merge_kwargs, render_chat, TemplateVars};
use crate::engine::llm_core::{
    chat_messages, completion_json, effective_context, finish_for, plan_budget, should_add_bos,
    StopHold,
};
use crate::engine::{Engine, EngineCapabilities};
use crate::host::{ChatModel, LoadedModel};
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
}

// `LlamaBackend::init()` can only run once per process; subsequent calls
// return `BackendAlreadyInitialized`.  We cache a single global handle so
// multiple `LlamaEngine` constructions in the same binary share it.
static GLOBAL_BACKEND: std::sync::OnceLock<Arc<LlamaBackend>> = std::sync::OnceLock::new();

/// Serialises first-time backend init.  `OnceLock::get_or_init` can't
/// host the init because `LlamaBackend::init()` is fallible; without
/// this lock two threads race `init()` and the loser observes
/// `BackendAlreadyInitialized` before the winner has published its
/// handle — a check-then-act gap a bounded spin-wait used to lose on
/// loaded CI runners.
static BACKEND_INIT_LOCK: Mutex<()> = Mutex::new(());

fn global_backend() -> Result<Arc<LlamaBackend>> {
    if let Some(b) = GLOBAL_BACKEND.get() {
        return Ok(b.clone());
    }
    let _guard = BACKEND_INIT_LOCK.lock();
    // Re-check under the lock: another thread may have initialised and
    // published while we waited for it.
    if let Some(b) = GLOBAL_BACKEND.get() {
        return Ok(b.clone());
    }
    let backend = LlamaBackend::init().map_err(|e| match e {
        // With init serialised by the lock, this can only mean some
        // other code path called `LlamaBackend::init()` directly — we
        // have no handle to share, so surface it loudly.
        llama_cpp_2::LlamaCppError::BackendAlreadyInitialized => anyhow!(
            "llama backend was initialised outside global_backend(); no shared handle available"
        ),
        other => anyhow!(other),
    })?;
    let arc = Arc::new(backend);
    let _ = GLOBAL_BACKEND.set(arc.clone());
    Ok(arc)
}

impl LlamaEngine {
    pub fn new(models_root: PathBuf) -> Result<Self> {
        let backend = global_backend().context("initialising llama backend")?;
        Ok(Self {
            backend,
            models_root,
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

    /// Load `path` for one transient job.  Dropped when the job ends, so a
    /// transient job never leaves memory behind that the model host (and
    /// its admission) cannot see; keeping a model warm is what residency is for.
    fn load_transient(&self, model: &str, path: &Path) -> Result<LlamaModel> {
        info!(
            target: TRACE_TARGET,
            op = "load",
            model,
            path = %path.display(),
            "loading model for one job"
        );
        let started = Instant::now();
        let loaded = load_model(&self.backend, model, path).inspect_err(|e| {
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
        info!(
            target: TRACE_TARGET,
            op = "load",
            model,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "model loaded"
        );
        Ok(loaded)
    }
}

/// Append a sampled token's decoded text to the running completion.
///
/// `llama.cpp`'s `token_to_piece` occasionally fails to decode a token
/// to UTF-8 text — a partial multi-byte sequence at a token boundary, or
/// a byte-fallback piece the decoder rejects — which surfaces as an
/// `Err`.  A failed piece is dropped from the completion (preserving the
/// existing behaviour) but counted and warn-logged, so a truncated
/// completion can never pass for a complete one and the "generation
/// complete" breadcrumb reports the real `decode_failures`.  Generic
/// over the error so it's unit-testable without a loaded model.
fn append_piece<E: std::fmt::Display>(
    out: &mut String,
    step: usize,
    piece: std::result::Result<String, E>,
    decode_failures: &mut u32,
) {
    match piece {
        Ok(s) => out.push_str(&s),
        Err(e) => {
            *decode_failures += 1;
            warn!(
                target: TRACE_TARGET,
                op = "generate",
                step,
                error = %e,
                "llama token piece decode failed; dropping it from the completion"
            );
        }
    }
}

/// All layers on the GPU when built with CUDA; CPU otherwise.
fn gpu_layers() -> u32 {
    if cfg!(feature = "cuda") {
        999
    } else {
        0
    }
}

/// Prompt tokens decoded per batch.  Matches llama-server's default
/// `n_batch`; the context's `n_batch` must be at least this.
const PROMPT_BATCH: usize = 2048;

fn load_model(backend: &LlamaBackend, id: &str, path: &Path) -> Result<LlamaModel> {
    let params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers());
    LlamaModel::load_from_file(backend, path, &params)
        .with_context(|| format!("loading model {id} from {}", path.display()))
}

/// Special-token text a template may reference.
fn template_vars(model: &LlamaModel) -> TemplateVars {
    let text = |token| {
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        model
            .token_to_piece(token, &mut decoder, true, None)
            .unwrap_or_default()
    };
    TemplateVars {
        bos_token: text(model.token_bos()),
        eos_token: text(model.token_eos()),
    }
}

/// Render the request with the model's own template and switches.
fn render_request(
    model: &LlamaModel,
    defaults: &ModelCliDefaults,
    params: &LlmParams,
) -> Result<String> {
    let template = model
        .meta_val_str("tokenizer.chat_template")
        .map_err(|_| anyhow!("model has no chat template (tokenizer.chat_template)"))?;
    let kwargs = merge_kwargs(
        defaults.chat_template_kwargs.as_ref(),
        params.chat_template_kwargs.as_ref(),
    );
    Ok(render_chat(
        &template,
        &chat_messages(params),
        &kwargs,
        &template_vars(model),
    )?)
}

fn model_adds_bos(model: &LlamaModel) -> bool {
    model
        .meta_val_str("tokenizer.ggml.add_bos_token")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Run one chat completion on a loaded model.  Shared by the resident
/// lane and the transient (per-job) path, so both behave the same.
pub fn complete(
    model: &LlamaModel,
    backend: &LlamaBackend,
    id: &str,
    defaults: &ModelCliDefaults,
    params: LlmParams,
    cancelled: &dyn Fn() -> bool,
    on_piece: &mut dyn FnMut(&str),
) -> Result<serde_json::Value> {
    let started = Instant::now();
    let prompt = render_request(model, defaults, &params)?;
    let bos = template_vars(model).bos_token;
    let add_bos = if should_add_bos(model_adds_bos(model), &prompt, &bos) {
        AddBos::Always
    } else {
        AddBos::Never
    };
    let tokens = model
        .str_to_token(&prompt, add_bos)
        .map_err(|e| anyhow!("tokenize prompt: {e:?}"))?;
    let n_ctx = effective_context(defaults.context_size, model.n_ctx_train());
    let budget = plan_budget(tokens.len(), params.max_tokens, n_ctx)?;
    debug!(
        target: TRACE_TARGET,
        op = "generate",
        model = id,
        prompt_tokens = tokens.len(),
        budget,
        n_ctx,
        "starting generation"
    );
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(n_ctx))
        .with_n_batch(PROMPT_BATCH as u32);
    let mut ctx = model
        .new_context(backend, ctx_params)
        .context("creating llama context")?;

    let mut batch = LlamaBatch::new(PROMPT_BATCH, 1);
    let mut pos: i32 = 0;
    let last = tokens.len() - 1;
    for chunk in tokens.chunks(PROMPT_BATCH) {
        if cancelled() {
            bail!("cancelled: the model is unloading or the client left");
        }
        batch.clear();
        for token in chunk {
            batch
                .add(*token, pos, &[0], pos as usize == last)
                .map_err(|e| anyhow!("batch add: {e:?}"))?;
            pos += 1;
        }
        ctx.decode(&mut batch).context("decoding prompt")?;
    }

    let mut sampler = if params.temperature <= 0.0 {
        LlamaSampler::greedy()
    } else {
        let mut chain = Vec::new();
        if let Some(p) = params.top_p {
            chain.push(LlamaSampler::top_p(p, 1));
        }
        chain.push(LlamaSampler::temp(params.temperature));
        chain.push(LlamaSampler::dist(1234));
        LlamaSampler::chain_simple(chain)
    };
    let mut hold = StopHold::new(&params.stop.clone().unwrap_or_default());
    // One decoder for the whole completion: a character split across two
    // tokens decodes once both halves arrive.
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut piece = String::new();
    let (mut generated, mut hit_end, mut decode_failures) = (0u32, false, 0u32);
    while generated < budget {
        if cancelled() {
            bail!("cancelled: the model is unloading or the client left");
        }
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            hit_end = true;
            break;
        }
        piece.clear();
        append_piece(
            &mut piece,
            generated as usize,
            model.token_to_piece(token, &mut decoder, false, None),
            &mut decode_failures,
        );
        generated += 1;
        let safe = hold.push(&piece);
        if !safe.is_empty() {
            on_piece(&safe);
        }
        if hold.stopped().is_some() {
            hit_end = true;
            break;
        }
        batch.clear();
        batch
            .add(token, pos, &[0], true)
            .map_err(|e| anyhow!("batch add (token): {e:?}"))?;
        pos += 1;
        ctx.decode(&mut batch).context("decoding token")?;
    }
    let rest = hold.finish();
    if !rest.is_empty() {
        on_piece(&rest);
    }
    let out = hold.text().to_string();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let finish = finish_for(generated, budget, hit_end);
    info!(
        target: TRACE_TARGET,
        op = "generate",
        model = id,
        prompt_tokens = tokens.len(),
        completion_tokens = generated,
        finish = finish.as_str(),
        decode_failures,
        elapsed_ms,
        "generation complete"
    );
    Ok(completion_json(
        id,
        &out,
        tokens.len(),
        generated,
        finish,
        elapsed_ms,
    ))
}

/// A GGUF held in memory by the model host (a resident model).
pub struct LoadedLlm {
    id: String,
    model: LlamaModel,
    backend: Arc<LlamaBackend>,
    defaults: ModelCliDefaults,
}

impl LoadedModel for LoadedLlm {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_chat(&self) -> Option<&dyn ChatModel> {
        Some(self)
    }
}

impl ChatModel for LoadedLlm {
    fn chat(
        &self,
        params: LlmParams,
        cancelled: &dyn Fn() -> bool,
        on_piece: &mut dyn FnMut(&str),
    ) -> Result<serde_json::Value> {
        complete(
            &self.model,
            &self.backend,
            &self.id,
            &self.defaults,
            params,
            cancelled,
            on_piece,
        )
    }

    fn tokenize(&self, text: &str, add_special: bool) -> Result<Vec<i32>> {
        let add_bos = if add_special {
            AddBos::Always
        } else {
            AddBos::Never
        };
        Ok(self
            .model
            .str_to_token(text, add_bos)
            .map_err(|e| anyhow!("tokenize: {e:?}"))?
            .into_iter()
            .map(|t| t.0)
            .collect())
    }
}

/// Download (if needed) and load `model` for the host to keep resident.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn load_resident(models_root: &Path, model: &CatalogModel) -> Result<LoadedLlm> {
    let backend = global_backend().context("initialising llama backend")?;
    let files = ensure_llm_files(models_root, &model.source)?;
    let path = pick_gguf(&files).ok_or_else(|| {
        anyhow!(
            "llama modelSource for `{}` contained no .gguf file",
            model.id
        )
    })?;
    let started = Instant::now();
    let loaded = load_model(&backend, &model.id, &path)?;
    info!(
        target: TRACE_TARGET,
        op = "load",
        model = %model.id,
        gpu_layers = gpu_layers(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "resident model loaded"
    );
    Ok(LoadedLlm {
        id: model.id.clone(),
        model: loaded,
        backend,
        defaults: model.source.cli_defaults.clone(),
    })
}

/// Download every file of `source` into `<root>/llm/`.
#[cfg_attr(coverage_nightly, coverage(off))]
fn ensure_llm_files(
    models_root: &Path,
    source: &ModelSource,
) -> Result<Vec<(ModelFileRole, PathBuf)>> {
    let dir = models_root.join("llm");
    source
        .files
        .iter()
        .map(|file| {
            Ok((
                file.role,
                crate::engine::download::ensure_file_reusing(&dir, models_root, file)?,
            ))
        })
        .collect()
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
            Err(crate::engine::UnsupportedTask::new("llama", other.kind()).into())
        }
    }
}

impl LlamaEngine {
    /// Load `path` (caching it) and run one chat completion with the
    /// model's catalogue `defaults`.  Shared by `dispatch` (local model)
    /// and `dispatch_with_source` (downloaded model).
    fn run_llm(
        &self,
        model: &str,
        path: &Path,
        defaults: &ModelCliDefaults,
        llm: LlmParams,
    ) -> Result<TaskResult> {
        let loaded = self.load_transient(model, path)?;
        let json = complete(
            &loaded,
            &self.backend,
            model,
            defaults,
            llm,
            &|| false,
            &mut |_| {},
        )
        .inspect_err(|e| {
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                kind = "llm",
                model,
                error = %e,
                "generation failed"
            );
        })?;
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
        self.run_llm(model, &path, &ModelCliDefaults::default(), llm)
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
            let resolved = ensure_llm_files(&self.models_root, source)?;
            pick_gguf(&resolved)
                .ok_or_else(|| anyhow!("llama modelSource for `{model}` contained no .gguf file"))?
        };
        self.run_llm(model, &path, &source.cli_defaults, llm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // append_piece — accumulates a sampled token's decoded text into the
    // running completion.  llama.cpp's `token_to_piece` occasionally
    // fails to decode a token to UTF-8 text; a failed piece is dropped
    // from the completion (preserving the existing behaviour) but
    // counted and warn-logged, so a truncated completion can never pass
    // for a complete one and the "generation complete" breadcrumb
    // reports the real decode_failures.
    // -----------------------------------------------------------------

    #[test]
    fn append_piece_concatenates_ok_pieces() {
        let mut out = String::new();
        let mut failures = 0u32;
        append_piece(&mut out, 0, Ok::<_, &str>("hel".to_string()), &mut failures);
        append_piece(&mut out, 1, Ok::<_, &str>("lo".to_string()), &mut failures);
        assert_eq!(out, "hello");
        assert_eq!(failures, 0);
    }

    #[test]
    fn append_piece_drops_and_counts_failed_pieces() {
        // A token whose text can't be decoded is dropped from the
        // completion but counted, never silently lost — so a truncated
        // completion can't pass for a complete one.
        let mut out = String::new();
        let mut failures = 0u32;
        append_piece(
            &mut out,
            0,
            Ok::<_, &str>("kept".to_string()),
            &mut failures,
        );
        append_piece(&mut out, 1, Err("invalid utf-8"), &mut failures);
        append_piece(
            &mut out,
            2,
            Ok::<_, &str>(" tail".to_string()),
            &mut failures,
        );
        assert_eq!(out, "kept tail");
        assert_eq!(failures, 1);
    }

    #[test]
    fn append_piece_warns_on_each_decode_failure() {
        let logs = crate::test_support::capture(|| {
            let mut out = String::new();
            let mut failures = 0u32;
            append_piece(&mut out, 7, Err("decode boom"), &mut failures);
        });
        assert!(
            logs.contains("studio_worker::engine::llama"),
            "expected llama target, got: {logs}"
        );
        assert!(logs.contains("WARN"), "expected WARN level, got: {logs}");
        assert!(
            logs.contains("decode boom"),
            "expected the underlying error, got: {logs}"
        );
        assert!(
            logs.contains("step=7"),
            "expected the step index, got: {logs}"
        );
    }

    /// Regression: `global_backend()` once used a bounded spin-wait for
    /// the `BackendAlreadyInitialized` race and flaked on loaded CI
    /// runners.  Hammer it from many threads — every call must succeed.
    #[test]
    fn global_backend_never_fails_under_contention() {
        let handles: Vec<_> = (0..32)
            .map(|_| std::thread::spawn(|| global_backend().map(|_| ())))
            .collect();
        for h in handles {
            h.join()
                .expect("thread panicked")
                .expect("global_backend must never fail under contention");
        }
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
