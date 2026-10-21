# torch engine: PyTorch models in a child process

Implemented in [`src/engine/torch/`](../../src/engine/torch/) and [`python/`](../../python/).
The `torch` engine (`ModelEngine::Torch`) runs PyTorch models that need custom CUDA extensions,
which no native engine can: today the image-to-3D models of the `mesh` kind.

| Model | Pipeline | Environment | Licence |
| --- | --- | --- | --- |
| Pixal3D (`TencentARC/Pixal3D`) | `pixal3d` | `trellis` | MIT; the card disallows the EU |
| TRELLIS.2 (`microsoft/TRELLIS.2-4B`) | `trellis2` | `trellis` | MIT |
| Hunyuan3D 2.1 (`tencent/Hunyuan3D-2.1`) | `hunyuan3d` | `hunyuan3d` | Tencent's; not in the EU, UK or South Korea; personal use only |

Decisions and licences: `DECISIONS.md`, "Image to 3D".

## Terms

| Term | Meaning |
| --- | --- |
| pipeline | one model's adapter: how it is fetched, run and exported (`python/pipelines/<name>.py`) |
| environment | a Python virtual environment with torch and the CUDA extensions a pipeline needs |
| recipe | the data an environment is built from: Python, torch, packages, sources, extensions |
| provisioning | building what a job needs that is not there yet: `uv`, an environment, weights |
| runner | `python/runner.py`: runs one pipeline for one job in a child process |
| budget | the device memory a job may use (`maxVramGib`), enforced on the child's CUDA allocator |

## The mesh kind

- A `mesh` task is a picture in and a GLB out: `{ "kind": "mesh", "imageUrl", "seed"?, "ext": "glb" }`.
- `imageUrl` is the studio's job-input URL, or a `data:image/…;base64,` URL from the local API.
- The result is `TaskResult::Mesh { bytes, ext: "glb" }`, uploaded through the multipart
  `/complete` route as `model/gltf-binary`.
- A GLB is web-sized: at most `WEB_GLB_TRIANGLES` (40 000) triangles and textures of
  `WEB_GLB_TEXTURE_PX` (1024) pixels, about 3 to 8 MB (Webber, 2026-10-20), unless the model's
  `cliDefaults` say otherwise.
- The synthetic engine serves `synthetic-mesh`: a textured cube, so CI runs a mesh job end to end.

## Model source

```jsonc
{
  "engine": "torch",
  "files": [],                    // weights are the pipeline's own (below)
  "cliDefaults": {
    "pipeline": "pixal3d",        // pixal3d | trellis2 | hunyuan3d
    "maxVramGib": 15.5,           // the budget; required
    "steps": 12,                  // flow sampling steps per stage
    "cfgScale": 7.5,              // guidance of the shape stage
    "width": 1024, "height": 1024, // unused by mesh pipelines (the type requires them)
    "resolution": 1024,           // pipeline resolution (Pixal3D, TRELLIS.2 cascade)
    "lowVram": true,              // keep stages on the CPU until they run
    "decimationTarget": 40000,    // triangles in the GLB
    "textureSize": 1024           // texture pixels in the GLB
  }
}
```

## Platform

- The engine registers on every build. It advertises the `mesh` kind only on Linux x86_64 with an
  NVIDIA GPU of compute capability 8.0 or more (`nvidia-smi --query-gpu=compute_cap`), and logs
  `op="platform"` either way, with the reason when it does not.
- No system CUDA toolkit, no root: everything it builds and downloads lives under `models_root`.
- It needs `git`, `tar` with xz, and a C/C++ compiler on the `PATH`; provisioning checks them first
  and names the missing one.

## Layout

| Path | What |
| --- | --- |
| `<models_root>/bin/uv` | `uv` at `UV_VERSION`, sha256-checked |
| `<models_root>/torch/python/` | Python interpreters `uv` installs |
| `<models_root>/torch/uv-cache/` | `uv`'s package cache |
| `<models_root>/torch/archives/` | NVIDIA redist archives, sha256-checked |
| `<models_root>/torch/src/<name>-<rev>/` | sources at pinned revisions |
| `<models_root>/torch/envs/<environment>/` | an environment: `.venv/`, `cuda/` (its `CUDA_HOME`), `recipe.json` |
| `<models_root>/torch/models/<pipeline>/` | model folders that replace gated parts |
| `<models_root>/torch/cache/` | `XDG_CACHE_HOME`, extension autotune caches |
| `<models_root>/torch/provision.lock` | one provisioning at a time |
| `<models_root>/hf-cache/` | `HF_HOME`: every Hugging Face weight |
| `<models_root>/torch-hub/` | `TORCH_HOME`: `torch.hub` code and weights |

Nothing is written outside `models_root` except the job's temporary directory, which holds the
input picture and the GLB and is removed when the job ends, on every path.

## Provisioning

A job provisions what its pipeline lacks, in this order, logging each step (`op="provision"`,
`step`, elapsed time):

1. `uv`: `$STUDIO_WORKER_UV` when set, else `<models_root>/bin/uv`, downloaded from the `uv`
   GitHub release at `UV_VERSION` and checked against its sha256 when missing.
2. The environment, when its `recipe.json` is missing or holds another recipe hash:
   1. the Python the recipe names (`uv python install`), then a fresh `.venv`;
   2. torch and torchvision from the PyTorch index of the recipe's CUDA;
   3. `CUDA_HOME`: the NVIDIA redist archives of the recipe's CUDA (`nvcc`, `cudart`, `cccl`,
      `profiler_api`, `nvtx`), each checked against the manifest's sha256, merged with the
      cuBLAS, cuSPARSE, cuSOLVER, cuRAND, cuFFT and nvJitLink headers and libraries of the
      environment's torch wheels;
   4. the recipe's packages, then its no-dependency packages;
   5. the sources at their pinned revisions (`git clone`, submodules when asked);
   6. the extensions, built for this GPU (`TORCH_CUDA_ARCH_LIST` from its compute capability)
      with `--no-build-isolation`;
   7. an import check of every module the recipe lists;
   8. `recipe.json`, written last: an environment without it is never used.
3. The weights: the runner's `prepare` downloads the pipeline's Hugging Face snapshots at pinned
   revisions into `HF_HOME` (a no-op when present), and writes its model folder.

- A failed step stops the job with `TorchError::Provision { step, tail }` (the last 40 lines of
  its output) and leaves no marker, so the next job starts that step again.
- Builds and downloads run under `provision.lock`; a second job waits for it.
- First provisioning of `trellis` takes about 15 minutes on a 4090 box (NATTEN alone about 10).

### Environments

| Environment | Python | torch | CUDA | Extensions | Sources |
| --- | --- | --- | --- | --- | --- |
| `trellis` | 3.10 | 2.6.0 + cu124 | 12.4.1 | nvdiffrast, CuMesh, FlexGEMM, o-voxel, NATTEN; flash-attn (wheel) | TRELLIS.2, Pixal3D, NAF |
| `hunyuan3d` | 3.10 | 2.5.1 + cu124 | 12.4.1 | custom_rasterizer, mesh_inpaint_processor | Hunyuan3D-2.1 |

Hunyuan3D 2.1 pins numpy 1.24, transformers 4.46 and diffusers 0.30, which the `trellis`
environment (numpy 2, transformers 4.57, diffusers 0.37) cannot hold, hence two environments. Its
`bpy` (Blender) is not installed: the runner gives it a stub module and writes the GLB itself.

### Gated parts

- Pixal3D and TRELLIS.2 name `briaai/RMBG-2.0` (gated) to remove the background. The pipeline's
  model folder holds a `pipeline.json` that names `ZhengPeng7/BiRefNet` instead, with `ckpts`
  linked to the downloaded snapshot. Upstream code is never edited.
- TRELLIS.2 names `facebook/dinov3-vitl16-pretrain-lvd1689m` (gated); its folder names the mirror
  `camenduru/dinov3-vitl16-pretrain-lvd1689m`, as Pixal3D's code does.

## A job

1. **Platform**: no `mesh` support here fails with `UnsupportedTask` (never retried).
2. **Picture**: fetched (`vision::fetch`: the studio URL or a `data:` URL, 25 MiB at most),
   decoded to check it is a picture, written to the job's temporary directory as PNG.
3. **Admission** happened before the job started, as for every transient job
   ([model lifecycle](../runtime/model-lifecycle.md#concurrency)): the offer's `vramGbEstimate`
   (or the catalogue's) next to the loaded models; a studio offer that does not fit is rejected
   `insufficient_memory`. The estimate covers the budget plus the CUDA context (about 0.5 GiB).
   The engine never unloads a model; it only uses what is free.
4. **Provisioning** as above.
5. **Run**: `python runner.py <request.json>` in the environment, as a child process
   (`crate::process::command`): `request.json` names the pipeline, the picture, the GLB path,
   the seed, the budget and the `cliDefaults`. It runs until it exits or `RUN_TIMEOUT`
   (30 minutes) passes, when it is killed.
6. **Result**: the GLB is read back, its header checked (`glTF`, version 2, its length), and
   returned. The temporary directory is removed.

### Runner protocol

- The runner caps the process's CUDA allocator at the budget
  (`torch.cuda.set_per_process_memory_fraction(budget / total)`), so a job cannot take the memory
  of models the worker keeps loaded.
- Its stdout carries events only, one JSON object per line; everything libraries print goes to
  stderr (fd 1 is redirected to fd 2 before any import).

| Event | Fields |
| --- | --- |
| `stage` | `name` (`prepare`, `load`, `preprocess`, `generate`, `export`) |
| `done` | `triangles`, `bytes`, `vramPeakGib`, `seconds` |
| `failed` | `class` (`out_of_memory`, `bad_input`, `internal`), `message` |

- Exit codes: 0 done, 2 out of memory, 3 bad input, 1 anything else.
- `TQDM_DISABLE=1` keeps progress bars out of the log; `PYTHONUNBUFFERED=1` keeps lines live.
- Both pipes stream into tracing (`op="runner"`), so the job log shows the run as it happens.
- The runner dies with the worker: `PR_SET_PDEATHSIG` and a parent watch.

### Errors

| Error | When | Retried by the studio |
| --- | --- | --- |
| `UnsupportedTask` | no mesh support on this machine | no |
| `TorchError::Provision` | a provisioning step failed | yes |
| `TorchError::BadInput` | the picture cannot be read, or the pipeline refuses it | no |
| `TorchError::OutOfMemory` | the run passed its budget | no |
| `TorchError::Runner` | the runner failed otherwise (exit code, last lines) | yes |
| `TorchError::Timeout` | `RUN_TIMEOUT` passed | yes |
| `TorchError::InvalidGlb` | the output is missing or not a GLB | yes |

## Budgets (measured on an RTX 4090)

| Pipeline | Budget | Settings | Measured |
| --- | --- | --- | --- |
| `pixal3d` | 15.5 GiB (estimate 16.5) | `lowVram`, 1024 cascade | 107 to 178 s a run; 14.2 GiB peak on both test pictures; about 2.1 MB |
| `trellis2` | 8 GiB (estimate 9) | `lowVram`, 512 | 100 s a run; 4.0 GiB peak; about 2.2 MB |
| `hunyuan3d` | 16 GiB | CPU offload, 6 views of 512 px | measured in the plan's phase 7 |

- A run's first job also loads its weights from disk (about 50 to 90 s) and, for Pixal3D, autotunes
  FlexGEMM once (cached in `<torch>/cache`).
- TRELLIS.2 runs at 512: at 1024 its shape decoder needs more than 17 GiB on a detailed picture
  (the crab sample ran out of memory at 8, 12 and 17 GiB), which does not fit next to the shared
  models on a 24 GB card; at 512 it peaks at 4 GiB and, decimated to 40 000 triangles, keeps its
  detail. The registry can set `resolution: 1024` on a card with room.
- The estimate is the budget plus about 1 GiB for the CUDA context; admission keeps another 1 GiB
  in reserve, so a run fits when `estimate + 1` GiB is free.

## Observability

All under target `studio_worker::engine::torch`.

| `op` | When |
| --- | --- |
| `platform` | at start: mesh advertised or not, and why |
| `provision` | each provisioning step: started, done (seconds), failed (tail) |
| `runner` | each line the runner prints (stderr at info, events parsed) |
| `run` | the run started, finished (seconds, triangles, bytes, peak memory) or failed |

## Constants

| Constant | Value | Provenance | Safe range |
| --- | --- | --- | --- |
| `WEB_GLB_TRIANGLES` | 40 000 | Webber, 2026-10-20: web-sized GLB, 3 to 8 MB | 10 000 to 1 000 000 |
| `WEB_GLB_TEXTURE_PX` | 1024 | Webber, 2026-10-20 | 512 to 4096 |
| `RUN_TIMEOUT` | 30 min | a Pixal3D run is about 3 min; Hunyuan with offload is slower | 10 to 55 min |
| `MIN_COMPUTE_CAPABILITY` | 8.0 | the extensions' kernels (bf16, NATTEN) | 8.0 and up |
| `UV_VERSION` | 0.13.3 | latest on 2026-10-20 | any release with `python install` |
| `CUDA_REDIST_VERSION` | 12.4.1 | the cu124 wheels both environments pin | must match torch's CUDA |
