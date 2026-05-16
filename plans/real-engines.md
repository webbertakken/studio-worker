# Real high-performance engines

Replace each synthetic engine with a real, GPU-capable implementation.
Each goes behind its own cargo feature so the default build remains
small + tested in free CI.

## Acceptance per modality

- Compiles on Linux x86_64 (CI baseline) with the feature enabled.
- Has an integration test that runs **real** inference end-to-end with
  the **smallest possible model** and asserts the output is real,
  decodable bytes (image / WAV / chat-completion JSON).
- Downloaded models live in `$STUDIO_WORKER_MODELS` (or platform default
  cache) — cached between test runs.
- Skipped automatically when the model cache is empty AND
  `RUN_REAL_ENGINE_TESTS=1` isn't set — keeps the default `cargo test`
  fast and offline.

## Phase 1 — LLM (`llama` feature)

- [ ] Add `llama-cpp-2` dependency behind `llama` feature.
- [ ] Implement `LlamaEngine` with `Engine` trait.  Loads any GGUF from
      `<models>/llm/*.gguf`; declares supported models as the file
      stems.  Returns chat-completion JSON matching the synthetic shape.
- [ ] Wire into `engine::build()` when `engine = "llama"`.
- [ ] Integration test `tests/real_llama.rs` that downloads SmolLM-135M
      Q8 (~145 MB), runs a 1-token completion, asserts non-empty content.
- [ ] CI matrix: `build --features llama` (build-only).

## Phase 2 — STT (`whisper` feature)

- [ ] Add `whisper-rs` dependency behind `whisper` feature.
- [ ] Implement `WhisperEngine`.  Loads any GGML model from
      `<models>/stt/*.bin`.  Fetches `input_url` over HTTP, transcribes,
      returns Whisper-shape JSON.
- [ ] Integration test that downloads whisper-tiny.en (~40 MB), feeds
      it a real WAV (generated via synthetic TTS), asserts transcript
      is non-empty.

## Phase 3 — TTS (`piper` feature)

- [ ] Attempt `piper-rs` if mature enough; otherwise ship a self-contained
      Rust TTS that produces real intelligible WAV (e.g. RustyTTS or
      bundle the eSpeak NG static library).  Fall back to documented
      synthetic if neither is available.
- [ ] Integration test that emits a real WAV ≥ 0.5 s for "hello world",
      asserts it decodes via `hound`.

## Phase 4 — Image (`image-candle` feature)

- [ ] Add `candle-core`, `candle-transformers` (and `candle-nn`)
      dependencies behind `image-candle` feature.
- [ ] Implement `CandleImageEngine` running SD 1.5 at low resolution
      (256×256, 4 inference steps) to keep test wall-clock reasonable.
- [ ] Integration test downloads SD 1.5 weights (~1.7 GB safetensors)
      via the Hugging Face hub crate, generates one image, asserts it
      decodes as PNG with non-trivial entropy.
- [ ] Guarded by `RUN_REAL_ENGINE_TESTS=1` (model is too big for
      free-tier CI by default).

## Phase 5 — Video (`video` feature)

- [ ] Add `mp4` (container) + `image` (encoder) dependencies behind
      `video` feature.  Encode a real MP4 by chaining N synthetic image
      frames + a tiny MJPEG stream (no external H.264 encoder).
- [ ] Integration test produces a 2-second MP4 (5 fps), asserts the file
      parses as MP4 via the `mp4` crate (atoms valid + duration close to
      the requested seconds).

## Phase 6 — Engine routing

- [ ] `Config::engine` becomes a list, not a single string — operators
      can run multiple engines in one binary (e.g. `llama` for LLM +
      `image-candle` for images).
- [ ] `engine::build()` returns a `MultiEngine` that dispatches by
      `Task::kind()` to the first registered engine that claims support.

## Phase 7 — Verification + docs

- [ ] All real-engine tests green locally (with models cached).
- [ ] CI matrix builds all features individually.
- [ ] README updated with the per-feature build commands + model cache
      paths + first-run model download instructions.
- [ ] AGENTS.md updated with the new feature flags.
- [ ] Coverage gate adjusted (real-engine adapters are mostly thin FFI
      wrappers around the C++ libraries; keep the overall ≥ 90% gate by
      excluding the FFI surface from coverage measurement).
