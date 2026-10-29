//! Sentry telemetry — opt-in error/panic reporting.
//!
//! Disabled by default.  Operators enable it by setting `SENTRY_DSN`
//! (and optionally `SENTRY_ENVIRONMENT`) before launching the worker.
//! Nothing is hard-coded so the public repo never carries a DSN.
//!
//! Wiring:
//!
//! * `init()` reads env vars, constructs `SentryConfig`, and calls
//!   `sentry::init`.  The returned `ClientInitGuard` must live for the
//!   entire program — `main.rs` keeps it in a binding that drops on
//!   shutdown, flushing any in-flight events.
//! * `tracing_layer()` returns the `sentry-tracing` breadcrumb + error
//!   layers, filtered so `error` -> Sentry event, `warn` -> breadcrumb,
//!   lower -> ignored.
//!   Layered into the global `tracing-subscriber` registry in `main.rs`.
//!
//! Panic capture is on by default (the `panic` feature is part of
//! `sentry`'s default feature set).
use crate::{sys, RELEASE_NAME};
use sentry_tracing::{BreadcrumbLayer, ErrorLayer};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::Layer;

/// Tracing target for telemetry events.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::telemetry=debug`.
const TRACE_TARGET: &str = "studio_worker::telemetry";

/// Fully-resolved Sentry client configuration.  Built either from the
/// host environment (`from_env`) or — in tests — by passing inputs
/// directly to `from_env_inner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentryConfig {
    pub dsn: String,
    pub environment: String,
    pub release: String,
    pub server_name: String,
}

impl SentryConfig {
    /// Read `SENTRY_DSN` + `SENTRY_ENVIRONMENT` from the process env.
    /// Returns `None` when no DSN is set (or it's whitespace only).
    pub fn from_env() -> Option<Self> {
        Self::from_env_inner(
            std::env::var("SENTRY_DSN").ok(),
            std::env::var("SENTRY_ENVIRONMENT").ok(),
            RELEASE_NAME.to_string(),
            sys::machine_name(),
        )
    }

    /// Pure resolver used by `from_env` and the unit tests.  Keeps the
    /// env-var plumbing isolated from the decision logic so tests don't
    /// need to mutate process-global state.
    pub fn from_env_inner(
        dsn: Option<String>,
        environment: Option<String>,
        release: String,
        server_name: String,
    ) -> Option<Self> {
        let dsn = dsn?.trim().to_string();
        if dsn.is_empty() {
            return None;
        }
        let environment = environment
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "production".to_string());
        Some(Self {
            dsn,
            environment,
            release,
            server_name,
        })
    }
}

/// Build the `sentry::ClientOptions` for a resolved config.  Returns
/// `None` (and leaves a tracing warning) when the DSN string can't be
/// parsed — split out from `init` so we can exercise both branches
/// without mutating global Sentry state.
pub fn build_client_options(cfg: &SentryConfig) -> Option<sentry::ClientOptions> {
    let dsn = match cfg.dsn.parse() {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                target: TRACE_TARGET,
                error = %e,
                "ignoring SENTRY_DSN: not a valid sentry DSN"
            );
            return None;
        }
    };
    let mut options = sentry::ClientOptions::new()
        .release(cfg.release.clone())
        .environment(cfg.environment.clone())
        .server_name(cfg.server_name.clone());
    options.dsn = Some(dsn);
    // We use Sentry purely for error/panic reporting; performance
    // tracing would add network traffic for very little value on a
    // worker that already ships structured logs.
    options.traces_sampling_strategy = sentry::TracesSamplingStrategy::Disabled;
    Some(options)
}

/// Initialise Sentry from the process environment.
///
/// Returns `None` when no DSN is configured, or when Sentry rejected
/// the supplied DSN (invalid URL / unsupported scheme).  When a guard
/// is returned, callers MUST keep it alive for the lifetime of the
/// program — dropping it triggers a flush of pending events.
pub fn init() -> Option<sentry::ClientInitGuard> {
    let cfg = SentryConfig::from_env()?;
    let options = build_client_options(&cfg)?;
    let guard = sentry::init(options);
    if !guard.is_enabled() {
        tracing::warn!(
            target: TRACE_TARGET,
            "sentry::init returned a disabled client (likely invalid DSN); telemetry off"
        );
        return None;
    }
    tracing::info!(
        target: TRACE_TARGET,
        environment = %cfg.environment,
        release = %cfg.release,
        server_name = %cfg.server_name,
        "sentry telemetry enabled"
    );
    Some(guard)
}

/// What Sentry does with a tracing event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentryAction {
    /// Raise a Sentry error event (an operator-visible alert).
    Event,
    /// Record a breadcrumb attached to the next event.
    Breadcrumb,
    /// Send nothing.
    Ignore,
}

/// Map a tracing severity level to the Sentry ingest action.
///
/// * `ERROR` -> Sentry event (operator-visible alert)
/// * `WARN`  -> breadcrumb attached to the next event
/// * `INFO`/`DEBUG`/`TRACE` -> ignored (too noisy for breadcrumbs;
///   already surfaced via the structured log shipper)
///
/// Split out of [`tracing_layer`] so the severity-mapping policy is a
/// pure function unit tests can pin without constructing a subscriber
/// or mutating global Sentry state.
pub fn sentry_action_for_level(level: tracing::Level) -> SentryAction {
    match level {
        tracing::Level::ERROR => SentryAction::Event,
        tracing::Level::WARN => SentryAction::Breadcrumb,
        _ => SentryAction::Ignore,
    }
}

/// Build the `sentry-tracing` breadcrumb and error layers, each filtered
/// to the events [`sentry_action_for_level`] routes to it.  The
/// breadcrumb layer comes first so a breadcrumb recorded by an event is
/// in place before any error event that follows it.
pub fn tracing_layer<S>() -> impl Layer<S> + Send + Sync
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let routed_to =
        |action: SentryAction| filter_fn(move |md| sentry_action_for_level(*md.level()) == action);
    BreadcrumbLayer::new()
        .with_filter(routed_to(SentryAction::Breadcrumb))
        .and_then(ErrorLayer::new().with_filter(routed_to(SentryAction::Event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    fn sample_config(dsn: &str) -> SentryConfig {
        SentryConfig {
            dsn: dsn.to_string(),
            environment: "staging".into(),
            release: "studio-worker@9.9.9".into(),
            server_name: "rig-01".into(),
        }
    }

    #[test]
    fn build_client_options_carries_release_environment_and_server_name() {
        let cfg = sample_config("https://abc123@o1.ingest.sentry.io/42");
        let opts = build_client_options(&cfg).expect("a valid DSN must yield options");
        assert!(opts.dsn.is_some(), "the parsed DSN must be attached");
        assert_eq!(opts.release.as_deref(), Some("studio-worker@9.9.9"));
        assert_eq!(opts.environment.as_deref(), Some("staging"));
        assert_eq!(opts.server_name.as_deref(), Some("rig-01"));
        // Performance tracing stays off on the worker — it already ships
        // structured logs, so sampling traces would only add network
        // traffic (see build_client_options).
        assert!(
            matches!(
                opts.traces_sampling_strategy,
                sentry::TracesSamplingStrategy::Disabled
            ),
            "performance tracing must be disabled, got {:?}",
            opts.traces_sampling_strategy
        );
    }

    #[test]
    fn build_client_options_rejects_invalid_dsn_and_warns() {
        // A malformed DSN must disable telemetry with a breadcrumb —
        // never panic the worker at startup (init() propagates the
        // None via `?`).
        let cfg = sample_config("not-a-valid-dsn");
        let logs = capture(move || {
            assert!(
                build_client_options(&cfg).is_none(),
                "an unparseable DSN must yield no options"
            );
        });
        assert!(logs.contains("WARN"), "expected WARN event, got: {logs}");
        assert!(
            logs.contains("studio_worker::telemetry"),
            "expected telemetry target, got: {logs}"
        );
        assert!(
            logs.contains("not a valid sentry DSN"),
            "expected the invalid-DSN message, got: {logs}"
        );
    }

    #[test]
    fn from_env_inner_rejects_empty_string_after_trim() {
        let resolved =
            SentryConfig::from_env_inner(Some("\t \n".into()), None, "0.0.0".into(), "h".into());
        assert!(resolved.is_none());
    }

    #[test]
    fn event_filter_maps_each_severity_to_its_sentry_action() {
        // The operator-facing telemetry contract: ERROR is the only
        // level that becomes a Sentry *event* (the alert an operator
        // gets paged on), WARN attaches a breadcrumb to the next event
        // (context, not noise), and INFO/DEBUG/TRACE are ignored (they
        // already ship via the structured log shipper).  This mapping
        // had no test, so a regression flipping WARN -> Event would
        // flood Sentry with warning spam, and ERROR -> Breadcrumb would
        // silently drop every alert — both invisible until production.
        assert_eq!(
            sentry_action_for_level(tracing::Level::ERROR),
            SentryAction::Event,
            "ERROR must surface as a Sentry event"
        );
        assert_eq!(
            sentry_action_for_level(tracing::Level::WARN),
            SentryAction::Breadcrumb,
            "WARN must attach a breadcrumb, not raise an event"
        );
        for quiet in [
            tracing::Level::INFO,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ] {
            assert_eq!(
                sentry_action_for_level(quiet),
                SentryAction::Ignore,
                "{quiet} must be ignored by Sentry (it ships via the log shipper)"
            );
        }
    }

    #[test]
    fn from_env_inner_populates_all_fields() {
        let cfg = SentryConfig::from_env_inner(
            Some("https://k@example.ingest.sentry.io/1".into()),
            Some("prod".into()),
            "9.9.9".into(),
            "machine".into(),
        )
        .expect("dsn set");
        assert_eq!(cfg.dsn, "https://k@example.ingest.sentry.io/1");
        assert_eq!(cfg.environment, "prod");
        assert_eq!(cfg.release, "9.9.9");
        assert_eq!(cfg.server_name, "machine");
    }
}
