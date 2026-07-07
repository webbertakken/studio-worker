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

use crate::{
    config::{self, SharedConfig},
    engine,
    http::ApiClient,
    runtime::build_capabilities,
    secrets::{new_secret_hex, new_uuid, sha256_hex},
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
    // Bind the cloned value in its own statement so the `cfg.lock()`
    // guard releases at the `;`.  Holding it across the `match` would
    // deadlock the non-reentrant mutex the moment the `None` arm below
    // re-locks to store a freshly generated secret.
    let existing_secret = cfg.lock().registration_secret.clone();
    let secret = match existing_secret {
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

/// The instant this `request_id` first entered the Pending state, read
/// back from the shared observer.  Falls back to `now` for a fresh
/// request (or a different id), so the UI's "pending since Xs ago"
/// counts up from the real first sighting instead of resetting every
/// poll.
fn pending_since(observers: &SharedRegistration, request_id: &str) -> DateTime<Utc> {
    match &*observers.lock() {
        RegistrationState::Pending {
            request_id: prev,
            since,
        } if prev == request_id => *since,
        _ => Utc::now(),
    }
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

    // Preserve the instant we first saw *this* request go Pending, so
    // the UI's "pending since" is the real wait time.  Resetting it to
    // `now` on every 30s poll (the old behaviour) made it perpetually
    // read "0s ago".
    let since = pending_since(observers, &request_id);

    let outcome = match result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(
                target: TRACE_TARGET,
                op = "poll",
                error = %e,
                "poll failed; will retry next tick"
            );
            let state = RegistrationState::Pending { request_id, since };
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
            let state = RegistrationState::Pending { request_id, since };
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
            let state = RegistrationState::Pending { request_id, since };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_since_preserves_the_first_sighting_for_the_same_request() {
        let observers = shared_initial();
        let first = Utc::now() - chrono::Duration::seconds(90);
        *observers.lock() = RegistrationState::Pending {
            request_id: "rr-1".into(),
            since: first,
        };
        // Same request id: the original instant is preserved so the
        // UI's "pending since" counts up from the real first sighting
        // instead of resetting to 0 on every 30s poll.
        assert_eq!(pending_since(&observers, "rr-1"), first);
        // A different request id: fresh clock, not the stale instant.
        assert_ne!(pending_since(&observers, "rr-2"), first);
    }

    #[test]
    fn pending_since_starts_fresh_from_non_pending_states() {
        let observers = shared_initial(); // Pristine
        let before = Utc::now();
        let since = pending_since(&observers, "rr-1");
        assert!(since >= before, "a fresh pending starts from ~now");
    }
}
