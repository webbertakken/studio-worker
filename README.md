# studio-worker

[![Checks](https://github.com/webbertakken/studio-worker/actions/workflows/checks.yml/badge.svg)](https://github.com/webbertakken/studio-worker/actions/workflows/checks.yml)
[![Build](https://github.com/webbertakken/studio-worker/actions/workflows/build.yml/badge.svg)](https://github.com/webbertakken/studio-worker/actions/workflows/build.yml)

A small Rust binary that pulls image-generation jobs from the minis.gg
studio API, runs them locally (synthetic or Gradio), and posts the
results back.

Replaces the previous push-based studio-proxy + cloudflared topology
with a pull-based pipeline: install the worker on any GPU PC, register
once, and it will claim queued jobs whose VRAM estimate fits its
threshold.

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
# Register with the API.  The bootstrap token is set as a Cloudflare
# Worker secret on the studio side; ask the studio operator for it.
# For local dev the default is `dev-bootstrap-token`.
studio-worker register \
  --bootstrap-token <TOKEN> \
  --api-base-url https://studio.example.com

# Install the auto-start service (systemd --user on Linux, launchd on
# macOS).  On Windows the binary writes a Task XML file and prints the
# `schtasks /Create` invocation.
studio-worker install-service
```

## CLI subcommands

| Subcommand           | Purpose                                                         |
| -------------------- | --------------------------------------------------------------- |
| `run`                | Start the heartbeat + claim loop in the foreground.             |
| `register`           | One-shot register with the API.  Idempotent.                    |
| `status`             | Print the local config + heartbeat info.                        |
| `install-service`    | Install the auto-start OS service.                              |
| `uninstall-service`  | Remove the auto-start OS service.                               |
| `enable`             | Set `auto_enabled = true` (resume claiming).                    |
| `disable`            | Set `auto_enabled = false` (worker online but doesn't claim).   |
| `set-threshold <gb>` | Set the max VRAM (GB) the worker is willing to claim per job.   |
| `config`             | Print the resolved config + its on-disk path.                   |

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

# Optional: only declare these models to the studio (defaults to the
# engine's full list).  Required for `gradio`.
supported_models_override = []
```

## Engines

- **`synthetic`** — produces deterministic, real WEBP/PNG images keyed by
  SHA-256 of the prompt.  No GPU required.  Use for smoke-tests, CI, and
  end-to-end verification.
- **`gradio`** — talks to a Gradio app running on `127.0.0.1`.  Drops the
  cloudflared tunnel: the worker is on the same machine as the GPU.  Supply
  the local Gradio URL in `gradio_endpoint_url` and the models you've
  verified work in `supported_models_override`.

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

## Observability

Each tick of the worker pushes a batch of log entries to
`POST /workers/<id>/logs`.  The studio surfaces these in its LogViewer.

## Development

```bash
# Run all tests (unit + integration).  None of them need a GPU.
cargo test

# Lints.
cargo clippy --tests -- -D warnings
cargo fmt --check
```

Integration tests live under `tests/`:

- `tests/http_contract.rs` — exercises every API endpoint against a
  wiremock-based fake studio.
- `tests/gradio_engine.rs` — exercises the GradioEngine code path against a
  wiremock-based fake Gradio.
- `tests/full_loop.rs` — wires all of the above together to prove one
  complete claim → generate → complete → ship-logs cycle.

The default `cheap-models` story: tests use the synthetic engine and a
mock Gradio that returns pre-rendered procedural images.  No VRAM is
consumed, so running the test suite on the GPU PC does not disturb other
GPU workloads.

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
