# AGENTS.md

This file provides guidance to Claude Code (claude.ai/code) when working
with code in this repository.

## Overview

`studio-worker` is a pull-based generation worker for the minis.gg
studio.  It registers with the studio API, heartbeats, claims jobs that
fit its VRAM threshold, runs them locally (synthetic or a real
backend), and posts the results back.

One binary runs as two processes: the **daemon** (`studio-worker run`)
hosts everything (studio session, local API, model host, streaming speech
listener, auto-updater, every job); the **tray UI** (`studio-worker ui`) is
a client of the daemon over the local API and starts one when none runs.
Design: [`@docs/runtime/daemon-and-tray.md`](docs/runtime/daemon-and-tray.md).

The repo is public and CI runs on free-tier GitHub Actions, so all tests
must run without a GPU.

## Commands

| Task            | Command                                                                  |
| --------------- | ------------------------------------------------------------------------ |
| Run the daemon  | `cargo run -- run`                                                       |
| Run the tray UI | `cargo run -- ui` (starts a daemon when none runs)                       |
| Isolated run    | `cargo run -- --config <tmp>/config.toml ui` with `HOME=<tmp>`            |
| Build (release) | `cargo build --release`                                                  |
| Compile check   | `cargo check`                                                            |
| Lint            | `cargo clippy --tests -- -D warnings`                                    |
| Lint, headless  | `cargo clippy --tests --no-default-features -- -D warnings`              |
| Lint, all       | `cargo clippy --tests --features all -- -D warnings`                     |
| Format check    | `cargo fmt --check`                                                      |
| Format          | `cargo fmt`                                                              |
| Test            | `cargo test`                                                             |
| Single test     | `cargo test <test_name>`                                                 |
| Coverage gate   | `cargo +nightly llvm-cov --no-default-features --fail-under-lines 90` (plus the `--ignore-filename-regex` in `coverage.yml`) |

`ui` rewrites the login entry under `$HOME` to the running exe: give test runs their own `HOME`.
`STUDIO_WORKER_UI_PAGE=<page>` and `STUDIO_WORKER_UI_JOB=<id|latest>` pick the first page and job.

`./.cargo/config.toml` caps `cargo build` at 2 parallel jobs by default so
local builds don't saturate the dev box.  Override with `--jobs N` when on
CI.

## Tech stack

- **Rust 2021 edition** with Cargo (pinned via `rust-toolchain.toml`)
- **clap** — CLI parsing
- **tokio-tungstenite** (rustls-tls-webpki-roots) — WebSocket session to the
  studio `WorkerConnections` Durable Object; carries every worker-side
  frame except the multipart `complete` upload
- **reqwest** (blocking, rustls) — HTTP client for the surviving
  `/register` + multipart `/complete` routes and model downloads. rustls
  (not native-tls) so a source build needs no OpenSSL.
- **sentry** (rustls transport) — opt-in error reporting, also OpenSSL-free.
- **futures-util** — sink/stream combinators for the WS split
- **thiserror** — typed errors on the WS client surface
- **serde / serde_json / toml** — wire-format + config persistence
- **image** — encode synthetic WEBP/PNG output
- **wiremock** — test-only mock HTTP server for integration tests
- **tracing / tracing-subscriber** — structured logging
- **egui / eframe** (`ui` feature, **on by default**) — the tray UI: a
  navigation rail (Jobs, Models, Worker, Logs, Config), a pulse header
  (running job, daemon, studio, GPU memory, Pause), a status bar; dark and
  light palettes held to WCAG AA by tests; line icons painted from
  geometry. glow/dlopen GL so the build needs no pkg-config / GTK.
- **notify-rust** (`ui`) — OS-native desktop notifications (zbus on
  Linux, pure Rust; no libdbus).
- **System tray** (`ui`): **ksni** (pure-Rust StatusNotifierItem) on
  Linux — no GTK; **tray-icon** (native) on macOS / Windows. Abstracted
  by `src/ui/tray_host.rs`.
- **winreg** (Windows only) — real HKCU `…\Run` autostart entry.

## Project layout

- `src/main.rs` — CLI entry point.
- `src/lib.rs` — exposes the library surface so integration tests can
  drive the contract without going through the CLI.
- `src/test_support.rs` — shared test-only helpers, exposed
  (`#[doc(hidden)]`) so integration tests can reuse them.
- `src/cli.rs` — clap CLI definitions, kept out of `main.rs` so
  they're testable.
- `src/config.rs` — TOML config persisted next to a per-user dir.
- `src/auto_register.rs` — auto-register state machine (Pristine →
  Pending → Approved); the only registration path.
- `src/telemetry.rs` — opt-in Sentry error/panic reporting + the
  `sentry-tracing` layer.  Off unless `SENTRY_DSN` is set.
- `src/update.rs` — auto-update: poll GitHub Releases, download
  cargo-dist's installer on a newer semver, re-exec into it.
- `src/autostart.rs` — per-OS autostart-on-login: Linux `.desktop`,
  macOS LaunchAgent, Windows HKCU `…\Run` registry value (winreg).
  `ui::run` reconciles it with `auto_start` on launch.
- `src/engine/` — pluggable inference engines (`SyntheticEngine` +
  `MultiEngine` dispatcher, `SdCppEngine`, plus feature-gated `llama`
  / `whisper` / `image-candle` / `image-onnx` (LaMa removal) / `video`
  / `tts` backends). Shared
  on-demand model provisioning lives in `src/engine/download.rs`
  (cache + Content-Length verify + path-traversal guard).
  `src/engine/sd_provision.rs` auto-provisions the `sd-cli` binary +
  preflights the Vulkan loader — see
  [`@docs/engines/sdcpp.md`](docs/engines/sdcpp.md) for the platform
  matrix, the GPU-runtime requirement, and the `sdcpp-prebuilt`
  workflow.
- `src/http.rs` — `ApiClient` wrapping the surviving HTTP routes
  (`register` + multipart `complete`).
- `src/local_api.rs` — the daemon's loopback HTTP API (bearer token):
  generation, catalogue, model lifecycle, `/daemon/*` control, job logs
  and thumbnails.  The tray UI's only way into the daemon.
- `src/control.rs` — `DaemonControl`: status snapshot, pause, config
  update, registration reset, shutdown; `src/daemon_api.rs` — its wire
  types, shared with the UI.
- `src/daemon_lock.rs` — one daemon per config dir (`daemon.lock`).
- `src/daemon_client.rs` + `src/daemon_link.rs` — the UI side: a blocking
  client found via `local-api.json`, the 1 s `Poller` into a `Replica`,
  the daemon starter, operator actions.
- `src/host.rs` — the model host: loaded models, lanes, residency,
  admission, swaps; with `src/lifecycle.rs` (per-model state machine),
  `src/residency.rs` (`residency.json`), `src/admission.rs` (free device
  memory), `src/loaders.rs` (in-process loaders per engine).
- `src/catalog.rs` — the local model catalogue (`models.json`);
  `src/local.rs` — local jobs without the studio (transient or on a lane).
- `src/job_run.rs`, `src/job_log.rs`, `src/thumbnail.rs`, `src/job_gate.rs` —
  one job's bookkeeping (running list, `job` span, rings), per-job logs and
  the worker log ring, image thumbnails, the one-transient-job gate.
- `src/stt_stream/` — the LAN streaming speech-to-text listener
  (`server`, `session`, `vad`, `tokens`).
- `src/net.rs` (download guards), `src/secrets.rs` (local token entropy).
- `src/ui/` (feature `ui`) — the tray UI, a client of the daemon:
  `mod.rs` (`ui::run`: UI lock, login entry, poller, eframe, display
  wait), `single_instance.rs` (`ui.lock` + `ui.raise`), `app.rs`,
  `chrome.rs` (rail, pulse header, status bar), `pulse.rs`, `theme.rs`,
  `prefs.rs` (`ui.toml`), `widgets.rs`, `icons.rs`, `log_view.rs`,
  `format.rs`, `page.rs`, `pages/{jobs,models,worker,logs,config}.rs`,
  `tray.rs` + `tray_host.rs`, `notifier.rs`, `actions.rs`.  Pure view
  models are unit-tested; rendering stays thin and is smoke-tested with
  `egui::__run_test_ui`.
- `src/runtime.rs` — the daemon (`run`: lock, local API, registration
  gate, session + updater loops), CLI helpers, `WorkerObservers`.  The
  session loop lives in `src/ws/session.rs`.
- `src/ws/{client,session,types}.rs` — WebSocket client + session
  + wire-format types mirroring `apps/studio/src/shared/types/workerWs.ts`.
- `src/service.rs` — systemd / launchd / scheduled-task installers.
- `src/sys.rs` — host probes (hostname, username, VRAM).
- `src/types.rs` — shared types (capabilities, tasks, results) used by
  both the HTTP and the WS surfaces.

Integration tests in `tests/`:

WebSocket session + wire format:

- `tests/ws_wire.rs` — round-trip every frame against the TS contract.
- `tests/ws_client_contract.rs` — WS client against a real
  tokio-tungstenite server (upgrade, hello, 401 → AuthFailed, close
  4001 → AuthFailed, binary-frame rejection, close idempotency).
- `tests/ws_session_full_loop.rs` — end-to-end hello → welcome →
  LLM offer → accept + completeJson → STT offer → accept +
  completeJson → clean close.

Surviving HTTP surface (wiremock):

- `tests/http_contract.rs` — register + multipart `complete` against
  wiremock.
- `tests/http_errors.rs` — error-status paths + tracing-emission.

Auto-register + register CLI:

- `tests/auto_register_http.rs` — register-request + poll-status wire
  contract against a wiremock fake studio.
- `tests/auto_register_orchestration.rs` — the orchestration tick
  driving Pristine → Pending → Approved with config persistence.
- `tests/auto_register_save_tracing.rs` — regression cover for silent
  `config::save` failures inside the poll loop.
- `tests/auto_register_log_fields.rs` — every auto-register breadcrumb
  carries structured `op` + `error` fields (register-request + poll
  WARN, credential-save ERROR).
- `tests/register_reset.rs` — `register` CLI contracts (`--reset`
  clears local registration state, etc.).
- `tests/registration_gate.rs` — `runtime::ensure_registered`, the
  startup gate: already-registered short-circuit, Ctrl-C abort,
  operator-rejection (with reset guidance), and approval pass-through.

Runtime helpers + loops:

- `tests/runtime_helpers.rs` — one-shot CLI helpers + cli dispatch
  (wiremock studio + temp config dir).
- `tests/runtime_observers.rs` — `WorkerObservers` slots the optional
  native UI subscribes to.
- `tests/runtime_startup_tracing.rs` — startup banner + `set_threshold`
  emit operator-visible tracing.
- `tests/runtime_ticks.rs` — per-tick auto-updater loop + clean-abort
  smoke test for `runtime::run`.

Auto-update:

- `tests/auto_update.rs` — update check against a wiremock GitHub
  Releases feed (no installer execution).

Engines + multi-modal:

- `tests/multi_modal.rs` — every TaskKind round-trips through the
  synthetic engine + decoders.
- `tests/engine_tracing.rs` — every engine emits tracing on dispatch
  and on its key failure paths.
- `tests/engine_download.rs` — the shared model downloader against a
  wiremock server (happy path, non-2xx, cache reuse).
- `tests/sd_provision.rs` — the `sd-cli` auto-provisioner against a
  wiremock release zip (download + extract, cache reuse, missing-binary
  error).

Telemetry + host-probe tracing:

- `tests/config_tracing.rs` — config persistence leaves tracing
  breadcrumbs on load/save.
- `tests/host_probe_tracing.rs` — `sys.rs` probes (VRAM, hostname,
  user) leave tracing breadcrumbs.
- `tests/telemetry.rs` — Sentry telemetry contract.

Real-backend E2E (feature-gated, off on free-tier CI — download real
weights, run with the matching feature):

- `tests/real_candle_image.rs` — `image-candle` SD v1.5 image gen.
- `tests/real_llama.rs` — `llama` GGUF chat-completion.
- `tests/real_whisper.rs` — `whisper` whisper-tiny.en STT.

## CI

- `.github/workflows/checks.yml` — fmt + clippy + cargo check + tests
  (frees ~25 GB of runner bloat first so the heavy candle/whisper legs
  + cache-save don't exhaust the disk).
- `.github/workflows/coverage.yml` — nightly `cargo llvm-cov`,
  `--fail-under-lines 90` (nightly so the `coverage(off)` exclusions apply).
- `.github/workflows/audit.yml` — `cargo audit` advisory gate on
  Cargo manifest changes + weekly cron; accepted informational
  advisories live in `.cargo/audit.toml`.
- `.github/workflows/build.yml` — matrix release build on every PR.
- `.github/workflows/commit-lint.yml` — semantic PR title check.
- `.github/workflows/lint-workflows.yml` — actionlint on workflow files.
- `.github/workflows/release-please.yml` — bump version + changelog.
- `.github/workflows/release.yml` — cargo-dist build + publish on tag push.
- `.github/workflows/publish-crate.yml` — publish to crates.io on tag push.
- `.github/workflows/sdcpp-prebuilt.yml` — manual: build + host `sd-cli`
  for platforms upstream doesn't (Linux arm64). Re-run when the pinned
  sd.cpp ref (`DEFAULT_RELEASE_TAG`) changes.

Repo secrets required:

- `RELEASE_TOKEN` — a fine-grained PAT with `contents: write` + `pull_requests: write`,
  used by release-please to open its release PRs.  `GITHUB_TOKEN` alone
  cannot create PRs from a workflow.

## Releasing

- release-please runs with `skip-github-release: true`, so merging its
  release PR bumps the version + changelog but does **not** tag.
- A maintainer must then push the tag manually — that's what triggers
  `release.yml` (cargo-dist) + `publish-crate.yml`:
  `git tag -a studio-worker-v<X.Y.Z> -m "studio-worker <X.Y.Z>" <merge-sha> && git push origin studio-worker-v<X.Y.Z>`.
- After tagging, relabel the merged release PR `autorelease: tagged`
  (remove `autorelease: pending`) or the next release-please run aborts
  with "untagged, merged release PRs outstanding".
- Pre-1.0 a `feat` bumps the patch (`bump-patch-for-minor-pre-major`).

## Rules

- For this repo feel free to push, make PRs and merge them at will.
- Public repo — never commit secrets, internal URLs, or non-public
  customer identifiers.
- All tests must run in GitHub Actions free-tier — no GPU, no real
  studio.  Use wiremock for the studio API.
- Conventional-commit PR titles are enforced.  Keep first line ≤ 52
  characters.
- Don't add hard dependencies that pull in heavy native libs (CUDA,
  Torch, etc.) at the top level — they belong behind a feature flag if
  ever needed.
- `default` features ship the full turnkey engine set (`ui`, `llama`,
  `video`, `tts`, `image-onnx`) so `cargo install studio-worker` and the
  prebuilt installer both "just work" with no `--features`. Never shrink
  `default` to `ui`-only: a worker without `image-onnx` cannot run the
  LaMa removals Find-the-Differences needs (it fails with "no `onnx`
  engine compiled into this worker"). Only conflicting engines (`whisper`,
  `image-candle`) stay opt-in.
- The prebuilt installer script is the recommended end-user install (no
  toolchain needed). `cargo install` builds from source and needs a
  C/C++ toolchain (cmake/cc) for llama.cpp.
