//! Auto-register state machine — the only registration path.
//!
//! On first launch the worker POSTs `/workers/register-request`
//! to the studio with a self-generated install id + a registration
//! secret (only its SHA-256 hash leaves the box), then polls
//! `/workers/register-requests/<id>` every 30s for the operator's
//! decision.  On Approved we persist `worker_id` + `auth_token` to
//! `config.toml` and fall through to the normal heartbeat / claim
//! loops.  On Rejected we surface the reason; the user clears state
//! with `studio-worker register --reset` to retry.
//!
//! `tick()` does at most one HTTP round-trip per call so the outer
//! orchestrator can sleep between polls.  All persistence goes
//! through `config::save` so a crash mid-flight leaves consistent
//! on-disk state.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::{
    config::{self, SharedConfig},
    engine,
    http::ApiClient,
    runtime::build_capabilities,
    types::{AutoRegisterRequest, RegisterStatus},
    AGENT_VERSION,
};

/// Tracing target for the registration state machine.  Stable so
/// operators can filter the worker's most-asked-about flow ("why is my
/// worker stuck unregistered?") with
/// `RUST_LOG=studio_worker::auto_register=debug`.
const TRACE_TARGET: &str = "studio_worker::auto_register";

/// What `tick()` returns + what the UI Status tab reads.  Distinct
/// from the persisted config fields, which carry the raw building
/// blocks (`install_id`, `registration_request_id`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationState {
    /// First-launch default; no request in flight, no worker_id.
    Pristine,
    /// Studio has a Pending row for us; we're polling for the
    /// operator's decision.
    Pending {
        request_id: String,
        /// First time we saw this request in the Pending state.
        since: DateTime<Utc>,
    },
    /// `worker_id` + `auth_token` are in config; ready for the
    /// normal heartbeat / claim loops.
    Approved,
    /// Operator rejected the request.  Worker stops trying;
    /// `studio-worker register --reset` clears the state.
    Rejected { reason: String },
}

/// Shared in-memory mirror of `RegistrationState` for the UI to read
/// (the persisted source of truth is `Config`).
pub type SharedRegistration = Arc<Mutex<RegistrationState>>;

pub fn shared_initial() -> SharedRegistration {
    Arc::new(Mutex::new(RegistrationState::Pristine))
}

/// One iteration of the state machine.
///
/// Reads the current `Config` snapshot, decides what to do, performs
/// at most one HTTP call, persists changes via `config::save`,
/// mirrors the new state into `observers`, and returns it.
///
/// Idempotent: re-running with the same on-disk state and a
/// pending-returning studio is a no-op on disk.
pub async fn tick(
    cfg: &SharedConfig,
    config_path: &Path,
    observers: &SharedRegistration,
) -> RegistrationState {
    // Fast path: already registered.
    {
        let snap = cfg.lock();
        if snap.worker_id.is_some() && snap.auth_token.is_some() {
            *observers.lock() = RegistrationState::Approved;
            return RegistrationState::Approved;
        }
    }

    // Ensure install_id + secret are present before doing any HTTP.
    ensure_install_state(cfg, config_path);

    // Branch on whether we already have a request id.
    let (api_base_url, request_id, secret, install_id) = {
        let snap = cfg.lock();
        (
            snap.api_base_url.clone(),
            snap.registration_request_id.clone(),
            snap.registration_secret.clone(),
            snap.install_id.clone(),
        )
    };

    match (request_id, secret) {
        (Some(rid), Some(sec)) => {
            poll_existing(cfg, config_path, observers, api_base_url, rid, sec).await
        }
        _ => {
            create_request(
                cfg,
                config_path,
                observers,
                api_base_url,
                install_id.expect("ensure_install_state seeds install_id"),
            )
            .await
        }
    }
}

fn ensure_install_state(cfg: &SharedConfig, config_path: &Path) {
    let mut snap = cfg.lock();
    let mut dirty = false;
    if snap.install_id.is_none() {
        snap.install_id = Some(new_uuid());
        dirty = true;
    }
    // Pre-allocate the secret only if we also have no request id.
    // Otherwise the existing pair is still valid.
    if snap.registration_request_id.is_none() && snap.registration_secret.is_none() {
        snap.registration_secret = Some(new_secret_hex());
        dirty = true;
    }
    if dirty {
        let snapshot = snap.clone();
        drop(snap);
        if let Err(e) = config::save(&snapshot, config_path) {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "ensure-install",
                config_path = %config_path.display(),
                error = %e,
                "failed to persist install state"
            );
        }
    }
}

async fn create_request(
    cfg: &SharedConfig,
    config_path: &Path,
    observers: &SharedRegistration,
    api_base_url: String,
    install_id: String,
) -> RegistrationState {
    let secret = match cfg.lock().registration_secret.clone() {
        Some(s) => s,
        None => {
            // Should never happen post-ensure_install_state, but be safe.
            let s = new_secret_hex();
            cfg.lock().registration_secret = Some(s.clone());
            s
        }
    };
    let secret_hash = sha256_hex(&secret);

    // Build the capabilities snapshot the operator will see.
    let payload = match build_payload(cfg, install_id.clone(), secret_hash) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "register-request",
                error = %e,
                "engine build failed during register-request"
            );
            return RegistrationState::Pristine;
        }
    };

    let api_base_url_for_task = api_base_url.clone();
    let payload_for_task = payload.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_> {
        let api = ApiClient::new(api_base_url_for_task)?;
        api.register_request(&payload_for_task)
    })
    .await;

    let response = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "register-request",
                error = %e,
                "register-request HTTP failed; will retry next tick"
            );
            return RegistrationState::Pristine;
        }
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "register-request",
                error = %e,
                "register-request task panic; will retry next tick"
            );
            return RegistrationState::Pristine;
        }
    };

    // Persist requestId.
    let now = Utc::now();
    {
        let mut snap = cfg.lock();
        snap.registration_request_id = Some(response.request_id.clone());
        let snapshot = snap.clone();
        drop(snap);
        if let Err(e) = config::save(&snapshot, config_path) {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "register-request",
                config_path = %config_path.display(),
                error = %e,
                "failed to persist request_id"
            );
        }
    }
    let state = RegistrationState::Pending {
        request_id: response.request_id,
        since: now,
    };
    *observers.lock() = state.clone();
    state
}

async fn poll_existing(
    cfg: &SharedConfig,
    config_path: &Path,
    observers: &SharedRegistration,
    api_base_url: String,
    request_id: String,
    secret: String,
) -> RegistrationState {
    let api_base_url_for_task = api_base_url.clone();
    let request_id_for_task = request_id.clone();
    let secret_for_task = secret.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_> {
        let api = ApiClient::new(api_base_url_for_task)?;
        api.poll_register_status(&request_id_for_task, &secret_for_task)
    })
    .await;

    let outcome = match result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "poll",
                error = %e,
                "poll failed; will retry next tick"
            );
            let state = RegistrationState::Pending {
                request_id,
                since: Utc::now(),
            };
            *observers.lock() = state.clone();
            return state;
        }
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "poll",
                error = %e,
                "poll task panic; will retry next tick"
            );
            let state = RegistrationState::Pending {
                request_id,
                since: Utc::now(),
            };
            *observers.lock() = state.clone();
            return state;
        }
    };

    match outcome {
        None => {
            // 404: studio doesn't know this request id anymore.  Drop
            // the stale id + secret so the next tick creates fresh.
            {
                let mut snap = cfg.lock();
                snap.registration_request_id = None;
                snap.registration_secret = None;
                let snapshot = snap.clone();
                drop(snap);
                if let Err(e) = config::save(&snapshot, config_path) {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "poll",
                        config_path = %config_path.display(),
                        error = %e,
                        "failed to persist cleared request state after stale 404; the stale request id stays on disk until the next successful save"
                    );
                }
            }
            *observers.lock() = RegistrationState::Pristine;
            RegistrationState::Pristine
        }
        Some(RegisterStatus::Pending) => {
            let state = RegistrationState::Pending {
                request_id,
                since: Utc::now(),
            };
            *observers.lock() = state.clone();
            state
        }
        Some(RegisterStatus::Approved {
            worker_id,
            auth_token,
        }) => {
            {
                let mut snap = cfg.lock();
                snap.worker_id = Some(worker_id);
                snap.auth_token = Some(auth_token);
                snap.registration_request_id = None;
                snap.registration_secret = None;
                let snapshot = snap.clone();
                drop(snap);
                if let Err(e) = config::save(&snapshot, config_path) {
                    tracing::error!(
                        target: TRACE_TARGET,
                        op = "poll",
                        config_path = %config_path.display(),
                        error = %e,
                        "failed to persist approved credentials; this session is registered in memory but the worker will re-register from scratch on the next restart"
                    );
                }
            }
            *observers.lock() = RegistrationState::Approved;
            RegistrationState::Approved
        }
        Some(RegisterStatus::Rejected { reason }) => {
            {
                let mut snap = cfg.lock();
                snap.registration_request_id = None;
                snap.registration_secret = None;
                let snapshot = snap.clone();
                drop(snap);
                if let Err(e) = config::save(&snapshot, config_path) {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "poll",
                        config_path = %config_path.display(),
                        error = %e,
                        "failed to persist cleared request state after rejection; the stale request id stays on disk until the next successful save"
                    );
                }
            }
            let state = RegistrationState::Rejected { reason };
            *observers.lock() = state.clone();
            state
        }
    }
}

fn build_payload(
    cfg: &SharedConfig,
    install_id: String,
    registration_secret_hash: String,
) -> Result<AutoRegisterRequest> {
    let snap = cfg.lock().clone();
    let engine_handle = engine::build(&snap)?;
    let capabilities = build_capabilities(&snap, &*engine_handle);
    Ok(AutoRegisterRequest {
        install_id,
        registration_secret_hash,
        capabilities,
        user_agent: format!("studio-worker/{AGENT_VERSION}"),
    })
}

fn new_uuid() -> String {
    // UUIDv4-ish without pulling in the `uuid` crate: 16 random bytes
    // formatted as 8-4-4-4-12.
    let bytes: [u8; 16] = rand_bytes::<16>();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn new_secret_hex() -> String {
    // 32 bytes of randomness = 64 hex chars (256 bits of entropy).
    let bytes: [u8; 32] = rand_bytes::<32>();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fill `N` bytes from the OS cryptographically-secure RNG via the
/// `getrandom` crate (`getrandom(2)` / `/dev/urandom` on Linux,
/// `getentropy` on macOS, `BCryptGenRandom` on Windows).  Used for
/// the install id and the registration secret, both of which must be
/// unpredictable: an attacker who could guess the secret could claim
/// another box's pending registration.
///
/// Panics if the OS entropy source is unavailable.  That only happens
/// on a fundamentally broken platform, and failing loudly (the panic
/// is captured by Sentry) is the right call — minting a guessable
/// secret from a timestamp would be worse than a clean crash.
///
/// This used to hand-roll a `/dev/urandom` read on unix and silently
/// fall back to a SHA-256-mixed timestamp on Windows (and on any I/O
/// error), which left the Windows secret predictable.  `getrandom` is
/// a tiny syscall wrapper already in the dep tree, so the whole
/// per-OS dance collapses into one secure, testable path.
fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS entropy source (getrandom) unavailable");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uuid_has_expected_shape() {
        let id = new_uuid();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn new_uuid_is_unique() {
        let a = new_uuid();
        let b = new_uuid();
        assert_ne!(a, b);
    }

    #[test]
    fn new_secret_hex_is_64_chars() {
        let s = new_secret_hex();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex("abc"), sha256_hex("abc"));
        assert_ne!(sha256_hex("abc"), sha256_hex("abd"));
        assert_eq!(sha256_hex("").len(), 64);
    }

    // ---------------------------------------------------------------
    // Entropy primitive.  `rand_bytes` is the single source for the
    // install id + registration secret on every platform, so these
    // also cover the formerly-untested Windows path (which used to
    // route through a predictable timestamp fallback).
    // ---------------------------------------------------------------

    #[test]
    fn rand_bytes_are_distinct_across_many_calls() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..2_000 {
            assert!(
                seen.insert(rand_bytes::<32>()),
                "rand_bytes produced a duplicate 32-byte value"
            );
        }
    }

    #[test]
    fn rand_bytes_cover_every_bit_position() {
        // OR + AND across many samples: a stuck or constant source
        // would leave a bit position never set (an OR-zero) or never
        // cleared (an AND-one).  An OS CSPRNG flips every one of the
        // 256 bits within a handful of samples.
        let mut ever_set = [0u8; 32];
        let mut ever_clear = [0xffu8; 32];
        for _ in 0..256 {
            let b = rand_bytes::<32>();
            for i in 0..32 {
                ever_set[i] |= b[i];
                ever_clear[i] &= b[i];
            }
        }
        assert_eq!(ever_set, [0xffu8; 32], "a bit position was never set");
        assert_eq!(ever_clear, [0u8; 32], "a bit position was never cleared");
    }
}
