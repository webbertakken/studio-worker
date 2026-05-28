# Real model loading, on-demand

Today the worker accepts any model name (synthetic engine advertises
the `"*"` wildcard) but always returns deterministic placeholder
bytes — it never actually runs the model the studio asked for. To
ship real images / LLM / STT / TTS / video, the worker needs to:

1. Detect the model the studio wants from the `Offer.model` string.
2. Map the model name to an engine + on-disk weights path
   (`<models_root>/<kind>/<model-name>`).
3. If the weights aren't present, download them (and surface
   progress in the UI's Status tab).
4. Load the engine on the right device (CUDA / MPS / CPU), reusing
   it for back-to-back jobs of the same (engine, model) pair.
5. Dispatch the task against the loaded engine.
6. Fall back to synthetic on download / load failure with a `Fail`
   frame carrying `retryable=false` so the studio doesn't loop.

## Tasks

### Engine registry

- [ ] Introduce `EngineRegistry` keyed by `(TaskKind, model_name)`
      that lazily builds + caches an `Arc<dyn Engine>`.
- [ ] When a job arrives, the dispatcher asks the registry for an
      engine.  Registry returns the cached one or builds a new one
      (eviction policy: most-recently-used, capped at N to bound
      VRAM).
- [ ] Tracing event per cache hit / miss / eviction.

### Model store

- [ ] `ModelStore` rooted at `cfg.models_root` (default `~/models`).
- [ ] Per-kind subdirectories: `image/`, `llm/`, `stt/`, `tts/`,
      `video/`.
- [ ] Layout: `{root}/{kind}/{model-name}/<files...>` so a single
      model can have multiple files (sharded safetensors, tokenizer,
      config).
- [ ] Resolver: given (kind, model_name), return a canonical
      directory; if not present, dispatch a download.

### Download path

- [ ] Per-format resolver:
    * `*.gguf` → HuggingFace via `hf-hub` (already a dep behind
      `image-candle`); pull the file directly.
    * SDXL / Flux folders → HuggingFace, snapshot the whole repo.
    * Custom URLs (e.g. for proprietary weights) → `reqwest`
      streaming download, SHA-256 verified.
- [ ] Show progress in the Status tab (`download_progress: Option<
      DownloadStatus>` on `WorkerObservers`).
- [ ] Resumable: store partial downloads under `<file>.partial`,
      verify size + hash on resume.
- [ ] Bounded concurrency (1 download at a time per worker).

### Engine wiring

For each kind we already have a feature gate:

| Kind | Feature | Backend | Today |
|---|---|---|---|
| Image | `image-candle` | candle-transformers SD | scaffolded |
| LLM | `llama` | llama-cpp-2 (gguf) | scaffolded |
| STT | `whisper` | whisper-rs | scaffolded |
| TTS | `tts` | piper-style | scaffolded |
| Video | `video` | gif/ffmpeg | scaffolded |

- [ ] Llama: load `*.gguf` from `<root>/llm/<name>/model.gguf`,
      first dispatch loads + caches, evict on registry overflow.
- [ ] Whisper: same pattern.
- [ ] Candle-image: pull SD/Flux snapshots from HF, load once,
      reuse.
- [ ] Each backend's `dispatch` runs on `spawn_blocking` so it
      doesn't starve the tokio runtime.

### Engine selection bias

The user's original requirement: "try to pick jobs of the same
type if an engine is already running. Also for the same engine try
to pull jobs intended for the same model".

Worker-side can't pick which offer arrives, but it can publish a
hint in heartbeat:

- [ ] `WorkerCapabilities` gets a new `currentlyLoaded: { engine,
      model } | null` field, populated from the engine registry.
- [ ] Studio's `findQueuedJobForWorker` consults this hint and
      orders candidates by `(currentlyLoaded.matches ?, updatedAt
      ASC)` so the same model wins ties.
- [ ] Cross-repo: studio change ships alongside the worker change.

### Config + UI

- [ ] `models_root` already exists; show used / available disk in
      the Status tab.
- [ ] Add a "loaded engines" panel showing `(kind, model, since,
      bytes_in_use)`.
- [ ] Add a "downloads" panel showing in-flight downloads with
      progress bar.

### Fallback policy

- [ ] If the studio offers a model the worker can't resolve (no
      backend feature compiled in, no download URL, etc.), the
      worker sends `Reject { reason: "model not available" }` so
      the studio offers it elsewhere instead of failing it terminal.
- [ ] If a download fails after N retries (exponential backoff),
      same path.

### Testing

- [ ] Unit-test the registry's cache hit / miss / evict logic.
- [ ] Integration test against `hf-hub` mock: download → cache →
      reuse.
- [ ] `tests/real_*.rs` already exists for the feature-gated paths;
      gate behind `RUN_REAL_ENGINE_TESTS=1` so default CI stays
      offline + fast.

### Rollout

1. Ship the EngineRegistry + ModelStore scaffolding without any
   real backend (still falls back to synthetic).  Validates the
   download path + UI plumbing.
2. Enable `llama` feature in the release builds; ship LLM real
   dispatch first (SmolLM-135M for smoke).
3. Enable `whisper` + a small whisper model for STT.
4. Enable `image-candle` once an acceptable Flux/SD model is
   licensable.
5. Video + TTS last.

### Open questions

- Memory budget: how do we know when to evict?  Hold off until we
  see real VRAM pressure?  Simple LRU with a configurable
  `max_loaded_engines: usize` first.
- Model name canonicalisation: studio sends
  `z-image-turbo-q4_k_m.gguf` — is that an HF model id or a local
  file name?  Need a convention.  Proposal: studio sends an HF
  model id when downloadable, the worker maps `*.gguf` files to
  `<repo>/<filename>` HF paths via a small registry the studio
  maintains.
