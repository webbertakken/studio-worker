# studio-worker

[![Checks](https://github.com/webbertakken/studio-worker/actions/workflows/checks.yml/badge.svg)](https://github.com/webbertakken/studio-worker/actions/workflows/checks.yml)
[![Build](https://github.com/webbertakken/studio-worker/actions/workflows/build.yml/badge.svg)](https://github.com/webbertakken/studio-worker/actions/workflows/build.yml)
[![Coverage](https://github.com/webbertakken/studio-worker/actions/workflows/coverage.yml/badge.svg)](https://github.com/webbertakken/studio-worker/actions/workflows/coverage.yml)

A single self-contained Rust binary that pulls **image**, **LLM**,
**audio (STT/TTS)**, and **video** jobs from the minis.gg studio API,
runs them locally, and posts the results back.

Replaces the previous push-based studio-proxy + cloudflared topology
with a pull-based pipeline: install the worker on any PC, register
once, and it will claim queued jobs whose VRAM estimate fits its
threshold.  The worker also **auto-updates itself** between jobs.

## Tasks supported

| Kind        | Wire `kind`   | Synthetic engine (default)                   | Real engine (planned)     |
| ----------- | ------------- | -------------------------------------------- | ------------------------- |
| Image       | `image`       | real WEBP / PNG via the `image` crate        | `image-candle` / `gradio` |
| LLM         | `llm`         | OpenAI-shape JSON (`chat.completion`)        | `llama` (llama.cpp)       |
| Audio STT   | `audio_stt`   | Whisper-shape JSON                           | `whisper` (whisper.cpp)   |
| Audio TTS   | `audio_tts`   | real WAV (sine wave keyed by hash(text))     | `tts-piper`               |
| Video       | `video`       | real WebP image (single-frame stand-in)      | `video-ffmpeg`            |

The synthetic engine is the default and exercises the full pipeline
end-to-end with no GPU, no model downloads, and ~0 ms per task — exactly
what the unattended CI suite uses.  Real high-performance backends
(llama.cpp, whisper.cpp, candle, Piper, ffmpeg) are wired in via
feature flags and are deferred to a follow-up iteration (the trait,
contract, and dispatch are already in place).

## Quick install

### Linux / macOS

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/webbertakken/studio-worker/releases/latest/download/studio-worker-installer.sh | sh
```

### Windows (PowerShell)

```powershell
irm https://github.com/webbertakken/studio-worker/releases/latest/download/studio-worker-installer.ps1 | iex
```

### From cargo

```bash
cargo install studio-worker
```

Each release ships pre-built binaries for:

- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`

## First run

```bash
# 1. Register with the studio API.
studio-worker register \
  --bootstrap-token <TOKEN> \
  --api-base-url https://studio.example.com

# 2. Install the auto-start service (systemd --user on Linux, launchd
#    on macOS, scheduled task on Windows).
studio-worker install-service
```

## CLI subcommands

| Subcommand           | Purpose                                                         |
| -------------------- | --------------------------------------------------------------- |
| `run`                | Start the heartbeat + claim + log + auto-update loops.          |
| `register`           | One-shot register with the API.  Idempotent.                    |
| `status`             | Print the local config + heartbeat info.                        |
| `install-service`    | Install the auto-start OS service.                              |
| `uninstall-service`  | Remove the auto-start OS service.                               |
| `enable`             | Set `auto_enabled = true` (resume claiming).                    |
| `disable`            | Set `auto_enabled = false` (worker online but doesn't claim).   |
| `set-threshold <gb>` | Set the max VRAM (GB) the worker is willing to claim per job.   |
| `config`             | Print the resolved config + its on-disk path.                   |
| `check-update`       | Check the release feed for a newer version (does not install).  |

## Configuration

Config lives at:

- Linux/macOS — `~/.config/minis-studio-worker/config.toml`
- Windows — `%APPDATA%\minis-studio-worker\config.toml`

```toml
api_base_url        = "https://studio.example.com"
bootstrap_token     = "<used only at register>"
worker_id           = "<filled by register>"
auth_token          = "<filled by register>"
vram_threshold_gb   = 12.0                       # max GB per claim
auto_start          = true
auto_enabled        = true
engine              = "synthetic"                # or "gradio"

# Only used when engine = "gradio":
gradio_endpoint_url = "http://127.0.0.1:7860"

# Optional: only declare these models to the studio.
supported_models_override = []

# Auto-update — checks the release feed on the cadence below, applies
# updates only when no job is running, then re-execs the new binary.
auto_update_enabled       = true
auto_update_interval_secs = 1800
auto_update_feed          = "https://api.github.com/repos/webbertakken/studio-worker/releases"
auto_update_prerelease    = false
```

## Engines

- **`synthetic`** (default) — produces deterministic, real WEBP/PNG/WAV/JSON
  outputs keyed by SHA-256 of the prompt/text/input.  No GPU required.  Use
  for smoke-tests, CI, and end-to-end verification of every modality.
- **`gradio`** — talks to a Gradio app running on `127.0.0.1` (image only).
  Drops the cloudflared tunnel entirely.  Supply the local Gradio URL in
  `gradio_endpoint_url` and the models you've verified in
  `supported_models_override`.

### Adding a real engine

Implement the `Engine` trait in `src/engine.rs` (see `SyntheticEngine`
and `GradioEngine` for examples).  An engine declares its `capabilities`
(per-kind supported models) and a `dispatch(model, task) -> TaskResult`
function.  Wire it into `engine::build()` behind a cargo feature, e.g.:

```toml
[features]
llama = ["dep:llama-cpp-2"]
```

The trait is already kind-aware so a single binary can host multiple
engines (one per modality).

## VRAM threshold

The worker reports two numbers to the API:

- `vramTotalGb` — physical VRAM on the host (probed from
  `/proc/driver/nvidia` on Linux; `0` when no NVIDIA GPU is present).
- `vramThresholdGb` — the **max** estimated VRAM per claim, controlled by
  the operator via `set-threshold` or by editing `config.toml`.

The studio API only hands a job to a worker if `job.vramGbEstimate ≤
worker.vramThresholdGb` **and** `job.model ∈ worker.supportedModels`.
Jobs that no worker can take stay `queued` until either a suitable worker
appears or the operator cancels.

## Auto-update

A dedicated background task polls the GitHub Releases feed every
`auto_update_interval_secs` (default 30 min).  When a higher semver is
available the worker:

1. Confirms no job is currently in flight (per a shared `busy` flag).
2. Downloads the cargo-dist installer for the current platform.
3. Runs it (it overwrites the binary in place).
4. Re-execs itself so the new code takes over.

Set `auto_update_enabled = false` to opt out.  Set
`auto_update_prerelease = true` to track pre-releases.

## Observability

Each tick of the worker pushes a batch of log entries to
`POST /workers/<id>/logs`.  The studio surfaces these in its LogViewer.

## Development

```bash
cargo test           # 169 unit + integration tests
cargo clippy --tests -- -D warnings
cargo fmt --check
cargo llvm-cov --workspace \
  --ignore-filename-regex 'src/main\.rs$' \
  --summary-only
```

Coverage CI enforces **≥ 90% line coverage**; current is **93.45%**.
Truly-untestable bits excluded from the gate:

- `src/main.rs` — the CLI bootstrap (all logic lives in `lib.rs`).
- `update::RealRunner::{download, run_installer}` — real network +
  process spawn (tested through the `UpdateRunner` trait with a fake).
- `update::restart_self` — calls `execvp`, never returns.
- `sys::detect_vram_gb` NVIDIA-specific branch — requires NVIDIA hardware.

Integration tests live under `tests/`:

- `tests/http_contract.rs` — exercises every API endpoint against a
  wiremock-based fake studio.
- `tests/http_errors.rs` — error-status paths for every endpoint.
- `tests/gradio_engine.rs` — GradioEngine code paths against a fake
  Gradio (incl. data-URL / relative-URL / object-with-url responses).
- `tests/multi_modal.rs` — every TaskKind round-trips through the
  synthetic engine + decoders.
- `tests/auto_update.rs` — release feed parsing + apply_with full flow.
- `tests/runtime_helpers.rs` — one-shot CLI helpers via wiremock.
- `tests/runtime_ticks.rs` — per-tick coverage of the background loops.
- `tests/runtime_loops.rs` — spawn_* wrappers driven with millisecond
  intervals to exercise the wrapper bodies.
- `tests/full_loop.rs` — one full claim → generate → complete cycle.

## Release process

1. PRs merge to `main` with conventional-commit titles
   (`feat:`, `fix:`, `docs:`, etc. — enforced by the Commit lint workflow).
2. `release-please` opens a release PR that bumps the version and updates
   the changelog.
3. Merging the release PR creates a git tag.
4. The tag triggers the `release.yml` workflow (cargo-dist), which builds
   binaries for all supported targets and uploads them to the GitHub
   release alongside `installer.sh` + `installer.ps1` one-liners.

## Licence

MIT.  See [LICENSE](./LICENSE).
