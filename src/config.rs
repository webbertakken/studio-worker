//! Persistent config in `~/.config/minis-studio-worker/config.toml` (Linux/macOS)
//! or `%APPDATA%\minis-studio-worker\config.toml` (Windows).
//!
//! Every load/save emits a structured tracing breadcrumb so operators
//! can tell from `journalctl` which file the worker actually consulted
//! (and whether the file existed, was freshly bootstrapped with
//! defaults, or failed to read/parse).  The events deliberately omit
//! the secret fields
//! (`auth_token`, `registration_secret`) so logs can be shipped
//! off-box without leaking credentials.  See `tests/config_tracing.rs`
//! for the regression contract.
//!
//! What lives here vs. what's stripped from the user-editable surface:
//!
//! * **Operator-facing**: `api_base_url`, `vram_threshold_gb`,
//!   `auto_start`, `auto_update_*`, `models_root`.
//!   These are exposed in the desktop UI's Config tab.
//! * **Internal state, persisted but not user-editable**: `worker_id`,
//!   `auth_token`, `install_id`, `registration_request_id`,
//!   `registration_secret`.  The auto-register flow owns them; the UI
//!   hides them entirely.
//! * **Engine selection**: removed.  The runtime always builds a
//!   `MultiEngine` containing every backend compiled into this binary
//!   and routes each job to the right one.

use anyhow::{anyhow, Context, Result};
use directories::{ProjectDirs, UserDirs};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Tracing target for config persistence events.  Stable so operators
/// can filter with `RUST_LOG=studio_worker::config=debug`.
const TRACE_TARGET: &str = "studio_worker::config";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Base URL of the studio API (e.g. `https://studio.minis.gg/`).
    pub api_base_url: String,
    /// Worker id, written on operator approval.  Cleared by
    /// `studio-worker register --reset`.  Internal — not surfaced as
    /// a user-editable widget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Per-worker token issued at registration.  Internal — never
    /// surfaced in the UI and redacted from log events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// VRAM threshold the worker reports as its max claim size, in GB.
    pub vram_threshold_gb: f32,
    /// Whether to auto-launch the run loop at boot via the OS service.
    pub auto_start: bool,
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
    /// Defaults to `~/models` (resolved at load time).
    #[serde(default = "default_models_root_persisted")]
    pub models_root: PathBuf,
    /// Maximum number of WebSocket reconnect attempts before the
    /// worker gives up and exits non-zero (relying on the service
    /// manager to restart it).  `0` = infinite.  Defaults to `5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_reconnect_attempts: Option<u32>,
    /// Per-install UUID written once on first launch.  Stable across
    /// worker restarts so the studio can dedup pending requests.
    /// Internal state, populated by the auto-register flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    /// `requestId` returned by `POST /workers/register-request`.
    /// Cleared on approval / rejection.  Internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_request_id: Option<String>,
    /// Bearer secret presented when polling the request status.
    /// Cleared on approval / rejection.  Internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_secret: Option<String>,
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

/// Resolve `~/models` for the user running the worker.  Falls back to
/// `$TMPDIR/studio-worker-models` on the (extremely unusual) machines
/// where `directories` can't find a home directory.
pub fn default_models_root() -> PathBuf {
    if let Some(user) = UserDirs::new() {
        return user.home_dir().join("models");
    }
    std::env::temp_dir().join("studio-worker-models")
}

fn default_models_root_persisted() -> PathBuf {
    default_models_root()
}

/// Resolve a leading `~` to the running user's home dir.  Stops the
/// worker from creating a literal `~` directory on disk when the
/// config carries an unexpanded path (most commonly: a hand-edited
/// `models_root = "~/models"`).
fn expand_home(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return UserDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or(path);
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(d) = UserDirs::new() {
            return d.home_dir().join(rest);
        }
    }
    path
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base_url: "https://studio.minis.gg/".into(),
            worker_id: None,
            auth_token: None,
            vram_threshold_gb: 12.0,
            auto_start: true,
            auto_update_enabled: default_auto_update_enabled(),
            auto_update_interval_secs: default_auto_update_interval(),
            auto_update_feed: default_auto_update_feed(),
            auto_update_prerelease: false,
            models_root: default_models_root(),
            ws_reconnect_attempts: None,
            install_id: None,
            registration_request_id: None,
            registration_secret: None,
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
            api_base_url = %cfg.api_base_url,
            vram_threshold_gb = cfg.vram_threshold_gb,
            auto_start = cfg.auto_start,
            models_root = %cfg.models_root.display(),
            "config file missing — bootstrapped defaults"
        );
        return Ok((cfg, path));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            // Mirror save()'s failure breadcrumb: an unreadable config
            // is never silent.  The io error names the path/cause only
            // (never file content), so it is safe to log verbatim.
            tracing::warn!(
                target: TRACE_TARGET,
                op = "load",
                config_path = %path.display(),
                error = %e,
                "failed to read config file"
            );
            return Err(e).with_context(|| format!("reading {}", path.display()));
        }
    };
    let mut cfg: Config = match toml::from_str(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            // Deliberately omit the parser detail: toml renders the
            // offending source span, which can echo a secret value
            // (e.g. an unterminated `auth_token = "...`).  The path +
            // category keep the failure operator-visible without
            // risking a credential leak in journalctl / Sentry.
            tracing::warn!(
                target: TRACE_TARGET,
                op = "load",
                config_path = %path.display(),
                "config file is not valid TOML"
            );
            return Err(e).context("parsing config.toml");
        }
    };
    cfg.models_root = expand_home(std::mem::take(&mut cfg.models_root));
    tracing::debug!(
        target: TRACE_TARGET,
        op = "load",
        source = "existing_file",
        config_path = %path.display(),
        api_base_url = %cfg.api_base_url,
        vram_threshold_gb = cfg.vram_threshold_gb,
        auto_start = cfg.auto_start,
        models_root = %cfg.models_root.display(),
        worker_id = cfg.worker_id.as_deref().unwrap_or("(unregistered)"),
        has_auth_token = cfg.auth_token.is_some(),
        "loaded config from disk"
    );
    Ok((cfg, path))
}

pub fn save(cfg: &Config, path: &Path) -> Result<()> {
    match write_config(cfg, path) {
        Ok(bytes) => {
            tracing::debug!(
                target: TRACE_TARGET,
                op = "save",
                config_path = %path.display(),
                vram_threshold_gb = cfg.vram_threshold_gb,
                auto_start = cfg.auto_start,
                models_root = %cfg.models_root.display(),
                bytes = bytes,
                "persisted config to disk"
            );
            Ok(())
        }
        Err(e) => {
            // Log at the source so a failed persist is never silent,
            // regardless of whether the caller logs the returned Err
            // (the UI Save button discards it, the auto-register flow
            // logs it with extra context).  `error` carries an
            // IO / serialisation message + the path only — never the
            // config's secret fields — so this stays log-shippable.
            tracing::warn!(
                target: TRACE_TARGET,
                op = "save",
                config_path = %path.display(),
                error = %e,
                "failed to persist config to disk"
            );
            Err(e)
        }
    }
}

/// Side-effecting half of [`save`]: serialise + write, returning the
/// byte count on success.  Split out so `save` can log a structured
/// event on both the success and failure branch without duplicating
/// the happy path.
fn write_config(cfg: &Config, path: &Path) -> Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).with_context(|| "serialising config")?;
    let bytes = text.len();
    write_atomic(path, text.as_bytes())?;
    Ok(bytes)
}

/// Persist `bytes` to `path` atomically and owner-only.  The config
/// carries the worker's identity and registration secrets
/// (`auth_token`, `registration_secret`), so a plain `fs::write` is
/// unsafe on two counts:
///
/// * **Durability**: an interrupted write (crash, power loss, full
///   disk) truncates `path` to a half-written, unparseable file,
///   wiping the worker's registration and forcing a fresh operator
///   approval.  We stream into a temp file in the *same directory* (so
///   the final step is a same-filesystem rename, which is atomic) and
///   rename it over the target.  A failure leaves the previous config
///   intact and drops the temp file.
/// * **Confidentiality**: `fs::write` honours the umask and typically
///   lands `0644`, exposing the secrets to every other local user.
///   `tempfile` creates the temp file `0600` on Unix and `persist`
///   keeps that mode through the rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .with_context(|| "writing temp config")?;
    tmp.as_file()
        .sync_all()
        .with_context(|| "flushing temp config to disk")?;
    tmp.persist(path)
        .map_err(|e| anyhow!("atomically replacing {}: {}", path.display(), e.error))?;
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
        assert_eq!(cfg.api_base_url, "https://studio.minis.gg/");
        assert!(cfg.auto_start);
        assert!(cfg.auto_update_enabled);
        assert_eq!(cfg.auto_update_interval_secs, 1800);
        assert!(!cfg.auto_update_prerelease);
        assert!(cfg.auto_update_feed.contains("webbertakken/studio-worker"));
        assert_eq!(cfg.vram_threshold_gb, 12.0);
        assert!(cfg.worker_id.is_none());
        assert!(cfg.auth_token.is_none());
        // Models root defaults to ~/models (or a temp fallback on
        // headless boxes without UserDirs).
        let m = cfg.models_root.to_string_lossy().to_string();
        assert!(m.ends_with("models") || m.contains("studio-worker-models"));
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
        assert_eq!(cfg.api_base_url, "https://studio.minis.gg/");
        // File should have been written.
        assert!(path.exists());
    }

    #[test]
    fn round_trip_via_save_and_load_preserves_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            worker_id: Some("w-123".into()),
            auth_token: Some("tok-xyz".into()),
            vram_threshold_gb: 24.0,
            auto_update_prerelease: true,
            models_root: PathBuf::from("/tmp/test-models"),
            ..Config::default()
        };
        save(&cfg, &path).unwrap();

        let path_str = path.to_string_lossy().to_string();
        let (loaded, _) = load(Some(&path_str)).unwrap();
        assert_eq!(loaded.api_base_url, cfg.api_base_url);
        assert_eq!(loaded.worker_id, cfg.worker_id);
        assert_eq!(loaded.auth_token, cfg.auth_token);
        assert_eq!(loaded.vram_threshold_gb, cfg.vram_threshold_gb);
        assert_eq!(loaded.auto_update_prerelease, cfg.auto_update_prerelease);
        assert_eq!(loaded.models_root, cfg.models_root);
    }

    #[test]
    fn shared_wraps_in_arc_mutex() {
        let cfg = Config::default();
        let shared = shared(cfg.clone());
        let guard = shared.lock();
        assert_eq!(guard.api_base_url, cfg.api_base_url);
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

    #[test]
    fn load_strips_legacy_engine_fields_silently() {
        // Older configs had `engine`, `engines`, `auto_enabled`, `label`.
        // serde::Deserialize on the new struct should ignore them (they
        // aren't in the schema any more); the worker keeps running.
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let legacy = r#"
            api_base_url = "https://example.invalid"
            vram_threshold_gb = 8.0
            auto_start = true
            engine = "multi"
            engines = ["llama", "synthetic"]
            auto_enabled = false
            label = "alice's rig"
        "#;
        std::fs::write(&path, legacy).unwrap();
        let (cfg, _) = load(Some(&path.to_string_lossy())).unwrap();
        assert_eq!(cfg.api_base_url, "https://example.invalid");
        assert_eq!(cfg.vram_threshold_gb, 8.0);
    }

    #[test]
    fn load_expands_leading_tilde_in_models_root() {
        // Users who hand-edit `config.toml` often write `~/models`;
        // the worker must expand it, not create a literal `~` dir.
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let raw = r#"
            api_base_url = "https://x.invalid"
            vram_threshold_gb = 4.0
            auto_start = true
            auto_update_enabled = false
            auto_update_interval_secs = 1
            auto_update_feed = "https://x.invalid"
            auto_update_prerelease = false
            models_root = "~/models-test"
        "#;
        std::fs::write(&path, raw).unwrap();
        let (cfg, _) = load(Some(&path.to_string_lossy())).unwrap();
        assert!(
            cfg.models_root.is_absolute(),
            "~/ should expand to an absolute path, got {}",
            cfg.models_root.display()
        );
        assert!(cfg.models_root.ends_with("models-test"));
    }

    #[test]
    fn expand_home_leaves_absolute_paths_alone() {
        let p = PathBuf::from("/tmp/anywhere");
        assert_eq!(expand_home(p.clone()), p);
    }

    #[test]
    fn expand_home_handles_bare_tilde() {
        let expanded = expand_home(PathBuf::from("~"));
        assert!(
            expanded.is_absolute() || expanded == Path::new("~"),
            "bare ~ expands to home (or stays put on weird boxes), got {}",
            expanded.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_config_owner_only_because_it_holds_secrets() {
        // config.toml persists `auth_token` + `registration_secret`.
        // A plain `fs::write` honours the umask and typically lands
        // `0644`, exposing those credentials to every other local
        // user.  The atomic temp-file write must leave the file
        // owner-only (`0600`).
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            auth_token: Some("super-secret-token".into()),
            registration_secret: Some("reg-secret".into()),
            ..Config::default()
        };
        save(&cfg, &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "secrets-bearing config must not be group/world-accessible; got mode {mode:o}"
        );
    }

    #[test]
    fn save_atomically_replaces_existing_config_without_temp_litter() {
        // A second save must fully replace the file (no stale fields
        // from a longer previous version) and leave no temp-file
        // siblings behind from the write-then-rename dance.
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let big = Config {
            api_base_url: "https://a-very-long-host-name.example.invalid/studio/".into(),
            worker_id: Some("worker-with-a-longish-id-000000".into()),
            ..Config::default()
        };
        save(&big, &path).unwrap();

        let small = Config {
            api_base_url: "https://x/".into(),
            ..Config::default()
        };
        save(&small, &path).unwrap();

        let (loaded, _) = load(Some(&path.to_string_lossy())).unwrap();
        assert_eq!(loaded.api_base_url, "https://x/");
        assert!(
            loaded.worker_id.is_none(),
            "a replacing save must not leave the previous worker_id behind"
        );

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["config.toml".to_string()],
            "atomic save must leave only the target file, found: {names:?}"
        );
    }
}
