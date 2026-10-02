# studio-worker: architecture overview

`studio-worker` is a single self-contained Rust binary that pulls
**image**, **LLM**, **audio (STT/TTS)**, and **video** generation
jobs from the [minis.gg studio](https://studio.minis.gg), runs them
locally, and posts the results back.  One binary runs as two
processes: a headless **daemon** (`studio-worker run`) that hosts every
model and job, and a **tray UI** (`studio-worker ui`) that is a client of
the daemon over the local API (see [daemon and tray UI](../runtime/daemon-and-tray.md)).
No shared secrets, no out-of-band setup: an operator clicks Approve in
the studio dashboard once per machine, and the worker takes over from
there.

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
9. [Tray UI](#tray-ui)
10. [Auto-update](#auto-update)
11. [Observability](#observability)
12. [Install / autostart](#install--autostart)
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
| `POST /workers/:id/jobs/:jobId/complete` (multipart) | Per finished job with binary output | Image / audio / video bytes → R2 |

Everything else (heartbeat ack, accept, fail, log shipping, etc.) is
WebSocket frames — the legacy `/heartbeat`, `/claim`,
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
runtime.rs::run       (the daemon; `ui::run` is the tray UI client)
   |
   +--> 1. config::load              (config.rs)
   +--> 2. daemon_lock::acquire      (one daemon per config dir; a second exits 0)
   +--> 3. spawn_local_api           (local API + model host + stream listener, with DaemonControl)
   +--> 4. serve_studio
              |
              +--> ensure_registered  (auto_register::tick in a loop until Approved;
              |                        a rejection waits for a reset, local serving continues)
              +--> run_loops          (spawns the WS session + auto-updater)
                     |
                     +--> ws::session::spawn_ws_session  (heartbeats, claim, complete, fail, logs)
                     +--> runtime::spawn_auto_updater    (release-feed poll + re-exec)

ui::run (tray UI)
   |
   +--> single_instance::acquire     (one tray UI per config dir; a second hands over and exits 0)
   +--> autostart::ensure            (login entry, always)
   +--> daemon_link::Poller          (1 s poll into a Replica; starts the daemon when absent)
   +--> eframe window + tray         (restarts itself in place while no display is usable)
```

The CLI surface from [`src/cli.rs`](../../src/cli.rs):

| Subcommand | What it does |
|---|---|
| `setup` | Finish an install: the tray UI's login entry, start the tray UI detached, print guidance (the installers run it) |
| `run` (hidden) | The daemon: local API + model host, ensure registered, then the WS session + auto-updater.  Started by the tray UI; not an install method |
| `ui` (feature `ui`) | The tray UI: egui window + tray + notifications, a client of the daemon (starts one when absent) |
| `register` | Persist api-base-url / clear state (`--reset`).  **No HTTP** — the next `run`/`ui` actually auto-registers |
| `status` | Print config path, registration state, threshold, auto-update toggle |
| `set-threshold <gb>` | Update `vram_threshold_gb` |
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
├── net.rs            Transport-level guards for every download.
├── secrets.rs        Entropy for locally-minted credentials (local API token).
├── catalog.rs        Local model catalogue (models.json); studio models mirror into it.
├── local.rs          Local jobs without the studio: transient dispatch + chat on a lane.
├── local_api.rs      Loopback HTTP API (bearer token): generation, catalogue, lifecycle,
│                     daemon control (`/daemon/*`), job logs and thumbnails.
├── control.rs        DaemonControl: status snapshot, pause, config update, reset, shutdown.
├── daemon_api.rs     Wire types of the daemon-control routes (daemon + tray UI).
├── daemon_client.rs  Blocking client of the local API, found via the discovery file.
├── daemon_link.rs    The tray UI's poller, replica, daemon starter and actions.
├── daemon_lock.rs    One daemon per config directory (`daemon.lock`).
├── job_run.rs        One job's bookkeeping: running list, span, thumbnail, ring.
├── job_log.rs        Per-job log capture + the worker log ring (tracing layers).
├── thumbnail.rs      PNG thumbnails of image jobs, bounded ring.
├── job_gate.rs       One-transient-job-at-a-time reservation gate.
├── lifecycle.rs      Per-model state machine: unloaded/loading/loaded/unloading/failed.
├── host.rs           Model host: loaded models, lanes, residency, admission, swaps.
├── residency.rs      Persisted resident set (residency.json).
├── admission.rs      Free-device-memory probe + admission with a safety margin.
├── loaders.rs        In-process loaders per engine, behind the host's ModelRuntime.
├── stt_stream/       Streaming speech-to-text served on the LAN.
│   ├── session.rs    The /transcribe protocol over any streaming transcriber (pure).
│   ├── vad.rs        Energy voice-activity detection (hands-free finalise).
│   ├── tokens.rs     Short-lived stream tokens (stored hashed).
│   └── server.rs     LAN WebSocket listener; one session per loaded model's lane.
├── setup.rs          `setup`: the tray UI's login entry, start the tray UI, print guidance.
├── legacy_service.rs Removes the headless service older versions installed (tray UI start).
├── exe_watch.rs      Notices a replaced launch path so the tray UI restarts on the new binary.
├── log_trim.rs       Copy-truncate for daemon.log / ui.log (10 MiB + 3 copies).
├── autostart.rs      Cross-OS tray-UI login entry, always installed by `ui::run` (logged).
├── update.rs         GitHub release feed poll + installer script download + re-exec on success;
│                     keeps the build variant (CPU or CUDA).
├── variant.rs        The build variant (`cpu` or `cuda`) and the release target of this binary.
├── telemetry.rs      Sentry init (opt-in via SENTRY_DSN env var) + tracing-subscriber layer.
├── test_support.rs   #[doc(hidden)] tracing capture + host doubles for tests.
│
├── engine/           Pluggable inference backends.
│   ├── mod.rs        Engine trait + dispatch / dispatch_with_source.  Always-on SyntheticEngine.
│   ├── multi.rs      MultiEngine; routes strictly by ModelSource.engine (no fallback).
│   ├── sdcpp.rs      Real image inference via stable-diffusion.cpp subprocess.
│   ├── llama.rs      (feature `llama`, `cuda` for GPU) llama-cpp-2: transient jobs +
│   │                 resident LoadedLlm, both through one `complete`.
│   ├── llama_subprocess.rs  Windows LLM via a llama-cli subprocess.
│   ├── llm_core.rs   Engine-free LLM rules: context budget, BOS, reasoning split, response.
│   ├── chat_template.rs  The model's own Jinja chat template (minijinja + pycompat).
│   ├── download.rs   Shared model-file provisioning (cache, size + sha256 checks).
│   ├── sd_provision.rs  Auto-provisioned sd-cli binary + Vulkan preflight.
│   ├── onnx.rs       (feature `image-onnx`) ONNX Runtime image engine (LaMa).
│   ├── onnx_provision.rs  Shared ONNX Runtime, provisioned at runtime (CPU or CUDA flavour).
│   ├── parakeet.rs   (feature `stt-stream`) streaming speech models (Nemotron, Parakeet EOU).
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
└── ui/               (feature `ui`) The tray UI, a client of the daemon.
    ├── mod.rs        ui::run: UI lock, login entry, poller + raise threads, eframe;
    │                 display wait (restart in place).  Tray install (Linux ksni on tokio).
    ├── single_instance.rs  One tray UI per config dir (ui.lock); a second launch
    │                 leaves ui.raise for the running one and exits.
    ├── app.rs        eframe App impl: chrome + page dispatch, theme, hide-to-tray, quit.
    ├── chrome.rs     Navigation rail, pulse header, status bar; Ctrl+1…5.
    ├── pulse.rs      What the header says: activity, daemon, studio, GPU memory (pure).
    ├── theme.rs      Dark + light palettes held to WCAG AA by tests; egui visuals; glow.
    ├── prefs.rs      Window preferences (theme, reduce motion, notifications) in ui.toml.
    ├── widgets.rs    Cards, pills, dots, buttons, fact rows, copy buttons, meters.
    ├── icons.rs      Line icons painted from geometry (rail, job kinds).
    ├── log_view.rs   Shared log view: monospace, levels coloured, wrapping, copyable.
    ├── format.rs     Durations, ages, day labels, clock times.
    ├── actions.rs    Operator actions to the daemon off the UI thread + feedback.
    ├── page.rs       Page enum + STUDIO_WORKER_UI_PAGE env override for screenshots.
    ├── pages/
    │   ├── jobs.rs   Running slot + history by day, filters; detail pane, larger image.
    │   ├── models.rs GPU memory summary; models grouped by loader; Load / Unload / Retry.
    │   ├── worker.rs State, registration, studio, hardware, local API, about, update check.
    │   ├── logs.rs   Level filter + search + follow + copy, windowed.
    │   └── config.rs Operator-editable fields (Save goes to the daemon) + window prefs.
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
HTTP multipart upload — R2 doesn't fit cleanly into WS frames.
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

- **`synthetic`** — deterministic real bytes for every kind,
  keyed by SHA-256 of the prompt.  Real WEBP, real WAV, real animated
  WEBP, real OpenAI-shaped JSON.  No GPU, no model downloads, ~0ms
  per task.  Powers CI + smoke-tests.  Advertises only `synthetic*`
  model names so it never claims a real-model job (it would happily
  upload placeholder bytes for a real manifest, which is destructive
  on a live queue).
- **`sdcpp`** — real image inference via `stable-diffusion.cpp` as a
  subprocess.  Reads the `ModelSource` off every offer, downloads
  any missing files into `cfg.models_root`, invokes `sd-cli` with
  the right `--diffusion-model` / `--llm` / `--vae` flags + CLI
  defaults from the source.  Image kind only today.  Deep dive in
  [`docs/engines/sdcpp.md`](../engines/sdcpp.md).

The legacy `gradio` engine is gone (operators run a Gradio app via
an external service if they need it).  Feature-gated heavyweights
(`llama`, `whisper`, `image-candle`, `video`, `tts`) still drop in
via the same trait when their cargo features are enabled — see
[`plans/real-engines.md`](../../plans/real-engines.md).

---

## Model host

Besides one-off jobs, the worker keeps chosen models **loaded** for local
clients and unloads them on request.  [`src/host.rs`](../../src/host.rs) owns
that: each catalogue model has a lifecycle state, loaded models serve on their
own lane (one request at a time, next to the transient-job gate), residency is
persisted so loaded models come back after a restart, and admission refuses a
load that would not fit in free device memory.  Models in an exclusive group
swap.  Full design: [model lifecycle](../runtime/model-lifecycle.md); API:
[local API](../local-api.md#model-lifecycle).

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
       `observers.current_job` (the heartbeat reports it) and starts a
       `JobRun` (running list, job log span, thumbnail).
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
     - Pushes CurrentJob → RecentJob in the observers ring (the UI's
       pulse header and Jobs page surface this).

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

The runtime tracks its observable state in
[`runtime::WorkerObservers`](../../src/runtime.rs):

- `current_job: Option<CurrentJob>` — the studio job in flight (heartbeat)
- `active_jobs: Vec<CurrentJob>` — every running job, whatever its source
- `recent_jobs` / `local_jobs: VecDeque<RecentJob>` (cap 50 each, newest-first)
- `thumbnails` — PNG thumbnails of recent image jobs
- `last_heartbeat: Option<HeartbeatStatus>` — written after every
  WS heartbeat ack / failure
- `recent_logs` — the worker log ring the Logs page shows

The daemon serves them to the tray UI as `GET /daemon/status`; the UI
never reads them in-process.

---

## Config + persisted state

[`src/config.rs`](../../src/config.rs) defines the persisted
`Config` struct.

**File location** (via the `directories` crate):

- Linux / macOS: `~/.config/minis-studio-worker/config.toml`
- Windows: `%APPDATA%\minis-studio-worker\config.toml`

**Operator-facing fields** (exposed in the UI's Config page):

| Field | Default | Purpose |
|---|---|---|
| `api_base_url` | `https://studio.minis.gg/` | Studio API root |
| `vram_threshold_gb` | `12.0` | Max VRAM per claim |
| `start_minimised` | `true` | Tray UI window starts minimised |
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
`studio_worker::config` target with the resolved path — makes
"why is the worker reading the wrong config" trivially debuggable from
`journalctl`.  `auth_token` and `registration_secret` are
**deliberately omitted** from these events so logs ship off-box
without leaking credentials.

Coverage regression contract in
[`tests/config_tracing.rs`](../../tests/config_tracing.rs).

---

## Tray UI

Built behind the `ui` cargo feature (on by default); brings in `egui` +
`eframe` + `notify-rust`, plus the platform tray backend: `tray-icon` on
macOS / Windows, `ksni` (pure-Rust StatusNotifierItem) on Linux, so the
build needs no GTK.

The tray UI is a **client of the daemon**: it never runs a job and never
talks to the studio.  A poller ([`daemon_link.rs`](../../src/daemon_link.rs))
reads `GET /daemon/status`, new worker log entries, the model list, the
selected job's log and missing thumbnails once a second into a `Replica`
the pages render; actions go back over the local API.  When the daemon does
not answer, the replica is emptied (no stale data), a "daemon not
reachable" card replaces the pages, and when no daemon holds the daemon
lock the UI starts one.  One tray UI runs per config directory
(`ui.lock`); a second launch asks the running one to show its window and
exits.  Full design:
[daemon and tray UI](../runtime/daemon-and-tray.md).

### Window structure

A navigation rail on the left (`Ctrl+1` … `Ctrl+5`), a pulse header on top
(activity with the running job and its elapsed time, daemon link, studio
connection, GPU memory held by loaded models, **Pause / Resume**) and a
status bar at the bottom (the daemon's version and URL, the result of the
last action).  None of them change size with the state.  The window opens
on Jobs.

| Page | What it shows |
|---|---|
| **Jobs** | A running slot that is always reserved (one glowing card per running job, or an empty card), then every finished job (studio and local) grouped by day with All / Studio / Local filters.  Cards have a fixed height: a thumbnail or kind glyph, the prompt, kind and model, source, time and duration, an outcome pill.  The detail pane shows the selected job: image (click for a larger view), facts, whole prompt, failure reason, and its log (monospace, levels coloured, wrapping, selectable, Copy log).  `↑` / `↓` move the selection. |
| **Models** | GPU memory held (one bar segment per loaded model), then models kept in memory (in-process loader) and models loaded per job, in catalogue order: state, name and id, kind, engine, estimate, resident pin, exclusive group, since, error; one action (Load / Unload / Retry). |
| **Worker** | State with **Pause / Resume**; registration (worker id, or Initialising / Pending with request id + copy / Rejected with reason + **Reset registration**); studio connection, last heartbeat, API URL; GPU runtime, VRAM total / threshold, memory held; local API URL; tray UI and daemon versions, Sentry release, config path, manual "Check for updates". |
| **Logs** | Everything the daemon logs at info and up (level filter, search, follow, copy), from the daemon's worker log ring. |
| **Config** | The operator-editable subset of `Config` in cards (Connection / Worker / Auto-update / Models / Start-up); Save sends it to the daemon (`PUT /daemon/config`), which validates, saves and applies it.  This window: theme (dark by default, light, follow system), reduce motion, notifications, stored at once in `<config dir>/ui.toml`. |

Both themes meet WCAG 2.2 AA contrast (tests in
[`src/ui/theme.rs`](../../src/ui/theme.rs)); errors show where they
happen, never as toasts.  Screenshots in
[`docs/screenshots/`](../screenshots/).

### Tray icon

Three coloured variants:

- **Idle** — green; daemon reachable, nothing running, heartbeat fresh + ok
- **Busy** — amber; a job runs (any source)
- **Disconnected** — red; daemon not reachable, or heartbeat stale
  (> 3 × interval), missing, or failed

Menu: **Open Window** / **Pause / Resume** / **Quit**.  Pause / Resume goes
to the daemon; Quit stops the daemon (`POST /daemon/shutdown`) and closes
the UI.  Closing the window hides it to the tray; the daemon keeps running.

**Per-OS backends** ([`src/ui/tray_host.rs`](../../src/ui/tray_host.rs)):
Linux uses **ksni** (pure-Rust StatusNotifierItem over zbus) so the
build needs no GTK; the tray runs on the tokio runtime.  macOS / Windows
use **tray-icon** (native APIs), built on the eframe main thread, with
menu events arriving through muda's global `MenuEvent::receiver()`
channel.  Either backend is best-effort — the window UI works without a
tray.

### Display wait

When the window cannot open (no usable display yet, e.g. started at login
before the X session accepts clients), the UI logs `op="display_wait"`
with the attempt and the error, waits (2 s doubling to 60 s) and restarts
itself in place.  The windowing library allows one event loop per process
and caches a failed display connection, so the retry needs a fresh
process.

### Notifications

OS-native desktop notifications via `notify-rust`, gated behind a
`Notifier` trait so tests inject a `CapturingNotifier` and assert
what would have been shown.  Both completion and failure
notifications are off by default, opt-in per event on the Config
page (stored with the window's preferences in `ui.toml`).

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
   - Pick the build variant (`op="variant"`): a CUDA build stays CUDA, falling back to CPU
     with a warning only when the release ships no CUDA archive for its target; a CPU build
     moves to CUDA when the release ships one and the NVIDIA driver (`libcuda.so.1`) loads.  See
     [release: CUDA variant](../operations/release.md#cuda-variant).
   - Download the per-platform cargo-dist installer script and verify it against its
     `<installer>.sha256` sidecar.  An update to CUDA refuses an installer without the variant
     switch, so it can never install the CPU build in its place.
   - On Windows only: **park** the running exe first (rename to
     `<exe>.old` — NTFS allows renaming a running binary but not
     overwriting it, so without this the installer's `Copy-Item`
     fails with "file in use" every time).  After the installer
     runs, confirm a new binary landed at the original path; roll
     the rename back otherwise.  The parked file is removed on the
     next start (`update::cleanup_parked_artifact`).
   - Run the installer with `STUDIO_WORKER_VARIANT=<variant>` (overwrites the binary in
     place).
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
trait — they're excluded from the 90% coverage gate
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
- **Tray UI Logs page**: the daemon's worker log ring (its own info-and-up
  events plus the studio-session breadcrumbs), served by
  `GET /daemon/logs?after=<seq>`.
- **Per-job logs**: every job runs in a `job` span; the events inside it
  (engine downloads, loads, generation) are kept per job and served by
  `GET /jobs/:id/log`.
- **Sentry (opt-in)**: set `SENTRY_DSN` (and optionally
  `SENTRY_ENVIRONMENT`) before launch.  Captures panics, forwards
  `tracing::error!` events, attaches preceding `warn!` events as
  breadcrumbs.  Tags with `release = studio-worker@<version>` and
  `server_name = <hostname>`.  Performance tracing intentionally off.

---

## Install / autostart

Installed, the worker runs as the tray UI only; nothing installs the daemon on its own
([daemon and tray UI](../runtime/daemon-and-tray.md#install)).

### `studio-worker setup` (the installers run it)

[`src/setup.rs`](../../src/setup.rs).  Installs the tray UI's login entry, starts the
tray UI detached (output to `<config dir>/ui.log`) and prints the approval guidance.

### Legacy headless services

[`src/legacy_service.rs`](../../src/legacy_service.rs).  Older versions wrote a systemd
user unit, a LaunchAgent or a scheduled task that ran `studio-worker run`.  The tray UI
deregisters and deletes it at start, without stopping a running legacy daemon.

### Tray UI login entry (always)

[`src/autostart.rs`](../../src/autostart.rs).  Every `ui::run` makes sure
the tray UI starts at login from the current executable (rewriting a stale
entry); there is no setting to turn it off.  Each write or no-op emits a
structured `tracing` event on target `studio_worker::autostart`.  Writes:

- Linux: `~/.config/autostart/studio-worker-ui.desktop`
- macOS: `~/Library/LaunchAgents/gg.minis.studio-worker-ui.plist`
- Windows: an `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
  registry value `studio-worker-ui` = `"<exe>" ui` (via `winreg`).
  The standard per-user autostart mechanism: no console flash, no admin
  rights, no COM.

The two mechanisms coexist: the service runs the daemon before anyone
logs in; the tray UI starts at login and starts the daemon itself when
none runs.  The daemon lock keeps it to one daemon per config directory.

---

## Failure modes + reconnect policy

| Failure | Detection | Behaviour |
|---|---|---|
| `register-request` HTTP 5xx | `auto_register::tick` | Stay Pristine, log warn, retry on next tick |
| `register-request` rate-limited (429) | studio binding | Same as 5xx; the 30s poll cadence already respects backoff implicitly |
| `register-requests/:id` 404 | poll response | Drop stale `request_id` + secret from config, recreate on next tick |
| `register-requests/:id` 401 | poll response | Same as 404; the worker's secret doesn't match the row — only happens if config was tampered |
| WS connect refused / TLS error | `WsClientError::Transport` | Back off + reconnect, up to `ws_reconnect_attempts` |
| WS close code `4001 AuthFailed` | session loop | Stop reconnecting; user must `register --reset` |
| WS close code `4003 DuplicateWorker` | session loop | Stop reconnecting (another instance is connected with the same id) |
| WS close code `4004 WorkerDeleted` | session loop | Stop; the studio operator deleted us |
| WS protocol violation | session loop | Server sends `Error { code: ProtocolViolation }` then closes |
| Engine `dispatch` returns `UnsupportedKind` | runtime job-runner | `Fail { retryable: false }` — server moves the job to terminal failed |
| Engine `dispatch` returns generic `Err` | runtime job-runner | `Fail { retryable: true }` — server requeues |
| `complete` multipart 5xx | runtime job-runner | `Fail` so the server can retry |
| Auto-update download / install failure | `update::apply` | Log + leave worker running on the old version; try again next interval |
| Auto-update `execvp` failure (unix) | `update::restart_self` | Should never happen; if it does, exit 1; the tray UI starts a new daemon |
| Offer without `ModelSource` to sdcpp engine | engine `dispatch_with_source` | `Fail { retryable: false }` with "requires a ModelSource on the offer" |
| Model file download fails | sdcpp `ensure_files` | `Fail { retryable: true }`; the next claim of the same job retries the download |
| `sd-cli` non-zero exit | sdcpp `dispatch_image` | `Fail { retryable: true }` with the last stderr line included so operators can spot OOM / driver issues quickly |
| `sd-cli` binary missing | sdcpp `ensure_sd_cli` (first image job) | The engine always registers and advertises `image`; on the first image job it resolves `sd-cli` or auto-provisions the prebuilt into `cfg.models_root/bin`.  If no prebuilt exists for the target or the download fails, the job `Fail`s with the install remedy |
| Vulkan loader (`libvulkan.so.1` / `vulkan-1.dll`) missing | sdcpp dispatch preflight | `Fail { retryable: true }` with the exact remedy (install `libvulkan1` + a GPU driver) instead of a cryptic `sd-cli` crash.  macOS uses Metal, so no Vulkan loader is involved |
| rustls 0.23+ CryptoProvider missing | first WSS handshake | Process panics on `crypto/mod.rs:249`.  Fix is `rustls::crypto::ring::default_provider().install_default()` once at startup; see [`src/main.rs`](../../src/main.rs) |
| `worker_id` / `auth_token` missing at WS connect | `has_credentials` check | Session loop waits (polling cfg every 1s) instead of fatal-bailing. |
| Operator rejected the registration | `serve_studio` | The daemon keeps serving locally and waits; Reset registration (tray UI, `POST /daemon/registration/reset`) clears the state and asks again |
| A second daemon for the same config | `daemon_lock::acquire` | Logs `op="daemon_lock"` and exits 0 |
| Daemon not reachable from the tray UI | `daemon_link::Poller` | Replica emptied, "not reachable" view; starts a daemon when the lock is free (at most every 10 s) |
| No usable display for the tray UI | `ui::run` | `op="display_wait"`, backoff 2 s → 60 s, restart in place |
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
  install_id + secret — no `rand` dep, smaller surface area.
- **Auto-update binary swap** runs the cargo-dist installer the same
  way the user did on first install — same HTTPS + checksum
  verification (cargo-dist's own), after checking the installer itself
  against the release's `<installer>.sha256` sidecar.

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
