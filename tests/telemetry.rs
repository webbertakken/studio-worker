//! Sentry telemetry contract.
//!
//! Two surfaces are exercised:
//!
//! 1. `telemetry::SentryConfig::from_env_inner` — the pure resolver
//!    that turns env-var inputs into a fully populated client config.
//!    All branches (missing DSN, blank DSN, default environment,
//!    explicit environment) are tested without touching real env vars
//!    so the suite stays parallel-safe.
//!
//! 2. `telemetry::tracing_layer()` — the `sentry-tracing` layer that
//!    forwards `tracing::error!` events to Sentry while dropping
//!    lower-severity events.  Driven via `sentry::test::with_captured_events`
//!    which installs a thread-local hub with a noop transport.
use studio_worker::telemetry::{self, SentryConfig};
use tracing_subscriber::prelude::*;

const TEST_DSN: &str = "https://public@example.ingest.sentry.io/1";

// ---------------------------------------------------------------------------
// SentryConfig::from_env_inner
// ---------------------------------------------------------------------------

#[test]
fn from_env_inner_is_none_when_dsn_missing() {
    let resolved = SentryConfig::from_env_inner(
        None,
        Some("production".into()),
        "1.2.3".into(),
        "host".into(),
    );
    assert!(resolved.is_none());
}

#[test]
fn from_env_inner_is_none_when_dsn_blank() {
    let resolved =
        SentryConfig::from_env_inner(Some("   ".into()), None, "1.2.3".into(), "host".into());
    assert!(resolved.is_none());
}

#[test]
fn from_env_inner_defaults_environment_to_production() {
    let cfg =
        SentryConfig::from_env_inner(Some(TEST_DSN.into()), None, "1.2.3".into(), "host-a".into())
            .expect("dsn set");
    assert_eq!(cfg.dsn, TEST_DSN);
    assert_eq!(cfg.environment, "production");
    assert_eq!(cfg.release, "1.2.3");
    assert_eq!(cfg.server_name, "host-a");
}

#[test]
fn from_env_inner_treats_blank_environment_as_default() {
    let cfg = SentryConfig::from_env_inner(
        Some(TEST_DSN.into()),
        Some("   ".into()),
        "1.2.3".into(),
        "host-a".into(),
    )
    .expect("dsn set");
    assert_eq!(cfg.environment, "production");
}

#[test]
fn from_env_inner_uses_explicit_environment() {
    let cfg = SentryConfig::from_env_inner(
        Some(TEST_DSN.into()),
        Some("staging".into()),
        "1.2.3".into(),
        "host-a".into(),
    )
    .expect("dsn set");
    assert_eq!(cfg.environment, "staging");
}

#[test]
fn from_env_inner_trims_whitespace_around_dsn() {
    let cfg = SentryConfig::from_env_inner(
        Some(format!("   {TEST_DSN}   ")),
        Some("dev".into()),
        "1.2.3".into(),
        "host".into(),
    )
    .expect("dsn set");
    assert_eq!(cfg.dsn, TEST_DSN);
}

// ---------------------------------------------------------------------------
// tracing_layer — error -> event, warn -> breadcrumb, lower -> ignored
// ---------------------------------------------------------------------------

#[test]
fn tracing_layer_forwards_error_events_to_sentry() {
    let events = sentry::test::with_captured_events(|| {
        let subscriber = tracing_subscriber::registry().with(telemetry::tracing_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("boom from telemetry test");
        });
    });
    assert_eq!(
        events.len(),
        1,
        "expected exactly one event, got {events:?}"
    );
    let msg = events[0]
        .message
        .clone()
        .or_else(|| events[0].logentry.as_ref().map(|le| le.message.clone()))
        .unwrap_or_default();
    assert!(
        msg.contains("boom from telemetry test"),
        "expected event message to contain marker, got {msg:?}"
    );
}

#[test]
fn tracing_layer_ignores_info_events() {
    let events = sentry::test::with_captured_events(|| {
        let subscriber = tracing_subscriber::registry().with(telemetry::tracing_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("calm info message");
        });
    });
    assert!(events.is_empty(), "expected no events, got {events:?}");
}

#[test]
fn tracing_layer_drops_warn_to_breadcrumb_only() {
    // A solitary warn must not produce an event (it becomes a
    // breadcrumb attached to a future error, instead).
    let events = sentry::test::with_captured_events(|| {
        let subscriber = tracing_subscriber::registry().with(telemetry::tracing_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("careful");
        });
    });
    assert!(events.is_empty(), "warn must not emit an event: {events:?}");
}

#[test]
fn tracing_layer_attaches_preceding_warns_as_breadcrumbs() {
    let events = sentry::test::with_captured_events(|| {
        let subscriber = tracing_subscriber::registry().with(telemetry::tracing_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("first breadcrumb");
            tracing::warn!("second breadcrumb");
            tracing::error!("trigger");
        });
    });
    assert_eq!(events.len(), 1);
    let crumbs: Vec<&str> = events[0]
        .breadcrumbs
        .iter()
        .filter_map(|b| b.message.as_deref())
        .collect();
    assert!(
        crumbs.iter().any(|m| m.contains("first breadcrumb")),
        "expected first warn breadcrumb, got {crumbs:?}"
    );
    assert!(
        crumbs.iter().any(|m| m.contains("second breadcrumb")),
        "expected second warn breadcrumb, got {crumbs:?}"
    );
}

// ---------------------------------------------------------------------------
// init() — no DSN means no client; we just exercise the env-driven path
// here because installing a real client mutates global state and would
// trample on other tests in the suite.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// build_client_options — DSN parse branches
// ---------------------------------------------------------------------------

#[test]
fn build_client_options_populates_release_environment_and_server_name() {
    let cfg = SentryConfig {
        dsn: TEST_DSN.into(),
        environment: "staging".into(),
        release: "4.5.6".into(),
        server_name: "box-7".into(),
    };
    let opts = telemetry::build_client_options(&cfg).expect("valid DSN parses");
    assert_eq!(opts.release.as_deref(), Some("4.5.6"));
    assert_eq!(opts.environment.as_deref(), Some("staging"));
    assert_eq!(opts.server_name.as_deref(), Some("box-7"));
    assert_eq!(opts.traces_sample_rate, 0.0);
    assert!(opts.dsn.is_some(), "DSN should round-trip into options");
}

#[test]
fn build_client_options_returns_none_for_invalid_dsn() {
    let cfg = SentryConfig {
        dsn: "not-a-valid-dsn".into(),
        environment: "production".into(),
        release: "1.0.0".into(),
        server_name: "host".into(),
    };
    assert!(telemetry::build_client_options(&cfg).is_none());
}

#[test]
fn init_returns_none_when_dsn_env_not_set() {
    // The suite must not have SENTRY_DSN set; if it does the test
    // harness is misconfigured.  Re-asserting here makes that obvious.
    assert!(
        std::env::var_os("SENTRY_DSN").is_none(),
        "test harness must not set SENTRY_DSN"
    );
    let guard = telemetry::init();
    assert!(guard.is_none());
}
