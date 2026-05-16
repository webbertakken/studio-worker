//! Persistent config in `~/.config/minis-studio-worker/config.toml` (Linux/macOS)
//! or `%APPDATA%\minis-studio-worker\config.toml` (Windows).
use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// Engine identifier (`synthetic` or `gradio`).
    pub engine: String,
    /// Local Gradio endpoint URL when `engine = "gradio"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gradio_endpoint_url: Option<String>,
    /// Explicit override of supported models.  When empty, the engine
    /// reports its native list.
    #[serde(default)]
    pub supported_models_override: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base_url: "http://localhost:9790".into(),
            bootstrap_token: "dev-bootstrap-token".into(),
            worker_id: None,
            auth_token: None,
            vram_threshold_gb: 12.0,
            auto_start: true,
            auto_enabled: true,
            engine: "synthetic".into(),
            gradio_endpoint_url: None,
            supported_models_override: Vec::new(),
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
        return Ok((cfg, path));
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| "parsing config.toml")?;
    Ok((cfg, path))
}

pub fn save(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).with_context(|| "serialising config")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Wrap a Config in a mutex for use across the runtime.
pub type SharedConfig = std::sync::Arc<Mutex<Config>>;

pub fn shared(cfg: Config) -> SharedConfig {
    std::sync::Arc::new(Mutex::new(cfg))
}
