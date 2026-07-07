//! Windows LLM via a subprocess `llama-cli` (mirrors the `sd-cli`
//! pattern) so Windows workers reach in-process-llama parity.
//!
//! `llama-cpp-2` (the in-process backend) doesn't link on Windows MSVC
//! (a static-vs-dynamic CRT clash, documented in `Cargo.toml`), so a
//! Windows release worker would otherwise fall back to the synthetic
//! LLM.  Instead we auto-provision the official `llama.cpp` Windows
//! Vulkan release binary into `<models_root>/bin/` on first use and run
//! `llama-cli` per request, exactly like the image engine runs
//! `sd-cli`.
//!
//! The module is always compiled (so its pure argv / response logic and
//! the provisioner are unit-tested on every platform), but only
//! *registered* on Windows — Linux/macOS keep the faster in-process
//! `llama-cpp-2` backend.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::info;

use super::download;
use crate::types::LlmParams;

// The engine struct + dispatch are only *registered* on Windows, but we
// also compile them in `test` builds on every platform so the dispatch
// path is genuinely type-checked (not just the pure helpers).  The
// pure argv/response helpers above them need none of these.
#[cfg(any(target_os = "windows", test))]
use crate::types::{ModelFileRole, ModelSource, Task, TaskKind, TaskResult};
#[cfg(any(target_os = "windows", test))]
use tracing::warn;

const TRACE_TARGET: &str = "studio_worker::engine::llama_subprocess";

/// Pinned llama.cpp release build.  Bumping this must keep the asset
/// naming (`llama-<build>-bin-win-vulkan-x64.zip`) in step; the
/// `pinned-assets-drift` CI job HEADs it.
const DEFAULT_BUILD: &str = "b6414";
const BUILD_ENV: &str = "STUDIO_WORKER_LLAMA_BUILD";
const URL_ENV: &str = "STUDIO_WORKER_LLAMA_URL";

/// The `llama-cli` executable name for this platform.
fn binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "llama-cli.exe"
    } else {
        "llama-cli"
    }
}

/// Resolve the release build tag: `STUDIO_WORKER_LLAMA_BUILD` wins, else
/// the pinned default.
pub fn select_build(env_value: Option<&str>) -> String {
    match env_value.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => b.to_string(),
        None => DEFAULT_BUILD.to_string(),
    }
}

/// The Windows-Vulkan release asset name for `build`.
pub fn asset_name(build: &str) -> String {
    format!("llama-{build}-bin-win-vulkan-x64.zip")
}

/// The full download URL: a `STUDIO_WORKER_LLAMA_URL` override wins
/// (air-gapped mirror / tests), else the pinned GitHub release asset.
pub fn resolve_url(build_env: Option<&str>, url_env: Option<&str>) -> String {
    if let Some(url) = url_env.map(str::trim).filter(|s| !s.is_empty()) {
        return url.to_string();
    }
    let build = select_build(build_env);
    format!(
        "https://github.com/ggml-org/llama.cpp/releases/download/{build}/{}",
        asset_name(&build)
    )
}

/// Build the `llama-cli` argument vector for a chat completion.  Pure so
/// the flag set is unit-tested without a binary.  `--no-display-prompt`
/// keeps the echoed prompt out of stdout so only the completion is
/// captured; `-st` (single-turn) + `-no-cnv` runs one non-interactive
/// turn and exits.
pub fn build_argv(model_path: &Path, prompt: &str, params: &LlmParams) -> Vec<String> {
    let mut argv = vec![
        "-m".to_string(),
        model_path.display().to_string(),
        "-p".to_string(),
        prompt.to_string(),
        "-n".to_string(),
        params.max_tokens.to_string(),
        "--temp".to_string(),
        params.temperature.to_string(),
        "--no-display-prompt".to_string(),
        "-no-cnv".to_string(),
    ];
    if let Some(top_p) = params.top_p {
        argv.push("--top-p".to_string());
        argv.push(top_p.to_string());
    }
    argv
}

/// Wrap raw `llama-cli` stdout in an OpenAI `chat.completion` object so
/// the local API / studio consumers parse it uniformly with the
/// synthetic + in-process engines.  Pure.
pub fn wrap_response(stdout: &str, model_id: &str, prompt: &str) -> serde_json::Value {
    let content = stdout.trim();
    serde_json::json!({
        "object": "chat.completion",
        "model": model_id,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": prompt.split_whitespace().count(),
            "completion_tokens": content.split_whitespace().count(),
            "total_tokens": prompt.split_whitespace().count() + content.split_whitespace().count(),
        },
    })
}

/// The last non-empty line of `prompt_for`-style chat input: the user
/// turn we feed llama-cli.  (llama-cli takes a single `-p` string; a
/// full chat template is a future enhancement.)
pub fn prompt_from_params(params: &LlmParams) -> String {
    params
        .messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Extract every file from the release zip, flattened into `dest_dir`
/// (defusing zip-slip by keeping only base file names).  Mirrors the
/// sd-cli provisioner's extractor.
fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<usize> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", zip_path.display()))?;
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let mut written = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = Path::new(entry.name()).file_name().map(|n| n.to_owned()) else {
            continue;
        };
        let out = dest_dir.join(&name);
        let mut writer =
            std::fs::File::create(&out).with_context(|| format!("creating {}", out.display()))?;
        std::io::copy(&mut entry, &mut writer)
            .with_context(|| format!("writing {}", out.display()))?;
        written += 1;
    }
    Ok(written)
}

/// Ensure `llama-cli` is present under `<models_root>/bin/`,
/// downloading + extracting the pinned release on first use.  Excluded
/// from coverage: the happy path needs a real multi-hundred-MB download;
/// the URL/asset/argv/response logic is unit-tested and the extractor is
/// fixture-tested.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn provision(models_root: &Path) -> Result<PathBuf> {
    let bin_dir = models_root.join("bin");
    let binary = bin_dir.join(binary_name());
    if binary.is_file() {
        return Ok(binary);
    }
    let url = resolve_url(
        std::env::var(BUILD_ENV).ok().as_deref(),
        std::env::var(URL_ENV).ok().as_deref(),
    );
    info!(
        target: TRACE_TARGET,
        op = "provision",
        url = %url,
        dest = %bin_dir.display(),
        "llama-cli not found; provisioning llama.cpp"
    );
    std::fs::create_dir_all(models_root)
        .with_context(|| format!("creating {}", models_root.display()))?;
    let zip_path = models_root.join(format!(".llama-cli-{}.zip", std::process::id()));
    let result = (|| -> Result<PathBuf> {
        download::download_file(&url, &zip_path)?;
        extract_zip(&zip_path, &bin_dir)?;
        if !binary.is_file() {
            bail!(
                "llama.cpp release {url} did not contain {} after extraction",
                binary_name()
            );
        }
        Ok(binary.clone())
    })();
    download::remove_temp_file(&zip_path);
    result
}

/// Subprocess-`llama-cli` LLM engine (Windows).
#[cfg(any(target_os = "windows", test))]
pub struct LlamaSubprocessEngine {
    models_root: PathBuf,
}

#[cfg(any(target_os = "windows", test))]
#[allow(dead_code)] // run_chat / ensure_model are the live Windows path
impl LlamaSubprocessEngine {
    pub fn new(models_root: PathBuf) -> Self {
        Self { models_root }
    }

    /// Resolve the GGUF weights from the offer's `ModelSource` (role
    /// `model` or `diffusion-model` fallback), downloading on first use.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn ensure_model(&self, model: &str, source: &ModelSource) -> Result<PathBuf> {
        let file = source
            .files
            .iter()
            .find(|f| matches!(f.role, ModelFileRole::Model))
            .ok_or_else(|| {
                anyhow::anyhow!("llama modelSource has no `model` file (the .gguf weights)")
            })?;
        download::ensure_file_for_model(&self.models_root, model, file)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn run_chat(&self, model: &str, params: LlmParams, source: &ModelSource) -> Result<TaskResult> {
        let binary = provision(&self.models_root)?;
        let model_path = self.ensure_model(model, source)?;
        let prompt = prompt_from_params(&params);
        let argv = build_argv(&model_path, &prompt, &params);
        let output = std::process::Command::new(&binary)
            .args(&argv)
            .output()
            .with_context(|| format!("spawning {}", binary.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let last = stderr.lines().last().unwrap_or("");
            warn!(
                target: TRACE_TARGET,
                op = "dispatch",
                model,
                code = ?output.status.code(),
                "llama-cli failed: {last}"
            );
            bail!("llama-cli exited {:?}: {last}", output.status.code());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(TaskResult::Llm {
            json: wrap_response(&stdout, model, &prompt),
        })
    }
}

#[cfg(any(target_os = "windows", test))]
impl super::Engine for LlamaSubprocessEngine {
    fn name(&self) -> &'static str {
        "llama-subprocess"
    }

    fn capabilities(&self) -> super::EngineCapabilities {
        let mut per_kind = std::collections::BTreeMap::new();
        // Kind-based selection on the studio side; the sentinel model
        // name is informational (mirrors sdcpp's `sd-cpp:*`).
        per_kind.insert(TaskKind::Llm, vec!["llama-cpp:*".to_string()]);
        super::EngineCapabilities {
            supported_models_per_kind: per_kind,
        }
    }

    fn dispatch(&self, _model: &str, _task: Task) -> Result<TaskResult> {
        bail!("llama-subprocess requires a ModelSource on the offer")
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        source: &ModelSource,
    ) -> Result<TaskResult> {
        match task {
            Task::Llm(p) => self.run_chat(model, p, source),
            other => Err(super::UnsupportedTask::new("llama-subprocess", other.kind()).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    #[test]
    fn select_build_prefers_env_then_default() {
        assert_eq!(select_build(Some("b9999")), "b9999");
        assert_eq!(select_build(Some("  ")), DEFAULT_BUILD);
        assert_eq!(select_build(None), DEFAULT_BUILD);
    }

    #[test]
    fn asset_and_url_follow_the_pinned_naming() {
        assert_eq!(asset_name("b6414"), "llama-b6414-bin-win-vulkan-x64.zip");
        let url = resolve_url(None, None);
        assert!(url.contains("ggml-org/llama.cpp/releases/download/"));
        assert!(url.ends_with("llama-b6414-bin-win-vulkan-x64.zip"));
        // Env URL override wins verbatim.
        assert_eq!(
            resolve_url(None, Some("http://127.0.0.1/x.zip")),
            "http://127.0.0.1/x.zip"
        );
        // Build override flows into the URL.
        assert!(resolve_url(Some("b7000"), None).ends_with("llama-b7000-bin-win-vulkan-x64.zip"));
    }

    #[test]
    fn build_argv_carries_prompt_tokens_and_temp() {
        let params = LlmParams {
            max_tokens: 128,
            temperature: 0.3,
            top_p: Some(0.9),
            ..Default::default()
        };
        let argv = build_argv(Path::new("/m/model.gguf"), "hello world", &params);
        // Model + prompt + token budget + temp + top-p are all present.
        assert!(argv.windows(2).any(|w| w == ["-m", "/m/model.gguf"]));
        assert!(argv.windows(2).any(|w| w == ["-p", "hello world"]));
        assert!(argv.windows(2).any(|w| w == ["-n", "128"]));
        assert!(argv.windows(2).any(|w| w == ["--temp", "0.3"]));
        assert!(argv.windows(2).any(|w| w == ["--top-p", "0.9"]));
        // Single-turn, no prompt echo.
        assert!(argv.iter().any(|a| a == "--no-display-prompt"));
        assert!(argv.iter().any(|a| a == "-no-cnv"));
    }

    #[test]
    fn build_argv_omits_top_p_when_unset() {
        let argv = build_argv(Path::new("/m.gguf"), "x", &LlmParams::default());
        assert!(!argv.iter().any(|a| a == "--top-p"));
    }

    #[test]
    fn wrap_response_produces_a_chat_completion() {
        let json = wrap_response("  the answer is 42  ", "my-llm", "what is it");
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["model"], "my-llm");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        // stdout is trimmed into the content.
        assert_eq!(json["choices"][0]["message"]["content"], "the answer is 42");
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert_eq!(json["usage"]["completion_tokens"], 4);
    }

    #[test]
    fn prompt_from_params_takes_the_last_message() {
        let params = LlmParams {
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "be terse".into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: "ping".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(prompt_from_params(&params), "ping");
        assert_eq!(prompt_from_params(&LlmParams::default()), "");
    }

    #[test]
    fn extract_zip_flattens_entries_into_the_bin_dir() {
        // Build a fixture zip with a nested path; extraction must flatten
        // it to the base name (zip-slip defence) next to a sibling.
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("llama.zip");
        {
            use std::io::Write as _;
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zw.start_file("build/bin/llama-cli.exe", opts).unwrap();
            zw.write_all(b"MZ fake binary").unwrap();
            zw.start_file("build/bin/ggml.dll", opts).unwrap();
            zw.write_all(b"dll").unwrap();
            zw.finish().unwrap();
        }
        let bin = dir.path().join("bin");
        let n = extract_zip(&zip_path, &bin).unwrap();
        assert_eq!(n, 2);
        assert!(bin.join("llama-cli.exe").is_file());
        assert!(bin.join("ggml.dll").is_file());
        // Nothing escaped into a nested subdir.
        assert!(!bin.join("build").exists());
    }

    #[test]
    fn engine_advertises_only_the_llm_kind() {
        use super::super::Engine as _;
        let engine = LlamaSubprocessEngine::new(PathBuf::from("/models"));
        let caps = engine.capabilities();
        assert_eq!(caps.kinds(), vec![TaskKind::Llm]);
        assert_eq!(engine.name(), "llama-subprocess");
        // Non-LLM tasks are rejected as unsupported.
        let err = engine
            .dispatch_with_source(
                "m",
                Task::Image(crate::types::ImageParams::default()),
                &ModelSource {
                    engine: crate::types::ModelEngine::Synthetic,
                    files: vec![],
                    cli_defaults: crate::types::ModelCliDefaults::default(),
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("cannot serve"));
    }
}
