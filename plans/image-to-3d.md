# Image to 3D: the torch engine and the mesh kind

The worker learns a new task kind, `mesh`: a picture in, a GLB out. Three PyTorch models make it,
Pixal3D, TRELLIS.2 and Hunyuan3D 2.1, through a new `torch` engine that provisions a Python
runtime with custom CUDA extensions, runs a model's pipeline in a child process and hands back the
GLB. The studio's Playground gets a 3D mode on top (minigames `plans/0249-playground-3d.md`).

Design: [`docs/engines/torch.md`](../docs/engines/torch.md) (written in phase 1).
Prototype: `~/Repositories/Pixal3D/setup-local.sh` and `download-local.sh`.

## Findings

- Verified (read): every engine is native (llama.cpp, sd.cpp, ONNX, parakeet); none runs Python.
- Verified (read, origin/main): transient jobs are admitted (`ModelHost::admit_transient`); a
  studio offer that does not fit is rejected `insufficient_memory`. A mesh job rides on it; the
  engine adds the cap on the child's allocator.
- Verified (read): an offer stays claimed while the worker heartbeats; there is no job time limit
  on the studio side, so a first job that provisions for half an hour is not reclaimed.
- Verified (run, `assets/images/0_img.png`, `--low_vram`, 1024 cascade, 4090 with 5.6 GB already
  used by the worker's shared models): 187 s end to end, of which about 95 s is start-up and
  weight loading; VRAM peak 22.3 GB (about 16.7 GB for Pixal3D); RAM about 20 GB in the child;
  GLB 37.6 MB (937 k faces, two 4096² WebP textures).
- Verified (HF API): Pixal3D MIT with `extra_gated_eu_disallowed: true`; TRELLIS.2-4B MIT;
  Hunyuan3D-2.1 own licence, Territory excludes the EU, UK and South Korea, EU flag set;
  BiRefNet MIT; MoGe-2 MIT; `camenduru/dinov3-vitl16-pretrain-lvd1689m` ungated mirror of the
  DINOv3 licence (`facebook/…` is gated manual); `briaai/RMBG-2.0` gated, avoided.
- Verified (read): TRELLIS.2's `pipeline.json` names `briaai/RMBG-2.0` and
  `facebook/dinov3-vitl16-pretrain-lvd1689m`, both gated; both are swapped the Pixal3D way.
- Verified (read): Pixal3D and TRELLIS.2 share torch 2.6.0 + cu124 and the same extensions
  (nvdiffrast, CuMesh, FlexGEMM, o-voxel); Pixal3D adds NATTEN and MoGe.
- Verified (read): Hunyuan3D 2.1 pins torch 2.5.1, numpy 1.24, transformers 4.46, diffusers 0.30,
  `bpy==4.0`, and builds `custom_rasterizer` and `DifferentiableRenderer`: it cannot share the
  Pixal3D environment. It needs 10 GB for shape and 21 GB for texture (README).
- Verified (registries, today): uv 0.13.3, NATTEN 0.21.7, nvdiffrast v0.4.0, torch 2.14.1 (the
  models pin 2.6.0 and 2.5.1), Hunyuan3D-2.1 `82920d6`, TRELLIS.2 `75fbf01`, Pixal3D `f7cf384`,
  CuMesh `12289e1`, FlexGEMM `6dd94a8`, MoGe `74fbce0` (Pixal3D needs `925b8ed`).

## Decisions (Webber, 2026-10-20; `DECISIONS.md`)

- A child process per job; one `torch` engine, a pipeline per model; environments `trellis`
  (Pixal3D, TRELLIS.2) and `hunyuan3d` (its pins clash); web-sized GLB, 40 000 triangles and
  1024 px textures; Hunyuan3D 2.1 textured within 16 GB (CPU offload, 512 px views); provisioned
  on the first job; Hunyuan's adapter in this repo.

## Phase 0: decisions

- [x] Record Webber's answers to the forks in `DECISIONS.md`.
- [x] Record the licences in `DECISIONS.md`: Pixal3D (EU flag, chosen anyway), TRELLIS.2 (MIT),
      Hunyuan3D 2.1 (Territory, personal use only, off every public and commercial path), DINOv3
      mirror, BiRefNet in place of RMBG-2.0.
- [x] Record the pins in `DECISIONS.md`, each with the latest version checked and why it is not
      taken where it is not.

## Phase 1: spec

- [x] `docs/engines/torch.md`: what the engine is, the runtime layout under `models_root`,
      provisioning, environments, pipelines, the runner protocol, admission and the VRAM cap,
      failure modes, observability (`op` table), privacy (inputs in a per-job temp dir).
- [x] `docs/runtime/model-source.md`: the `torch` engine and its `cliDefaults` fields.
- [x] `docs/architecture/overview.md`: the `mesh` kind and the torch engine (`flows.md` at fold-back).
- [x] `docs/index.md`: link the new page.

## Phase 2: wire contract

- [x] `TaskKind::Mesh` (`mesh`), `MeshParams { imageUrl, seed, ext }`, `TaskResult::Mesh { bytes, ext }`.
- [x] `ModelEngine::Torch` (`torch`); `ModelCliDefaults` gains `pipeline`, `resolution`,
      `textureSize`, `decimationTarget`, `lowVram`, `texture` (all optional).
- [x] `mime_for_ext("glb")` is `model/gltf-binary`; multipart `/complete` sends it (one table now,
      shared with the local API).
- [x] Synthetic engine: `synthetic-mesh` answers a valid GLB (one textured cube), so CI runs a mesh
      job end to end without a GPU.
- [x] Session: a mesh result uploads through multipart `/complete` like image, audio and video.
- [x] Tray UI: the `mesh` kind has a glyph (a cube); no thumbnail.
- [x] Tests: `multi_modal` wire round trip, synthetic mesh, `glb`, `mime_for_ext`.

## Phase 3: provisioning

- [x] `uv`: pinned release, sha256-checked, into `<models_root>/bin/`; `$STUDIO_WORKER_UV` overrides.
- [x] CUDA toolkit: NVIDIA redist manifest (its own sha256 pinned), every archive sha256-checked,
      merged with the cuBLAS, cuSPARSE, cuSOLVER, cuRAND, cuFFT and nvJitLink files of the
      environment's torch wheels, into each environment's `cuda/` (archives cached once).
- [x] Environments as data: Python version, torch index, pinned packages, sources at pinned
      commits, extensions with their build flags; a recipe hash in a marker, a changed recipe
      rebuilds; builds for the GPU's compute capability (`nvidia-smi --query-gpu=compute_cap`).
- [x] Environment `trellis` (Pixal3D and TRELLIS.2, plus flash-attn for TRELLIS.2) and environment
      `hunyuan3d` (Hunyuan3D 2.1).
- [x] Sources: git checkouts at pinned commits (submodules when asked) under
      `<models_root>/torch/src/`; NAF into `TORCH_HOME/hub`; `git`, `tar` and `c++` preflight with
      the remedy when missing.
- [x] Weights: each pipeline's `prepare` fetches its snapshots at pinned revisions
      (`snapshot_download`, `HF_HOME=<models_root>/hf-cache`, `TORCH_HOME=<models_root>/torch-hub`),
      then runs offline; local paths everywhere, so nothing resolves a moving `main`.
- [x] Model folders that swap gated parts: `pipeline.json` naming `ZhengPeng7/BiRefNet` (and the
      DINOv3 mirror for TRELLIS.2), `ckpts` linked to the snapshot.
- [x] One provisioning at a time (a lock file); a failed step leaves no marker and says which step
      failed, with the last lines of its output.
- [x] Unit tests for the builders, the marker logic and the manifest parser; `tests/torch_provision.rs`
      builds a fake environment end to end (wiremock uv and redist, a fake uv, a local git source).

## Phase 4: runner

- [x] `python/runner.py` (embedded with `include_str!`, written beside the environment): reads a
      request JSON, sets the VRAM cap (`torch.cuda.set_per_process_memory_fraction`), runs one
      pipeline, writes the GLB, prints progress as JSON lines on stdout, logs on stderr, exits
      0 (done), 2 (out of memory), 3 (bad input), 1 (anything else).
- [x] Pipelines `pixal3d.py`, `trellis2.py`, `hunyuan3d.py`: preprocess, generate, export GLB at
      the source's decimation target and texture size.
- [x] Rust side: spawn through `crate::process::command`, stream both pipes into tracing (the job
      log, progress-bar frames dropped), map exit codes to typed errors, kill the child on a
      timeout; the runner dies with the worker (`PR_SET_PDEATHSIG`, a parent watch).
- [x] Tests: protocol parsing, exit-code mapping, a fake runner script for the spawn path.

## Phase 5: engine

- [x] `TorchEngine`, registered on every build (no feature: it adds no dependency); the platform
      decides what it advertises.
- [x] Capabilities: `mesh` only on Linux x86_64 with an NVIDIA GPU of compute capability 8.0 or
      more; logged either way.
- [x] The budget: `maxVramGib` required, passed to the runner as its allocator cap; admission is
      the host's, as for every transient job; never unloads a model.
- [x] Dispatch: fetch the picture (`vision::fetch`, studio URL or `data:` URL), provision, run,
      check the GLB magic, return `TaskResult::Mesh`.
- [x] Every decision point logs with `op` under `studio_worker::engine::torch`.
- [x] Tests: capabilities gating, a missing budget or pipeline, an unreadable picture, the runner
      with a fake runtime.

## Phase 6: local API

- [x] `POST /mesh` (a picture raw, multipart or JSON; `model?`, `seed?`) answers `model/gltf-binary`;
      the picture parsing is shared with the vision routes (`local_api/picture.rs`).
- [x] `docs/local-api.md`: the route.
- [x] Tests: happy path on the synthetic mesh model, bad picture, unknown model, busy gate.

## Phase 7: real-model verification (4090)

- [x] Provision `trellis` from nothing: 13.5 min (NATTEN 10.7 min, o-voxel 2 min, CuMesh 2 min).
- [x] Pixal3D on `0_img.png` and `9_img.png` (`tests/real_torch_mesh.rs`): 107 to 178 s, 14.2 GiB
      peak, 2.1 MB, 39 k triangles; looked at.
- [x] TRELLIS.2 the same: at 1024 the crab runs out of memory at 17 GiB; at 512, 100 s, 4.0 GiB,
      2.2 MB; looked at.
- [ ] Provision `hunyuan3d`; Hunyuan3D 2.1 the same.
- [ ] A studio job end to end through a local studio (minigames plan 0249).
- [x] `tests/real_torch_mesh.rs` (skipped unless `RUN_REAL_ENGINE_TESTS=1`) reproduces the above.

## Phase 8: ship

- [ ] `cargo fmt --check`, clippy in all three feature sets, `cargo test`, coverage gate 90 %.
- [ ] PR (title ≤ 52 characters), CI green, merge.
- [ ] Release through release-please, tag, install, `/healthz` shows the version.

## Phase 9: fold-back

- [ ] Docs match what shipped: `torch.md`, model source, lifecycle,
      local API, overview, flows, `AGENTS.md` project layout; every symbol and link checked.
