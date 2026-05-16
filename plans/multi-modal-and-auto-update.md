# Multi-modal tasks + auto-update

Generalise the worker so it can claim and serve **Image**, **LLM**, **Audio
STT**, **Audio TTS**, and **Video** jobs from a single self-contained binary,
plus add a silent auto-update flow that only fires when no job is running.

## Goals (must-haves before "done")

- One binary that can register itself as capable of any subset of the five
  task kinds, advertise per-kind supported models, and dispatch jobs to the
  right engine.
- `synthetic` engine produces **real bytes** for every kind (real WEBP,
  real WAV, real MP4, real JSON-encoded LLM response).  Required: tests
  must round-trip bytes through a real decoder for each modality.
- Wire format is backwards-compatible with the existing studio API's
  image-only `JobClaim` (when `task` is absent we treat it as image).
- Real high-perf engines live behind cargo features so the default
  build stays small + CI stays fast.  Feature gates:
  - `llama` → llama-cpp-2 for LLM tasks.
  - `whisper` → whisper-rs for STT tasks.
  - `image-candle` → candle-transformers SD for image tasks.
  - `tts-piper` → Piper for TTS (scaffolded, may stub for v1).
  - `video-ffmpeg` → ffmpeg-next for video processing (scaffolded).
- Auto-update: when the worker is idle, check the GitHub releases of
  this repo (or a configurable feed) for a newer semver; download the
  install script (cargo-dist `install.sh` / `install.ps1`) and re-exec.
  Frequency configurable; skipped when a job is in flight.
- All existing tests stay green; new tests cover each modality and the
  auto-update flow against a mock release feed.

## Phase 1 — Type system

- [x] Add `TaskKind` enum (`image`, `llm`, `audio_stt`, `audio_tts`,
      `video`).
- [x] Add per-kind param structs (`ImageParams`, `LlmParams`,
      `AudioSttParams`, `AudioTtsParams`, `VideoParams`).
- [x] Add `Task` enum tagged by `kind`, with backward-compat default to
      Image when missing.
- [x] Extend `WorkerCapabilities` with `task_kinds: Vec<TaskKind>` +
      `supported_models_per_kind: BTreeMap<TaskKind, Vec<String>>`.
- [x] Extend `JobClaim` with optional `task: Option<Task>`.  When absent
      the worker treats it as `Task::Image` constructed from the
      legacy top-level fields.

## Phase 2 — Engine refactor

- [x] Replace `Engine` trait with `Engine { fn name(); fn capabilities();
      fn dispatch(task) -> TaskResult; }`.
- [x] `TaskResult` is a tagged enum: `Image(bytes, ext)`,
      `Llm(JsonValue)`, `AudioStt(JsonValue)`, `AudioTts(bytes, ext)`,
      `Video(bytes, ext)`.
- [x] `synthetic` engine implements all 5 kinds (see Phase 3).
- [x] `gradio` engine implements `Image` only; for other kinds it returns
      an `UnsupportedKind` error (worker rejects those claims).

## Phase 3 — Synthetic outputs for all kinds

- [x] Image: keep the existing procedural WEBP/PNG.
- [x] LLM: deterministic JSON `{ role: "assistant", content: "[synthetic]
      <hash(prompt)>" }`.
- [x] STT: deterministic JSON `{ text: "[synthetic transcript of <hash>]" }`.
- [x] TTS: real WAV (RIFF/WAVE/fmt /data) — a 1 s sine wave whose
      frequency depends on hash(text).  Use `hound`.
- [x] Video: real MP4 — 1 s, 256×256, single colour from hash(prompt),
      H.264 not required — use `mp4` crate's plain track of raw frames,
      or fall back to a minimal animated PNG (`apng`) if MP4 muxer is
      heavy.  Decision: MP4 with PNG-in-stream frames as a media stream
      so a real decoder accepts it; if that's painful, use AVIF
      sequence or animated WebP.

## Phase 4 — Worker dispatch

- [x] `JobClaim` → `Task` resolution (with legacy fallback).
- [x] Worker rejects jobs whose `kind` isn't in its `task_kinds` capability
      *before* CAS, via the existing claim filter on the API side.  In
      the meantime the worker handles "unsupported kind" defensively by
      failing the job with `retryable=false`.
- [x] HTTP `complete` becomes kind-aware: multipart for binary kinds,
      JSON for `Llm` / `AudioStt`.

## Phase 5 — Auto-update

- [x] Add `self_update` dependency (or hand-rolled equivalent against
      GitHub API + cargo-dist install scripts).
- [x] Config additions: `auto_update_enabled`, `auto_update_interval_secs`
      (default 1800), `auto_update_feed` (defaults to the GitHub repo's
      `releases/latest`).
- [x] Dedicated tokio task: when idle (no `currentJobId`), every
      `auto_update_interval_secs`, check the feed; if a higher semver is
      available, download the appropriate platform installer to a temp
      dir, run it (it overwrites our binary), then `exec()` ourselves
      so the new code takes over.  On Windows where exec-self isn't a
      thing, spawn a successor and exit cleanly.
- [x] CLI: `check-update` (one-shot) and a `--no-auto-update` flag on
      `run`.
- [x] Pause auto-update for the duration of a job: gated by a shared
      `AtomicBool` set when claim succeeds + cleared on
      complete/fail.

## Phase 6 — Real engines (feature flags)

Each feature is **off** by default to keep the standard build small +
CI fast.  CI matrix adds one job per feature for build-only verification.

- [ ] `llama` — wrap `llama-cpp-2`.  Engine declares supported models
      based on what GGUFs are present in `$STUDIO_WORKER_MODELS/llama/`.
      On first use of a missing model, fetch from configured HF mirror.
      **Deferred to a follow-up iteration**; the wire format + engine
      trait are ready, only the trait impl is missing.
- [ ] `whisper` — wrap `whisper-rs`.  Same model-cache pattern.
      **Deferred**.
- [ ] `image-candle` — wrap `candle-transformers` SD pipeline.  **Deferred**.
- [ ] `tts-piper` — scaffold trait impl; real Piper integration
      deferred unless time permits.  **Deferred**.
- [ ] `video-ffmpeg` — scaffold; real video work deferred.  **Deferred**.

## Phase 7 — Testing

- [x] Unit tests for synthetic outputs of every kind (decode roundtrip).
- [x] Wiremock integration tests: claim each kind, generate, complete.
- [x] Auto-update unit tests with a mock release feed (file:// URL).
- [x] CI: default build + tests; matrix job that does
      `cargo build --features llama,whisper,image-candle` build-only to
      catch regressions without needing models.

## Phase 8 — Docs

- [x] README: new Tasks section, list of modalities, how to enable perf
      features, auto-update behaviour, model cache location.
- [x] AGENTS.md: update modules + feature flags + auto-update notes.

## Surfaced trade-offs

- Real engines are **build-only** in CI by default — running them needs
  ~hundreds of MB to GBs of model weights, which we don't ship.  The
  synthetic engine is the contract test.
- Video gen quality in pure C++/Rust still lags PyTorch.  The
  `video-ffmpeg` scaffold lets us do *processing* (frame interp,
  encoding, image-to-video chain) but **video generation** is
  intentionally deferred to a follow-up iteration.
- Music gen has no good self-contained option yet.  Skipped for v1.
- Cross-compile with `llama-cpp-2` on Windows MSVC may need extra
  cmake/clang setup in `build.yml`; we'll guard those targets with the
  CI matrix.
