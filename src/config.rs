//! Persistent config in `~/.config/minis-studio-worker/config.toml` (Linux/macOS)
//! or `%APPDATA%\minis-studio-worker\config.toml` (Windows).
//!
//! Every load/save emits a structured tracing breadcrumb so operators
//! can tell from `journalctl` which file the worker actually consulted
//! (and whether the file existed or was freshly bootstrapped with
//! defaults).  The events deliberately omit the two secret fields
//! — `bootstrap_token` and `auth_token` — so logs can be shipped
//! off-box without leaking credentials.  See `tests/config_tracing.rs`
//! for the regression contract.
use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Tracing target for config persistence events.  Stable so operators
/// can filter with `RUST_LOG=studio_worker::config=debug`.
const TRACE_TARGET: &str = "studio_worker::config";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Base URL of the studio API (e.g. `https://studio.minis.gg`).
    pub api_base_url: String,
    /// Shared secret used only for the first registration.
    pub bootstrap_token: String,
    /// Worker id, filled in by `register`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Per-worker token issued at registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// VRAM threshold the worker reports as its max claim size, in GB.
    pub vram_threshold_gb: f32,
    /// Whether to auto-launch the run loop at boot via the OS service.
    pub auto_start: bool,
    /// Whether the worker should claim new jobs.
    pub auto_enabled: bool,
    /// Engine identifier: `synthetic`, `gradio`, `multi`, or — when
    /// built with the matching cargo feature — `llama`, `whisper`,
    /// `image-candle`, `video`, `tts`.
    pub engine: String,
    /// When `engine = "multi"`, the per-modality engines to combine.
    /// First engine that claims support for a job's kind+model wins.
    #[serde(default)]
    pub engines: Vec<String>,
    /// Local Gradio endpoint URL when `engine = "gradio"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gradio_endpoint_url: Option<String>,
    /// Explicit override of supported models.  When empty, the engine
    /// reports its native list.
    #[serde(default)]
    pub supported_models_override: Vec<String>,
    /// Periodically check the release feed and auto-install newer
    /// versions when no job is running.
    #[serde(default = "default_auto_update_enabled")]
    pub auto_update_enabled: bool,
    /// How often (seconds) to check the release feed.
    #[serde(default = "default_auto_update_interval")]
    pub auto_update_interval_secs: u64,
    /// GitHub Releases feed for this binary.
    #[serde(default = "default_auto_update_feed")]
    pub auto_update_feed: String,
    /// Whether to upgrade to pre-release versions.
    #[serde(default)]
    pub auto_update_prerelease: bool,
    /// Root directory for downloaded model files (per-engine
    /// subdirectories: `llm/`, `stt/`, `tts/`, `image/`, `video/`).
    /// Defaults to the OS cache dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_root: Option<std::path::PathBuf>,
}

fn default_auto_update_enabled() -> bool {
    true
}
fn default_auto_update_interval() -> u64 {
    1800
}
fn default_auto_update_feed() -> String {
    "https://api.github.com/repos/webbertakken/studio-worker/releases".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base_url: "https://studio.minis.gg".into(),
            bootstrap_token: "dev-bootstrap-token".into(),
            worker_id: None,
            auth_token: None,
            vram_threshold_gb: 12.0,
            auto_start: true,
            auto_enabled: true,
            engine: "synthetic".into(),
            engines: Vec::new(),
            gradio_endpoint_url: None,
            supported_models_override: Vec::new(),
            auto_update_enabled: default_auto_update_enabled(),
            auto_update_interval_secs: default_auto_update_interval(),
            auto_update_feed: default_auto_update_feed(),
            auto_update_prerelease: false,
            models_root: None,
        }
    }
}

fn default_config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("gg", "minis", "minis-studio-worker")
        .ok_or_else(|| anyhow!("cannot resolve config directory"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn resolve_path(override_path: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        Ok(PathBuf::from(p))
    } else {
        default_config_path()
    }
}

pub fn load(override_path: Option<&str>) -> Result<(Config, PathBuf)> {
    let path = resolve_path(override_path)?;
    if !path.exists() {
        let cfg = Config::default();
        save(&cfg, &path)?;
        tracing::info!(
            target: TRACE_TARGET,
            op = "load",
            source = "default_created",
            config_path = %path.display(),
            engine = %cfg.engine,
            api_base_url = %cfg.api_base_url,
            vram_threshold_gb = cfg.vram_threshold_gb,
            auto_enabled = cfg.auto_enabled,
            "config file missing — bootstrapped defaults"
        );
        return Ok((cfg, path));
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| "parsing config.toml")?;
    tracing::debug!(
        target: TRACE_TARGET,
        op = "load",
        source = "existing_file",
        config_path = %path.display(),
        engine = %cfg.engine,
        api_base_url = %cfg.api_base_url,
        vram_threshold_gb = cfg.vram_threshold_gb,
        auto_enabled = cfg.auto_enabled,
        worker_id = cfg.worker_id.as_deref().unwrap_or("(unregistered)"),
        has_auth_token = cfg.auth_token.is_some(),
        "loaded config from disk"
    );
    Ok((cfg, path))
}

pub fn save(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).with_context(|| "serialising config")?;
    let bytes = text.len();
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    tracing::debug!(
        target: TRACE_TARGET,
        op = "save",
        config_path = %path.display(),
        engine = %cfg.engine,
        vram_threshold_gb = cfg.vram_threshold_gb,
        auto_enabled = cfg.auto_enabled,
        bytes = bytes,
        "persisted config to disk"
    );
    Ok(())
}

/// Wrap a Config in a mutex for use across the runtime.
pub type SharedConfig = std::sync::Arc<Mutex<Config>>;

pub fn shared(cfg: Config) -> SharedConfig {
    std::sync::Arc::new(Mutex::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_values_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.engine, "synthetic");
        assert!(cfg.auto_enabled);
        assert!(cfg.auto_start);
        assert!(cfg.auto_update_enabled);
        assert_eq!(cfg.auto_update_interval_secs, 1800);
        assert!(!cfg.auto_update_prerelease);
        assert!(cfg.auto_update_feed.contains("webbertakken/studio-worker"));
        assert_eq!(cfg.vram_threshold_gb, 12.0);
        assert!(cfg.worker_id.is_none());
        assert!(cfg.auth_token.is_none());
    }

    #[test]
    fn resolve_path_uses_override_when_provided() {
        let path = resolve_path(Some("/tmp/test-config.toml")).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/test-config.toml"));
    }

    #[test]
    fn resolve_path_defaults_when_no_override() {
        let path = resolve_path(None).unwrap();
        let s = path.to_string_lossy();
        assert!(
            s.contains("minis-studio-worker") || s.contains("minis.gg.minis-studio-worker"),
            "unexpected default path: {s}"
        );
        assert!(s.ends_with("config.toml"));
    }

    #[test]
    fn load_creates_default_when_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        let path_str = path.to_string_lossy().to_string();
        let (cfg, returned_path) = load(Some(&path_str)).unwrap();
        assert_eq!(returned_path, path);
        assert_eq!(cfg.engine, "synthetic");
        // File should have been written.
        assert!(path.exists());
    }

    #[test]
    fn round_trip_via_save_and_load_preserves_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            engine: "gradio".into(),
            gradio_endpoint_url: Some("http://example.invalid".into()),
            worker_id: Some("w-123".into()),
            auth_token: Some("tok-xyz".into()),
            vram_threshold_gb: 24.0,
            auto_update_prerelease: true,
            supported_models_override: vec!["foo".into(), "bar".into()],
            ..Config::default()
        };
        save(&cfg, &path).unwrap();

        let path_str = path.to_string_lossy().to_string();
        let (loaded, _) = load(Some(&path_str)).unwrap();
        assert_eq!(loaded.engine, cfg.engine);
        assert_eq!(loaded.gradio_endpoint_url, cfg.gradio_endpoint_url);
        assert_eq!(loaded.worker_id, cfg.worker_id);
        assert_eq!(loaded.auth_token, cfg.auth_token);
        assert_eq!(loaded.vram_threshold_gb, cfg.vram_threshold_gb);
        assert_eq!(loaded.auto_update_prerelease, cfg.auto_update_prerelease);
        assert_eq!(
            loaded.supported_models_override,
            cfg.supported_models_override
        );
    }

    #[test]
    fn shared_wraps_in_arc_mutex() {
        let cfg = Config::default();
        let shared = shared(cfg.clone());
        let guard = shared.lock();
        assert_eq!(guard.engine, cfg.engine);
    }

    #[test]
    fn load_returns_error_on_malformed_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this :: is = not = toml = :").unwrap();
        let path_str = path.to_string_lossy().to_string();
        let err = load(Some(&path_str)).unwrap_err();
        assert!(err.to_string().contains("parsing config.toml"));
    }
}
