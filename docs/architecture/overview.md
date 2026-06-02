# studio-worker: architecture overview

`studio-worker` is a single self-contained Rust binary that pulls
**image**, **LLM**, **audio (STT/TTS)**, and **video** generation
jobs from the [minis.gg studio](https://studio.minis.gg), runs them
locally, and posts the results back.  It's deliberately one process:
no helper daemons, no shared secrets, no out-of-band setup.  An
operator clicks Approve in the studio dashboard once per machine,
and the worker takes over from there.

This page is the canonical "how does the whole thing work" reference.
For install / register / day-one instructions see the top-level
[README](../../README.md); for plans-in-flight see
[`plans/`](../../plans).

## Table of contents

1. [Two-binary big picture](#two-binary-big-picture)
2. [Process lifecycle](#process-lifecycle)
3. [Source-tree map](#source-tree-map)
4. [Registration (auto-register-with-approval)](#registration-auto-register-with-approval)
5. [The WebSocket session](#the-websocket-session)
6. [Engine abstraction](#engine-abstraction)
7. [Job lifecycle (one claim end-to-end)](#job-lifecycle-one-claim-end-to-end)
8. [Config + persisted state](#config--persisted-state)
9. [Optional desktop UI](#optional-desktop-ui)
10. [Auto-update](#auto-update)
11. [Observability](#observability)
12. [Service / autostart](#service--autostart)
13. [Failure modes + reconnect policy](#failure-modes--reconnect-policy)
14. [Security model](#security-model)
15. [Studio side (minigames repo)](#studio-side-minigames-repo)

---

## Two-binary big picture

```
  +-----------------+         WebSocket session (long-lived)
  |  studio-worker  | <----+---------------------------------+
  |   (Rust, this   |      |                                 |
  |     repo)       |      | + heartbeats every 5s           |
  |                 |      | + claim/offer/accept frames     |
  |                 |      | + completeJson / fail frames    |
  |                 |      | + log batches (1Hz)             |
  +-----------------+      |                                 |
       ^   ^               v                                 |
       |   |    +-------------------+                        |
       |   |    | studio Worker     |                        |
       |   |    | (Cloudflare,      |                        |
       |   |    | minigames repo)   |                        |
       |   |    +-------------------+                        |
       |   |          ^   ^                                  |
       |   |          |   |                                  |
       |   |          |   +--- D1: studioWorkers /           |
       |   |          |        workerRegistrationRequests /  |
       |   |          |        graphicsJobs / workerLogs     |
       |   |          |                                      |
       |   |          +--- React dashboard at                |
       |   |               studio.minis.gg                   |
       |   |                                                 |
       |   |    Bytes upload (HTTP multipart):               |
       |   +--- POST /workers/:id/jobs/:jobId/complete       |
       |                                                     |
       |    Auto-register + poll (HTTP):                     |
       +--- POST /workers/register-request                   |
            GET  /workers/register-requests/:id              |
                                                             |
            (operator approves in dashboard) ----------------+
```

The worker speaks **three** different surfaces to the studio:

| Channel | Lifetime | Carries |
|---|---|---|
| `POST /workers/register-request` + `GET /workers/register-requests/:id` | One-shot at install + 30s polling until approved | Operator-gated registration; mints `worker_id` + `auth_token` |
| WebSocket at `GET /workers/:id/connect` | Long-lived, reconnect on disconnect | Heartbeats, claim offers (carrying the [`ModelSource`](../runtime/model-source.md) the worker needs to download + run the model), accept/reject, complete-json, fail, log batches |
| `POST /workers/:id/jobs/:jobId/complete` (multipart) | Per finished job with binary output | Image / audio / video bytes \u2192 R2 |

Everything else (heartbeat ack, accept, fail, log shipping, etc.) is
WebSocket frames \u2014 the legacy `/heartbeat`, `/claim`,
`/complete-json`, `/fail`, `/logs` HTTP routes are gone.

---

## Process lifecycle

```
main.rs (process entry)
   |
   v
tokio runtime + tracing/sentry init  (telemetry.rs)
   |
   v
cli.rs::Cli::parse   ->   lib.rs::run_cli  ->  match on Command
   |
   v
runtime.rs::run       (or ui::run, or one-shot helpers)
   |
   +--> 1. config::load              (config.rs)
   +--> 2. ensure_registered         (calls auto_register::tick in a loop until Approved)
   +--> 3. run_loops                 (spawns the WS session + auto-updater)
              |
              +--> ws::session::spawn_ws_session  (heartbeats, claim, complete, fail, logs)
              +--> runtime::spawn_auto_updater    (release-feed poll + re-exec)
```

The CLI surface from [`src/cli.rs`](../../src/cli.rs):

| Subcommand | What it does |
|---|---|
| `run` | Start the runtime: ensure registered, then the WS session + auto-updater |
| `ui` (feature `ui`) | Same as `run` but with the egui window + tray + notifications |
| `register` | Persist api-base-url / clear state (`--reset`).  **No HTTP** \u2014 the next `run`/`ui` actually auto-registers |
| `status` | Print config path, registration state, threshold, auto-update toggle |
| `set-threshold <gb>` | Update `vram_threshold_gb` |
| `install-service` / `uninstall-service` | Per-OS service file (systemd / launchd / scheduled task) |
| `config` | Dump the resolved config |
| `check-update` | One-shot release-feed poll, doesn't install |

---

## Source-tree map

```
src/
├── main.rs           Thin process entry; sets up tokio + sentry + tracing, dispatches to lib::run_cli.
├── lib.rs            Module re-exports + run_cli dispatch table.
├── cli.rs            clap definitions.  Tested in-module.
├── config.rs         Config struct + load/save (~/.config/minis-studio-worker/config.toml).
├── runtime.rs        run/run_loops/register/status/format_status, the auto-update tick,
│                     the ensure_registered helper, WorkerObservers, JobOutcome.
├── auto_register.rs  State machine (Pristine/Pending/Approved/Rejected) + tick().
│                     install_id + registration_secret generation; SHA-256 hashing.
├── http.rs           Thin reqwest::blocking wrapper.  Two methods left now:
│                     register_request + poll_register_status + complete (multipart).
├── types.rs          Wire types shared with the studio: WorkerCapabilities, Task*,
│                     TaskResult, JobClaim, LogEntry, AutoRegisterRequest, RegisterStatus.
├── sys.rs            hostname/username/VRAM probe.
├── service.rs        Per-OS service file writers (systemd --user / launchd / schtasks XML).
├── autostart.rs      Cross-OS "run in tray on login" toggle (logged; desktop UI calls it).
├── update.rs         GitHub release feed poll + installer script download + re-exec on success.
├── telemetry.rs      Sentry init (opt-in via SENTRY_DSN env var) + tracing-subscriber layer.
├── test_support.rs   #[doc(hidden)] tracing capture helper for integration tests.
│
├── engine/           Pluggable inference backends.
│   ├── mod.rs        Engine trait + dispatch / dispatch_with_source.  Always-on SyntheticEngine.
│   ├── multi.rs      MultiEngine; routes strictly by ModelSource.engine (no fallback).
│   ├── sdcpp.rs      Real image inference via stable-diffusion.cpp subprocess.
│   ├── llama.rs      (feature `llama`) llama-cpp-2 wrapper for LLM tasks.
│   ├── whisper.rs    (feature `whisper`) whisper-rs wrapper for STT.
│   ├── candle_image.rs (feature `image-candle`) candle-transformers SD pipeline.
│   ├── video.rs      (feature `video`) animated-GIF video stand-in (no ffmpeg).
│   └── tts.rs        (feature `tts`) pure-Rust formant-synth TTS stand-in.
│
├── ws/               Replaces the four old polling loops with one WS session.
│   ├── mod.rs        Re-exports.
│   ├── types.rs      WorkerInbound / WorkerOutbound frame enums (mirror TS contract).
│   ├── client.rs     tokio-tungstenite wrapper; connect/send/recv; WsClientError.
│   └── session.rs    spawn_ws_session: connect, hello, heartbeat, offer-handler,
│                     log-flush, reconnect with exponential backoff.
│
└── ui/               (feature `ui`) Native egui desktop window.
    ├── mod.rs        ui::run: load config, spawn auto-register + run_loops on tokio,
    │                 hand main thread to eframe.  Tray install (Linux GTK thread).
    ├── app.rs        eframe App impl: tab dispatch, shared state, hide-to-tray, quit.
    ├── tab.rs        Tab enum + STUDIO_WORKER_UI_TAB env override for screenshots.
    ├── tabs/
    │   ├── status.rs Initialising / Pending / Rejected / Registered view models.
    │   ├── jobs.rs   Current card + bounded recent-jobs ring.
    │   ├── config.rs Every Config field as a widget; Save writes through.
    │   ├── logs.rs   Level filter + free-text search + auto-scroll, windowed.
    │   └── about.rs  Version / sentry release / config path / Check for updates.
    ├── tray.rs       3-variant icon (idle/busy/disconnected), menu factory.
    └── notifier.rs   Trait + DesktopNotifier + per-event NotificationPrefs gate.
```

Pluggable engine backends are gated behind cargo features so the
default build stays small and CI fast.  See
[`plans/real-engines.md`](../../plans/real-engines.md) for the
per-feature build matrix.

---

## Registration (auto-register-with-approval)

**No shared secret ever leaves the studio.**  Every worker auto-registers
on first launch and waits for the operator to click Approve in the
studio dashboard.  Implemented across
[`src/auto_register.rs`](../../src/auto_register.rs),
[`src/types.rs`](../../src/types.rs), and
[`src/http.rs`](../../src/http.rs); orchestration in
[`src/runtime.rs::ensure_registered`](../../src/runtime.rs).

### State machine

```
                       +-----------------+
                       |    Pristine     |  ← first launch, between requests, or
                       +-----------------+    after `register --reset`
                                |
                                | tick: POST /workers/register-request
                                | (body: installId, registrationSecretHash,
                                |        capabilities, label?, userAgent)
                                v
                       +-----------------+
                       |    Pending      |  ← config now has request_id +
                       |  { request_id,  |    registration_secret; UI shows
                       |    since }      |    "Waiting for approval"
                       +-----------------+
                                |
                  tick: GET /workers/register-requests/:id
                  bearer = registration_secret
                                |
              +--------+--------+--------+
              |        |        |        |
              v        v        v        v
       (pending)  (approved) (rejected) (404)
              |        |        |        |
              |        v        v        +-> Pristine (stale id, recreate)
              |  Approved   Rejected
              |  + writes   { reason }
              |  worker_id  --> loop exits; UI shows reason
              |  + auth_token  --> user runs `register --reset`
              |  to disk
              v
       (next tick — no HTTP, fast-path returns Approved)
```

### Per-install identity

- `install_id` — UUIDv4 generated on first launch, persisted in
  `config.toml`.  Stable across worker restarts so the studio can dedup
  re-submissions (operator hasn't decided yet → re-post returns the
  existing `requestId`).
- `registration_secret` — 256 bits of randomness from `/dev/urandom`
  on unix.  Hex-encoded.  Stored locally; **only the SHA-256 hash**
  leaves the box (sent on the initial POST, then presented as the
  raw Bearer when polling).
- `registration_request_id` — `rr-<uuid>` returned by the studio.
  Both this and the secret are cleared on Approved / Rejected.

### Capabilities snapshot

Each `register-request` carries a full
[`WorkerCapabilities`](../../src/types.rs):

- `machineName`, `username` (host identity from `whoami`)
- `agentVersion` (from `Cargo.toml`)
- `engine` (`multi` — the dispatcher wrapping every compiled-in backend)
- `vramTotalGb` (probed from `/proc/driver/nvidia/gpus` on Linux; 0 elsewhere)
- `vramThresholdGb` (operator-set max GB per claim)
- `autoEnabled`, `autoStart` (operator toggles)
- `supportedModels` (flat list across all task kinds)
- `taskKinds` (image / llm / audio_stt / audio_tts / video)
- `supportedModelsPerKind` (per-kind breakdown)

The operator sees all of this in the dashboard's Pending Workers row
before deciding.

### Operator override

There is no operator override.  Even the studio owner registers via
the same Pending → Approve flow.  This is intentional:

- Removes the chicken-and-egg of "how does Webber bootstrap his own
  worker without distributing a token to himself".
- Single source of truth for `studioWorkers` rows; no
  bootstrap-token-minted-out-of-band hidden path.
- Auditable: `workerRegistrationRequests.decided_by` records the
  approving studio user.

---

## The WebSocket session

After auto-register succeeds, [`ws::session::spawn_ws_session`](../../src/ws/session.rs)
opens a single long-lived WebSocket at `GET /workers/:id/connect` and
the heartbeat / claim / complete / fail / log pipelines all flow over
it as JSON frames.

Wire format mirrors `apps/studio/src/shared/types/workerWs.ts`.
Defined in [`src/ws/types.rs`](../../src/ws/types.rs) as two enums:

| Direction | Frame | Carries |
|---|---|---|
| → server | `Hello` | `authToken` + capabilities (sent immediately after upgrade) |
| → server | `Heartbeat` | capabilities + current_job_id (every 5s) |
| → server | `Accept` | `jobId` (responding to an Offer) |
| → server | `Reject` | `jobId` + `reason` (engine can't serve this model/kind) |
| → server | `CompleteJson` | `jobId` + `result` JSON (LLM, STT) |
| → server | `Fail` | `jobId` + `error` + `retryable` |
| → server | `LogBatch` | drained log entries (every 1s) |
| → server | `ReadyForMore` | hint that backpressure has cleared |
| server → | `Welcome` | `workerId` + server time (post-Hello ack) |
| server → | `Offer` | `JobOfferClaim` (worker chooses Accept or Reject) |
| server → | `HeartbeatAck` | (per heartbeat) |
| server → | `CompleteAck` | `jobId` (post-CompleteJson) |
| server → | `FailAck` | `jobId` (post-Fail) |
| server → | `Error` | `code` + `message` (auth, protocol, duplicate, deleted) |

The `complete` route for image / audio / video bytes is a separate
HTTP multipart upload \u2014 R2 doesn't fit cleanly into WS frames.
Everything else stays on the session.

### Session loop

[`spawn_ws_session`](../../src/ws/session.rs) wraps
`run_one_session` in a reconnect loop:

```
attempt = 0
loop:
   if stop: return Stopped
   match run_one_session():
     Stopped       → return
     AuthFailed    → return (do not reconnect; user must --reset)
     Fatal(msg)    → return (e.g. duplicate worker, missing creds)
     Disconnected  → back off BASE_BACKOFF_MS * 2^attempt, capped at
                     MAX_BACKOFF_MS (30s).  attempt += 1.
                     Out of attempts (default 5) → return Err so the
                     service manager restarts the binary.
```

Constants live at the top of `ws/session.rs`:

| Constant | Value |
|---|---|
| `HEARTBEAT_INTERVAL` | 5s |
| `LOG_FLUSH_INTERVAL` | 1s |
| `SHUTDOWN_TICK` | 250ms |
| `BASE_BACKOFF_MS` | 1 000 |
| `MAX_BACKOFF_MS` | 30 000 |
| `DEFAULT_RECONNECT_ATTEMPTS` | 5 |

`cfg.ws_reconnect_attempts` overrides the default.

---

## Engine abstraction

[`src/engine/mod.rs`](../../src/engine/mod.rs) defines:

```rust
pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> EngineCapabilities;
    fn dispatch(&self, model: &str, task: Task) -> Result<TaskResult>;

    // Dispatch with the offer's ModelSource attached.  Engines that
    // need the download spec / CLI defaults (sdcpp) override it;
    // engines that don't (synthetic) inherit this default.
    fn dispatch_with_source(
        &self,
        model: &str,
        task: Task,
        _source: &ModelSource,
    ) -> Result<TaskResult> {
        self.dispatch(model, task)
    }
}
```

`TaskResult` is tagged by kind:

- `Image { bytes, ext }` (webp / png)
- `Llm { json }` (OpenAI-shape `chat.completion`)
- `AudioStt { json }` (whisper-shape segments)
- `AudioTts { bytes, ext }` (wav)
- `Video { bytes, ext }` (animated webp from synthetic, gif from the `video` feature)

Engines are no longer config-selectable.  `engine::build()` always
returns a `MultiEngine` populated with every backend compiled into
this binary; per-offer routing happens inside the MultiEngine and is
driven by the offer's `ModelSource.engine` field (see [Job
lifecycle](#job-lifecycle-one-claim-end-to-end)).

Built-in:

- **`synthetic`** \u2014 deterministic real bytes for every kind,
  keyed by SHA-256 of the prompt.  Real WEBP, real WAV, real animated
  WEBP, real OpenAI-shaped JSON.  No GPU, no model downloads, ~0ms
  per task.  Powers CI + smoke-tests.  Advertises only `synthetic*`
  model names so it never claims a real-model job (it would happily
  upload placeholder bytes for a real manifest, which is destructive
  on a live queue).
- **`sdcpp`** \u2014 real image inference via `stable-diffusion.cpp` as a
  subprocess.  Reads the `ModelSource` off every offer, downloads
  any missing files into `cfg.models_root`, invokes `sd-cli` with
  the right `--diffusion-model` / `--llm` / `--vae` flags + CLI
  defaults from the source.  Image kind only today.  Deep dive in
  [`docs/engines/sdcpp.md`](../engines/sdcpp.md).

The legacy `gradio` engine is gone (operators run a Gradio app via
an external service if they need it).  Feature-gated heavyweights
(`llama`, `whisper`, `image-candle`, `video`, `tts`) still drop in
via the same trait when their cargo features are enabled \u2014 see
[`plans/real-engines.md`](../../plans/real-engines.md).

---

## Job lifecycle (one claim end-to-end)

```
1. Studio queues a graphicsJobs row (status=queued, model=X, vram=Y)

2. Server picks a worker whose:
     - capabilities.supportedModels contains X
     - vramThresholdGb >= Y
     - last heartbeat fresh (< 30s)
   Model-name matching is gone — the studio attaches the download
   spec, the worker is dumb.  Server pushes an Offer frame down the
   WS session with the model + ModelSource included.

3. Worker receives Offer:
     - Sends Accept frame; sets busy flag; populates
       `observers.current_job` for the Jobs tab.
     - Hands the task to `engine.dispatch_with_source(model, task,
       source)` on a blocking thread.
     - The MultiEngine routes by `source.engine`; the sdcpp engine
       ensures every file in `source.files` is cached under
       `cfg.models_root` (downloading any missing ones), then runs
       `sd-cli` with the CLI defaults.
     - If the engine bails: sends `Fail { error, retryable }`.

4. Engine produces a TaskResult:
     - Image / AudioTts / Video → HTTP POST multipart to
       `/workers/:id/jobs/:jobId/complete` (R2 upload), then
       success log entry.
     - Llm / AudioStt → WS frame CompleteJson with the JSON payload.

5. Server marks job done, sends CompleteAck, populates
   graphicsJobs.completedAt + R2 key.

6. Worker:
     - Clears busy flag.
     - Pushes CurrentJob → RecentJob in the observers ring (UI Status
       + Jobs tabs surface this).

Server-driven offer pipeline: the next Offer comes from the studio's
`notifyJobCompleted` (defer'd from the multipart route's `waitUntil`),
not from the worker.  The worker no longer sends `ReadyForMore` —
the dual trigger raced the studio's `commitOffer` and produced
`protocol_violation: accept for unknown jobId` errors that killed
sessions.

If engine returns Err:
     - Worker sends Fail { error, retryable }.
     - Server requeues (retryable) or marks failed (terminal).
```

Rules worth pinning explicitly:

- **Selection is kind-based, not model-name-based.**  The studio's
  `pickWorkerForJob` and `findQueuedJobForWorker` filter on the
  worker's `taskKinds`.  Model-name whitelisting on the worker is
  gone (a brief `'*'` wildcard sentinel shipped + got reverted in
  the same session as the model registry; the registry approach is
  cleaner because the studio already knows everything about the
  model).
- **Only one Offer in flight per worker.**  Server-driven offer
  cadence as above; no worker-side `ReadyForMore`.
- **Hello waits for Welcome before starting heartbeat / log-shipper.**
  `tokio::interval()` ticks at t=0, so the first heartbeat used to
  race the studio's async Hello-auth flow and trip
  `protocol_violation: session not authenticated`.  The session
  loop now blocks on the Welcome reply before spawning the
  background pumps.
- **Worker waits for credentials before opening a session.**  The
  UI's parallel auto-register + WS-session flow used to race; the
  WS session now polls the shared config every second until
  `worker_id` + `auth_token` are populated, rather than
  fatal-bailing on first attempt.

The runtime tracks all three observable slots in
[`runtime::WorkerObservers`](../../src/runtime.rs):

- `current_job: Option<CurrentJob>` \u2014 set during dispatch
- `recent_jobs: VecDeque<RecentJob>` (cap 50, newest-first)
- `last_heartbeat: Option<HeartbeatStatus>` \u2014 written after every
  WS heartbeat ack / failure

These are `Arc<Mutex<…>>` and read directly by the UI for live state.

---

## Config + persisted state

[`src/config.rs`](../../src/config.rs) defines the persisted
`Config` struct.

**File location** (via the `directories` crate):

- Linux / macOS: `~/.config/minis-studio-worker/config.toml`
- Windows: `%APPDATA%\minis-studio-worker\config.toml`

**Operator-facing fields** (exposed in the UI's Config tab):

| Field | Default | Purpose |
|---|---|---|
| `api_base_url` | `https://studio.minis.gg/` | Studio API root |
| `vram_threshold_gb` | `12.0` | Max VRAM per claim |
| `auto_start` | `true` | OS service auto-start at boot |
| `auto_update_enabled` | `true` | Check the GitHub release feed |
| `auto_update_interval_secs` | `1800` | How often (default 30 min) |
| `auto_update_feed` | release URL | GitHub feed to poll |
| `auto_update_prerelease` | `false` | Track pre-releases |
| `models_root` | `~/models` (resolved at load) | Where downloaded model files live |

**Internal state** (persisted but not exposed in the UI; the
auto-register flow owns it end-to-end):

| Field | Purpose |
|---|---|
| `worker_id` | Filled on operator approval; presented in the WS URL path |
| `auth_token` | Filled on operator approval; presented in WS Hello + the multipart `complete` Bearer |
| `ws_reconnect_attempts` | WS session reconnect budget (defaults to `5` when unset) |
| `install_id` | Per-install UUID generated on first launch |
| `registration_request_id` | Set during Pending, cleared on Approved/Rejected |
| `registration_secret` | Same |

**Runtime-only** (not in the file at all):

| Flag | Where it lives | Purpose |
|---|---|---|
| `paused: Arc<AtomicBool>` | Top-level state passed into `runtime::run_loops` | Operator pause toggle.  When true, heartbeats advertise `autoEnabled = false` and incoming offers are rejected.  Restarts come up unpaused.  See [`docs/runtime/pause-resume.md`](../runtime/pause-resume.md). |

The legacy fields `engine`, `engines`, `gradio_endpoint_url`,
`supported_models_override`, `auto_enabled` and `label` are gone:
engine selection is automatic ([Engine abstraction](#engine-abstraction)),
the runtime pause flag replaces `auto_enabled`, and the studio's
Pending Workers panel no longer surfaces a label.

Every load + save emits a structured `tracing` event on the
`studio_worker::config` target with the resolved path \u2014 makes
"why is the worker reading the wrong config" trivially debuggable from
`journalctl`.  `auth_token` and `registration_secret` are
**deliberately omitted** from these events so logs ship off-box
without leaking credentials.

Coverage regression contract in
[`tests/config_tracing.rs`](../../tests/config_tracing.rs).

---

## Optional desktop UI

Built behind the `ui` cargo feature; brings in `egui` + `eframe` +
`tray-icon` + `notify-rust` + GTK on Linux.  Off by default so the
headless server install stays lean.

### Tab structure

| Tab | What it shows |
|---|---|
| **Status** | Worker id, API URL, VRAM total / threshold, IDLE / BUSY / PAUSED badge, last heartbeat freshness, **Pause / Resume button** (flips the runtime `paused` flag).  When unregistered: Initialising / Pending (with request id + copy button) / Rejected (with reason + `--reset` hint) state. |
| **Jobs** | Current job card (kind, model, prompt preview, elapsed) + last 50 finished jobs with outcome / duration. |
| **Config** | The operator-facing subset of `Config` as widgets, grouped into Connection (API base URL) / Worker (VRAM threshold + Auto-start) / Auto-update / Models (folder picker for `models_root`) / Notifications / Background mode.  Save writes through; Reset reverts.  Internal state (`worker_id`, `auth_token`, `install_id`, registration ids) is deliberately not shown \u2014 the auto-register flow owns it. |
| **Logs** | Level filter (all/info/warn/error), free-text search across category/message/job id, auto-scroll toggle.  Reads from `WorkerObservers.recent_logs` (bounded 1000-entry ring) so it doesn't blank out when the WS log-shipper drains every second. |
| **About** | Version, Sentry release name, config path, manual "Check for updates" button. |

Screenshots in [`docs/screenshots/`](../screenshots/).

### Tray icon

Three coloured variants derived from `(busy, last_heartbeat)`:

- **Idle** \u2014 green; not busy + heartbeat fresh + ok
- **Busy** \u2014 amber; busy flag set
- **Disconnected** \u2014 red; heartbeat stale (> 3 \u00d7 interval), missing,
  or returned an error

Menu: **Open Window** / **Pause / Resume** / **Quit**.  The label
flips between Pause and Resume based on the runtime `paused` flag.

Closing the window hides to the tray; loops keep running.  Quit comes
from the tray menu (signals `stop`, awaits in-flight job up to ~5s,
then exits).

**Per-OS backends** ([`src/ui/tray_host.rs`](../../src/ui/tray_host.rs)):
Linux uses **ksni** (pure-Rust StatusNotifierItem over zbus) so the
build needs no GTK; the tray runs on the tokio runtime and the menu
`activate` callbacks drive the shared `paused` / `quit` flags + an
egui repaint.  macOS / Windows use **tray-icon** (native APIs), built
on the eframe main thread, with menu events arriving through muda's
global `MenuEvent::receiver()` channel.  Either backend is
best-effort — the window UI works without a tray.

### Notifications

OS-native desktop notifications via `notify-rust`, gated behind a
`Notifier` trait so tests inject a `CapturingNotifier` and assert
what would have been shown.  Both completion and failure
notifications are off by default, opt-in per-event from the Config
tab.

---

## Auto-update

[`src/update.rs`](../../src/update.rs) + the `spawn_auto_updater`
loop in `runtime.rs`.

Every `auto_update_interval_secs` (default 30 min):

1. Confirm no job is in flight (the shared `busy: AtomicBool` from
   the WS session).
2. GET the configured `auto_update_feed` (GitHub Releases API by
   default).
3. Compare highest published semver to `AGENT_VERSION`.
4. If newer:
   - Download the per-platform cargo-dist installer script.
   - Run it (overwrites the binary in place).
   - On unix: `execvp` the new binary, replacing this process.
   - On Windows: spawn the successor + exit, since `execvp` isn't
     a clean fit.

The flow short-circuits when `auto_update_enabled = false` or when
the worker is mid-job.  Between checks the idle wait is stop-aware: it
re-polls the shared `stop` flag every `AUTO_UPDATE_SHUTDOWN_TICK`
(default 250 ms) via `wait_with_stop`, so a SIGTERM / SIGINT during the
idle window stops the worker promptly instead of blocking
`run_loops`' join for a whole `auto_update_tick`.  The
`RealRunner::{download, run_installer}`
+ `restart_self` paths are tested through a fake `UpdateRunner`
trait \u2014 they're excluded from the 90% coverage gate
(`.cargo-llvm-cov.toml`).

---

## Observability

- **Local logs**: every `tracing` event is rendered through
  `tracing-subscriber::fmt` to stderr.  Filter via
  `RUST_LOG=studio_worker=debug` (or any of the per-target filters
  documented per module: `studio_worker::http`,
  `studio_worker::config`, `studio_worker::runtime`,
  `studio_worker::ws::session`, `studio_worker::ws::client`, etc.).
  The `studio_worker::ws::client` target carries transport-boundary
  breadcrumbs (connect / recv / send / close) so a dropped frame or a
  dead studio is never silent, even though the session discards recv
  errors and fires `let _ = sender.send(...)`.
- **Studio-side logs**: every tick of the worker pushes its log
  buffer over the WS LogBatch frame.  The studio drops them into the
  `workerLogs` D1 table; the dashboard's LogViewer renders them.
- **In-UI logs tab**: same buffer, virtualised view, level filter +
  search.
- **Sentry (opt-in)**: set `SENTRY_DSN` (and optionally
  `SENTRY_ENVIRONMENT`) before launch.  Captures panics, forwards
  `tracing::error!` events, attaches preceding `warn!` events as
  breadcrumbs.  Tags with `release = studio-worker@<version>` and
  `server_name = <hostname>`.  Performance tracing intentionally off.

---

## Service / autostart

Two distinct mechanisms:

### `studio-worker install-service` (headless background)

[`src/service.rs`](../../src/service.rs).  Writes a per-OS unit
file:

- Linux: `systemd --user` unit at
  `~/.config/systemd/user/minis-studio-worker.service`
- macOS: LaunchAgent plist at
  `~/Library/LaunchAgents/gg.minis.studio-worker.plist`
- Windows: `schtasks /Create` XML template (`%APPDATA%\\minis-studio-worker\\minis-studio-worker.task.xml`)
  \u2014 written but **not registered**, since CreateTrigger needs
  the operator to confirm.

`uninstall-service` removes them.  Tested in
[`tests/runtime_helpers.rs`](../../tests/runtime_helpers.rs)
under an `XDG_CONFIG_HOME` override.

### "Run in tray on login" (UI mode)

[`src/autostart.rs`](../../src/autostart.rs) (always compiled, like
`service.rs`; the desktop UI's Config tab is the only caller).  Toggle
in the Config tab's "Background mode" group.  Each enable/disable emits
a structured `tracing` event on target `studio_worker::autostart`.
Writes:

- Linux: `~/.config/autostart/studio-worker-ui.desktop`
- macOS: `~/Library/LaunchAgents/gg.minis.studio-worker-ui.plist`
- Windows: a marker file under `%LOCALAPPDATA%` (registry-key path
  is the proper Windows mechanism but deferred to a follow-up).

The two mechanisms coexist; they install different artefacts.  Use
the service for headless rigs, the autostart toggle for desktop
contributors.

---

## Failure modes + reconnect policy

| Failure | Detection | Behaviour |
|---|---|---|
| `register-request` HTTP 5xx | `auto_register::tick` | Stay Pristine, log warn, retry on next tick |
| `register-request` rate-limited (429) | studio binding | Same as 5xx; the 30s poll cadence already respects backoff implicitly |
| `register-requests/:id` 404 | poll response | Drop stale `request_id` + secret from config, recreate on next tick |
| `register-requests/:id` 401 | poll response | Same as 404; the worker's secret doesn't match the row \u2014 only happens if config was tampered |
| WS connect refused / TLS error | `WsClientError::Transport` | Back off + reconnect, up to `ws_reconnect_attempts` |
| WS close code `4001 AuthFailed` | session loop | Stop reconnecting; user must `register --reset` |
| WS close code `4002 DuplicateWorker` | session loop | Stop reconnecting (another instance is connected with the same id) |
| WS close code `4003 WorkerDeleted` | session loop | Stop; the studio operator deleted us |
| WS protocol violation | session loop | Server sends `Error { code: ProtocolViolation }` then closes |
| Engine `dispatch` returns `UnsupportedKind` | runtime job-runner | `Fail { retryable: false }` \u2014 server moves the job to terminal failed |
| Engine `dispatch` returns generic `Err` | runtime job-runner | `Fail { retryable: true }` \u2014 server requeues |
| `complete` multipart 5xx | runtime job-runner | `Fail` so the server can retry |
| Auto-update download / install failure | `update::apply` | Log + leave worker running on the old version; try again next interval |
| Auto-update `execvp` failure (unix) | `update::restart_self` | Should never happen; if it does, exit 0 and let systemd restart |
| Offer without `ModelSource` to sdcpp engine | engine `dispatch_with_source` | `Fail { retryable: false }` with "requires a ModelSource on the offer" |
| Model file download fails | sdcpp `ensure_files` | `Fail { retryable: true }`; the next claim of the same job retries the download |
| `sd-cli` non-zero exit | sdcpp `dispatch_image` | `Fail { retryable: true }` with the last stderr line included so operators can spot OOM / driver issues quickly |
| `sd-cli` binary missing | `SdCppEngine::try_new` | Engine doesn't register itself; the worker still boots with synthetic as the only available image engine and the studio sees a worker that advertises `image` kind only |
| rustls 0.23+ CryptoProvider missing | first WSS handshake | Process panics on `crypto/mod.rs:249`.  Fix is `rustls::crypto::ring::default_provider().install_default()` once at startup; see [`src/main.rs`](../../src/main.rs) |
| `worker_id` / `auth_token` missing at WS connect | `has_credentials` check | Session loop waits (polling cfg every 1s) instead of fatal-bailing.  Lets the UI's parallel auto-register + WS flow work. |
| Hello-without-Welcome race | `wait_for_welcome` gate | Block heartbeat + log-shipper spawn until the studio's Welcome reply arrives, so `tokio::interval()`'s t=0 first tick doesn't ship a heartbeat into an unauthenticated session |

All worker-side failures emit a structured `tracing::warn!` or
`error!` event before they're handled, so logs ship and Sentry
captures them.

---

## Security model

- **No shared secret distributed.**  Every worker generates its own
  256-bit `registration_secret`; only the SHA-256 hash leaves the
  box.  The studio operator gates each registration manually.
- **Per-worker auth tokens** minted server-side on approval (32 bytes
  hex, stored hashed in `studioWorkers`).  Worker presents the raw
  token in WS Hello + as Bearer on the multipart complete route.
- **No tokens logged**: `tracing` events at `studio_worker::config`
  redact `auth_token` and `registration_secret` (regression-tested
  in [`tests/config_tracing.rs`](../../tests/config_tracing.rs)).
- **Rate limited at the edge**: the studio binds
  `REGISTER_REQUEST_RATE_LIMIT` (Cloudflare native rate limiter,
  10 req / 60s / source IP) to `POST /workers/register-request`.
- **Idempotent register-request dedup**: same `installId` from the
  same source IP returns the existing `requestId` instead of piling
  up rows.
- **Approve / reject is admin-only**: studio's Firebase auth +
  allowlist guards the dashboard.
- **Worker side reads `/dev/urandom` directly** on unix for the
  install_id + secret \u2014 no `rand` dep, smaller surface area.
- **Auto-update binary swap** runs the cargo-dist installer the same
  way the user did on first install \u2014 same HTTPS + checksum
  verification (cargo-dist's own).

---

## Studio side (minigames repo)

This repo is the worker.  The other half lives in
`webbertakken/minigames` under
`apps/studio/src/worker/modules/graphics`:

| Path | Role |
|---|---|
| `routes/workers.ts` | Mounts `workerAdminRoutes` (Firebase-auth'd dashboard) + `workerAgentRoutes` (unauth'd register-request + secret-auth'd poll) |
| `WorkerConnections/` | Cloudflare Durable Object that owns every connected worker's WS session.  Receives offers from the queue, fans them out by capability fit |
| `routes/queue.ts` | Job CRUD + the "promote pending to queued" admin flow |
| `workerAuth.ts` | `hashToken` / `mintToken` / `requireRegistrationSecret` / `requireWorkerToken` middlewares |
| `apps/studio/migrations/graphics/0013_worker_registration_requests.sql` | D1 schema for the pending queue |
| `apps/studio/src/client/modules/graphics/components/PendingWorkersPanel.tsx` | The dashboard panel where the operator clicks Approve / Reject |

Wire-format contract is mirrored on both sides; the TypeScript
declarations in `apps/studio/src/shared/types/{worker,workerWs}.ts`
are the source of truth, and [`src/types.rs`](../../src/types.rs) +
[`src/ws/types.rs`](../../src/ws/types.rs) are hand-written
mirrors with regression tests in
[`tests/ws_wire.rs`](../../tests/ws_wire.rs).
