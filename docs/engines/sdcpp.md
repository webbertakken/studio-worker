# sd-cpp engine: real image inference

Implemented in [`src/engine/sdcpp.rs`](../../src/engine/sdcpp.rs).
Image-kind only.  Runs the [`stable-diffusion.cpp`](https://github.com/leejet/stable-diffusion.cpp)
CLI binary as a subprocess per job, parses the resulting WEBP off
disk, hands it back as a `TaskResult::Image`.

Used in conjunction with the studio's [`ModelSource`](../runtime/model-source.md)
contract: the engine doesn't hardcode any model names, paths, or
URLs.  It reads the file roster off every offer.

## Why a subprocess instead of FFI

The Rust ecosystem doesn't have a mature crate that wraps
stable-diffusion.cpp.  Writing one ourselves means dragging in CUDA /
Vulkan / Metal headers, dealing with `ggml` symbol clashes against
`llama-cpp-2` (both statically link `ggml.cpp`), and re-doing the
matrix of pre-built binaries upstream already publishes.

A subprocess gets us:

- Zero build-system pain on the Rust side; `cargo build --features
  ui` doesn't need CUDA.
- The upstream pre-built binaries we use \u2014 the Vulkan Linux build
  works on NVIDIA, AMD, Intel.
- Process isolation: a buggy sd-cli OOM kills the subprocess, not
  the worker.

The trade-off is per-job startup overhead.  In practice on a 4090
running the Vulkan build, model load is ~1s and amortises across
the steady-state job stream (`sd-cli` loads weights once then
generates back-to-back in the same process when called repeatedly
\u2014 actually no, each call spawns fresh, so each job pays the load
cost; see [Performance](#performance) below).

## Engine registration

`SdCppEngine::new(&cfg.models_root)` runs on every `engine::build()`
and **always registers** - it touches no filesystem and never fails.
Capabilities advertise the image kind with a single sentinel model
name `"sd-cpp:*"` for informational purposes; selection on the studio
side is kind-based, not model-based.

`sd-cli` is resolved lazily on the **first image job** (cached for the
worker's lifetime) via `ensure_sd_cli`:

1. A path cached from a previous job (if still a file).
2. `$STUDIO_WORKER_SD_CLI` -> `<models_root>/bin/sd-cli` ->
   `~/.local/bin/sd-cli` -> `$PATH` - any operator install wins.
3. **Auto-provision**: if nothing resolves, download the platform's
   prebuilt stable-diffusion.cpp Vulkan build and extract it into
   `<models_root>/bin/` (see [auto-provisioning](#auto-provisioning)).

So a fresh worker - including a Windows prod install - serves real
image jobs out of the box: the binary and the model weights both
download on demand.

## Auto-provisioning

Implemented in [`src/engine/sd_provision.rs`](../../src/engine/sd_provision.rs).
On the first image job with no resolvable `sd-cli`, the engine:

1. Downloads the pinned prebuilt zip for this platform (see the matrix
   below), routed to upstream or our own release via `AssetSource`.
2. Extracts `sd-cli`(`.exe`) + the shared library
   (`stable-diffusion.dll` / `libstable-diffusion.so` / `.dylib`)
   flat into `<models_root>/bin/` (the path-free slot the resolver
   prefers).  Flattening to bare file names also defuses zip-slip.
3. On Linux / macOS the per-job `Command` gets `LD_LIBRARY_PATH` /
   `DYLD_LIBRARY_PATH` pointed at that dir so the loader finds the
   sibling library; Windows resolves sibling DLLs automatically.

### Platform matrix

Every release target can auto-provision out of the box:

| Target | Backend | Asset source |
|---|---|---|
| Windows x64 | Vulkan | upstream `win-vulkan-x64` |
| Linux x64 | Vulkan | upstream `Linux-Ubuntu-24.04-x86_64-vulkan` |
| macOS arm64 | Metal | upstream `Darwin-macOS-15.7.7-arm64` |
| macOS x64 (Intel) | Metal | upstream `Darwin-…-arm64` — it's a **universal2** binary |
| Linux arm64 | Vulkan | **our** `sdcpp-prebuilt-<ref>` release (see below) |

Upstream ships no aarch64-Linux build, so
[`.github/workflows/sdcpp-prebuilt.yml`](../../.github/workflows/sdcpp-prebuilt.yml)
builds `sd-cli` (Vulkan, shared lib) on a native `ubuntu-24.04-arm`
runner at the same pinned sd.cpp commit, smoke-tests it, and publishes a
`sdcpp-prebuilt-<ref>` release the provisioner downloads from.  Re-run
it (manual dispatch) whenever `DEFAULT_RELEASE_TAG` is bumped.

### GPU runtime requirement (Vulkan)

The Vulkan builds need the Vulkan **loader** on the box:
`libvulkan.so.1` (Linux) or `vulkan-1.dll` (Windows).  Windows GPU
drivers ship it; on Linux install `libvulkan1` + a GPU driver.  We can't
auto-provision it (it comes with the driver / a system package), so the
engine **preflights** the loader (via `dlopen`/`LoadLibrary`) before
spawning sd-cli and, when it's missing, fails the job with the exact
remedy instead of a cryptic sd-cli crash.  macOS uses Metal, so no
Vulkan loader is involved there.

Overrides:

| Env | Effect |
|---|---|
| `STUDIO_WORKER_SD_CLI` | Absolute path to an existing binary; skips provisioning |
| `STUDIO_WORKER_SDCPP_RELEASE` | A `master-<n>-<sha>` tag to fetch instead of the pinned default |
| `STUDIO_WORKER_SDCPP_URL` | A full zip URL (air-gapped mirror / tests); skips tag + asset resolution |

When `STUDIO_WORKER_SDCPP_RELEASE` / `STUDIO_WORKER_SDCPP_URL` are in
effect the worker logs an `info` breadcrumb at
`studio_worker::engine::sd_provision` naming the env var that won, so a
typo'd override that's silently ignored is easy to spot in the logs.

Targets with no prebuilt at all (e.g. Windows arm64) error with a
pointer to the manual [install playbook](../operations/sd-cli-install.md).

There is **no** pre-staged model registry on the worker any more.
The legacy `with_builtin(models_root)` returned a hardcoded
`z-image-turbo` entry; that's gone.

## Per-job flow

`dispatch_with_source(model, task, Some(&source))`:

1. **`ensure_files(&source)`** \u2014 for every `ModelFile` in
   `source.files`, check `cfg.models_root / file.filename`.  If
   missing, stream-download from `file.url` (atomic via `.part` +
   rename).  Cached after first download.
2. **Resolve roles**: pick out the diffusion-model + vae +
   text-encoder paths.  A `model` role is also accepted as the
   diffusion file (single-file packagings like Flux).
3. **Build `sd-cli` argv**:

   ```
   sd-cli
     --diffusion-model <local diffusion file>
     --vae             <local vae file>          # if present
     --llm             <local text encoder>      # if present
     -p                <prompt from the task>
     --cfg-scale       <cli_defaults.cfgScale>
     --steps           <cli_defaults.steps or task.steps if explicit>
     -W                <cli_defaults.width or task.width>
     -H                <cli_defaults.height or task.height>
     -o                /tmp/studio-worker-sdcpp/out-<pid>-<nanos>.webp
     --sampling-method <cli_defaults.samplingMethod>  # if present
     --diffusion-fa                                   # always
     --seed            <task.seed>                    # if explicit
   ```

   The CLI defaults from the studio's source win over the task's
   when the task is at its parameter-default value (e.g. `steps=20`
   is the studio's default; we override to the model's 8-step
   schedule).  When the task explicitly overrides we honour it.

4. **`Command::output()`** \u2014 blocking wait.  On non-zero exit,
   warn-log the stderr's last line + bail with a `Fail { retryable:
   true }`-shaped anyhow error.  Operators see the OOM / driver
   message immediately in the worker logs.

5. **Read `out_path`** \u2014 the WEBP bytes from `sd-cli`.  Delete the
   file (best-effort; ignored on failure).
6. **Return `TaskResult::Image { bytes, ext: "webp" }`** \u2014 the WS
   session uploads it via the multipart `/complete` route.

## Models root

`cfg.models_root` defaults to `~/models`.  Every file from every
`ModelSource.files` lands directly in that directory \u2014 there's no
per-model subdirectory.  Two models that name the same `filename`
will collide; in practice the registry uses distinguishing filenames
(e.g. `z_image_turbo-Q4_K.gguf` vs `Qwen3-4B-Instruct-2507-Q4_K_M.gguf`).

A future enhancement is per-model subdirs + a manifest file with
sha256 verification.  Tracked in [`plans/real-models-on-demand.md`](../../plans/real-models-on-demand.md).

## Performance

For Z-Image-Turbo Q4_K (4GB diffusion) + Qwen3-4B Q4_K_M (2.5GB) +
Flux VAE (335MB) on an RTX 4090 via the Vulkan build:

- Model load (cold): ~1s
- 8-step generation at 1024\u00d71024: ~5-9s
- 8-step generation at 768\u00d7768: ~5s
- Multipart upload to R2: 2-3s
- **End-to-end per job: ~10-15s steady-state**

Each `sd-cli` invocation pays the model-load cost.  That's the
biggest single optimisation we haven't done \u2014 keeping an
`sd-server` subprocess alive and routing every job through HTTP
would amortise load to once per worker boot.  Not done yet because
the current rate is already inside the operator's tolerance window.

## Failure modes specific to this engine

| Failure | Cause | Recovery |
|---|---|---|
| `requires a ModelSource on the offer` | Legacy queued row without `modelSource` (pre-migration-0015) | Re-promote via the studio; the registry attaches the source |
| Download `GET <url> -> 404` | Bad URL in the registry, or HF moved the file | Operator fixes the registry entry |
| Download `GET <url> -> 401` | HF gated repo without auth | Operator finds a public mirror (see Z-Image-Turbo's VAE \u2014 BFL's repo is gated, Comfy-Org's mirror isn't) |
| `sd-cli exited with Some(...): error: invalid parameter for argument` | sd-cli flag drift between releases | Pin the sd-cli version in the install playbook |
| `sd-cli exited` with no stderr line | OOM or driver crash | Lower `cli_defaults.width`/`height`; check `nvidia-smi` |

## Install playbook

See [`docs/operations/sd-cli-install.md`](../operations/sd-cli-install.md).

## Where this came from

Shipped in PR #17 (`feat: real image inference via stable-diffusion.cpp`)
after the synthetic engine spent ~3 hours uploading placeholder
bytes for real-model jobs.  The first iteration had a hardcoded
`z-image-turbo` registry on the worker side; the [ModelSource](../runtime/model-source.md)
re-architecture moved that knowledge to the studio.
