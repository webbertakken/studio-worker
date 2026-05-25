# AGENTS.md

This file provides guidance to Claude Code (claude.ai/code) when working
with code in this repository.

## Overview

`studio-worker` is a pull-based image-generation agent for the minis.gg
studio.  It registers with the studio API, heartbeats, claims jobs that
fit its VRAM threshold, runs them locally (synthetic or Gradio), and
posts the results back.

The repo is public and CI runs on free-tier GitHub Actions, so all tests
must run without a GPU.

## Commands

| Task            | Command                                |
| --------------- | -------------------------------------- |
| Run             | `cargo run -- run`                     |
| Build (release) | `cargo build --release`                |
| Compile check   | `cargo check`                          |
| Lint            | `cargo clippy --tests -- -D warnings`  |
| Format check    | `cargo fmt --check`                    |
| Format          | `cargo fmt`                            |
| Test            | `cargo test`                           |
| Single test     | `cargo test <test_name>`               |

`./.cargo/config.toml` caps `cargo build` at 2 parallel jobs by default so
local builds don't saturate the dev box.  Override with `--jobs N` when on
CI.

## Tech stack

- **Rust 2021 edition** with Cargo (pinned via `rust-toolchain.toml`)
- **clap** — CLI parsing
- **tokio-tungstenite** (rustls-tls-webpki-roots) — WebSocket session to the
  studio `WorkerConnections` Durable Object; carries every worker-side
  frame except the multipart `complete` upload
- **reqwest** (blocking) — HTTP client for the surviving `/register` +
  multipart `/complete` routes
- **futures-util** — sink/stream combinators for the WS split
- **thiserror** — typed errors on the WS client surface
- **serde / serde_json / toml** — wire-format + config persistence
- **image** — encode synthetic WEBP/PNG output
- **wiremock** — test-only mock HTTP server for integration tests
- **tracing / tracing-subscriber** — structured logging
- **egui / eframe** (`ui` feature, off by default) — native desktop
  UI with tab shell, in-window register form, live job + heartbeat
  view, full config editor, log tail, manual update check
- **tray-icon / notify-rust** (`ui` feature, off by default) — system
  tray icon with Open / Pause-Resume / Quit menu + opt-in OS-native
  desktop notifications on job completion / failure
- **gtk** (`ui` feature, Linux only) — needed for `gtk::init()` on a
  dedicated thread so the tray's AppIndicator backend works

## Project layout

- `src/main.rs` — CLI entry point.
- `src/lib.rs` — exposes the library surface so integration tests can
  drive the contract without going through the CLI.
- `src/config.rs` — TOML config persisted next to a per-user dir.
- `src/engine/` — pluggable inference engines (`SyntheticEngine`,
  `GradioEngine`, plus feature-gated real backends).
- `src/http.rs` — `ApiClient` wrapping the surviving HTTP routes
  (`register` + multipart `complete`).
- `src/runtime.rs` — CLI helpers + auto-updater loop.  The session
  loop has moved to `src/ws/session.rs`.
- `src/ws/{client,session,types}.rs` — WebSocket client + session
  + wire-format types mirroring `apps/studio/src/shared/types/workerWs.ts`.
- `src/service.rs` — systemd / launchd / scheduled-task installers.
- `src/sys.rs` — host probes (hostname, username, VRAM).
- `src/types.rs` — shared types (capabilities, tasks, results) used by
  both the HTTP and the WS surfaces.

Integration tests in `tests/`:

- `tests/ws_wire.rs` — round-trip every frame against the TS contract.
- `tests/ws_client_contract.rs` — WS client against a real
  tokio-tungstenite server (upgrade, hello, 401 → AuthFailed, close
  4001 → AuthFailed, binary-frame rejection, close idempotency).
- `tests/ws_session_full_loop.rs` — end-to-end hello → welcome →
  LLM offer → accept + completeJson → STT offer → accept +
  completeJson → clean close.
- `tests/http_contract.rs` — register + multipart `complete` against
  wiremock.
- `tests/http_errors.rs` — error-status paths + tracing-emission.
- `tests/gradio_engine.rs` — GradioEngine against a wiremock fake Gradio.

## CI

- `.github/workflows/checks.yml` — fmt + clippy + cargo check + tests.
- `.github/workflows/build.yml` — matrix release build on every PR.
- `.github/workflows/commit-lint.yml` — semantic PR title check.
- `.github/workflows/lint-workflows.yml` — actionlint on workflow files.
- `.github/workflows/release-please.yml` — bump version + changelog.
- `.github/workflows/release.yml` — cargo-dist build + publish on tag push.

Repo secrets required:

- `RELEASE_TOKEN` — a fine-grained PAT with `contents: write` + `pull_requests: write`,
  used by release-please to open its release PRs.  `GITHUB_TOKEN` alone
  cannot create PRs from a workflow.

## Rules

- Public repo — never commit secrets, internal URLs, or non-public
  customer identifiers.
- All tests must run in GitHub Actions free-tier — no GPU, no real
  studio.  Use wiremock for the studio API and Gradio.
- Conventional-commit PR titles are enforced.  Keep first line ≤ 52
  characters.
- Don't add hard dependencies that pull in heavy native libs (CUDA,
  Torch, etc.) at the top level — they belong behind a feature flag if
  ever needed.
