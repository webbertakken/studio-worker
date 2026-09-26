# Model host

The worker becomes a multi-purpose model host: it keeps chosen models loaded for local
clients, loads and unloads them on request, streams speech-to-text, and shows every job with
its log (and a thumbnail for images) in the tray UI. Studio jobs keep working as today.

Design: [`docs/runtime/model-lifecycle.md`](../docs/runtime/model-lifecycle.md).

## Findings

- The in-process LLM engine is CPU-only (`llama-cpp-2` without `cuda`), renders a made-up chat
  template and uses a fixed 2048-token context.
- `llama-cpp-2 =0.1.157` + `cuda` runs Qwen3.5 GGUFs; rendering the GGUF's own template with
  minijinja (pycompat) honours `enable_thinking = false`.
- `parakeet-rs` 0.3.8 streams in-process on CUDA (Nemotron streaming, Parakeet EOU); it needs
  `ort ^2.0.0-rc.13`, which the ONNX engine compiles against unchanged.
- The job gate allows one job worker-wide; a resident streaming session would starve it.
- The UI hosts the runtime today and dies when no display is available at login.

## Phase 1 - lifecycle

- [x] 1.1 Lifecycle state machine (pure): states, transitions, guards, exclusive groups.
- [x] 1.2 Residency store (`residency.json`) with load/save and tracing breadcrumbs.
- [x] 1.3 Admission: free-memory probe + safety margin, named refusal.
- [x] 1.4 Model host: owns loaded models and lanes, drives the state machine, restores residents.
- [ ] 1.5 Local API routes: state in `GET /models`, `GET /models/:id/state`, load, unload.
- [ ] 1.6 LLM engine on the host: CUDA, the model's own chat template, configurable context,
      real token counts; transient path unchanged for studio offers.
- [ ] 1.7 Docs: overview, flows, local API; screenshots where the UI changes.

## Phase 2 - streaming speech-to-text

- [ ] 2.1 `stt-stream` engine over `parakeet-rs` (Nemotron + EOU) as loadable models.
- [ ] 2.2 Stream tokens: `POST /stream-tokens` mints a short-lived token for one model.
- [ ] 2.3 LAN streaming listener (WebSocket, PCM16 16 kHz mono in, partial/final text out),
      accepting stream tokens only.
- [ ] 2.4 Catalogue seeds for both models with checksummed downloads.

## Phase 3 - daemon and tray UI

- [ ] 3.1 Event stream on the local API: model states, jobs, per-job log lines.
- [ ] 3.2 Tray UI as a client of the daemon; waits for the display; always on at login.
- [ ] 3.3 Per-job log capture for local and studio jobs.
- [ ] 3.4 Jobs tab: per-job log, image thumbnail inline, model state panel.

## Phase 4 - release

- [ ] 4.1 PR, release through release-please, install, verify `/healthz` version.

## Fold-back

- [ ] F.1 Docs match what shipped; symbols and links verified.
