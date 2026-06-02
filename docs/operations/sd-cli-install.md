# Installing sd-cli (stable-diffusion.cpp)

The [`sdcpp`](../engines/sdcpp.md) engine subprocess-invokes
`sd-cli` per image job.

## You usually don't need this page

The worker **auto-provisions** `sd-cli` on the first image job: if no
binary is resolvable it downloads the platform's prebuilt
stable-diffusion.cpp **Vulkan** build (~37 MB, universal across
NVIDIA / AMD / Intel) into `<models_root>/bin/` and runs it from
there.  A fresh worker - Linux, macOS arm64, or Windows - serves real
image jobs out of the box with no manual step.  See
[auto-provisioning](../engines/sdcpp.md#auto-provisioning).

Every release target auto-provisions out of the box — Windows x64,
Linux x64, macOS arm64 and **Intel** (the upstream Darwin binary is
universal2), and Linux **arm64** (we build + host that one ourselves;
see [auto-provisioning](../engines/sdcpp.md#auto-provisioning)).

Use this playbook only when you want to **override** the auto-
provisioned binary: a CUDA build for maximum NVIDIA throughput, a
from-source build, an air-gapped mirror, or a target with no prebuilt
at all (e.g. Windows arm64).

The binary is not bundled into our release artefacts on purpose:
sd-cpp's pre-built matrix (CUDA / Vulkan / ROCm / Metal / CPU)
already covers the platforms we care about, bundling would either
ship a 30+ MB binary in every release or fragment the matrix, and the
worker fetches the right one on demand anyway.

## What to install

Upstream releases live at
<https://github.com/leejet/stable-diffusion.cpp/releases/latest>.
There is **no Linux CUDA pre-build** (only Vulkan / ROCm / CPU /
Windows-CUDA).  On NVIDIA the Vulkan build works perfectly \u2014 we
verified Z-Image-Turbo at 1024\u00d71024 / 8 steps in ~5-9 s on an
RTX 4090.  If you need every last drop of perf you can compile sd.cpp
from source against CUDA, but Vulkan is the unattended default.

| Platform | Asset |
|---|---|
| Linux x86_64 + NVIDIA / AMD / Intel | `sd-master-<sha>-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip` |
| Linux x86_64 + AMD ROCm 7.x | `sd-master-<sha>-bin-Linux-Ubuntu-24.04-x86_64-rocm-7.13.0.zip` |
| Linux x86_64 + CPU only | `sd-master-<sha>-bin-Linux-Ubuntu-24.04-x86_64.zip` |
| macOS arm64 | `sd-master-<sha>-bin-Darwin-macOS-15.7.7-arm64.zip` |
| Windows x64 + CUDA | `sd-master-<sha>-bin-win-cuda12-x64.zip` |

The zip ships three files:

- `sd-cli` \u2014 the CLI we invoke per job
- `sd-server` \u2014 long-running HTTP server (we don't use it today,
  but it's a likely future optimisation)
- `libstable-diffusion.so` \u2014 shared library both binaries dynlink

## Install layout

We expect:

```
~/.local/lib/stable-diffusion/
  sd-cli
  sd-server
  libstable-diffusion.so
~/.local/bin/
  sd-cli       \u2192 wrapper script that exports LD_LIBRARY_PATH
  sd-server    \u2192 same
```

The wrapper script keeps the actual binaries next to their .so and
sets `LD_LIBRARY_PATH` at launch time so the dynamic linker finds
the .so.

## One-shot installer

```bash
#!/usr/bin/env bash
set -euo pipefail

URL='https://github.com/leejet/stable-diffusion.cpp/releases/download/master-655-29ab511/sd-master-29ab511-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip'
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -L --fail -o "$TMPDIR/sd.zip" "$URL"
unzip -o "$TMPDIR/sd.zip" -d "$TMPDIR"

install -d "$HOME/.local/lib/stable-diffusion" "$HOME/.local/bin"
install -m 755 "$TMPDIR/sd-cli" "$TMPDIR/sd-server" "$HOME/.local/lib/stable-diffusion/"
install -m 644 "$TMPDIR/libstable-diffusion.so" "$HOME/.local/lib/stable-diffusion/"

for bin in sd-cli sd-server; do
  cat > "$HOME/.local/bin/$bin" <<EOF
#!/usr/bin/env bash
export LD_LIBRARY_PATH="\$HOME/.local/lib/stable-diffusion:\${LD_LIBRARY_PATH:-}"
exec "\$HOME/.local/lib/stable-diffusion/$bin" "\$@"
EOF
  chmod +x "$HOME/.local/bin/$bin"
done

sd-cli -h | head -1
```

Pin the URL to a specific `master-N-<sha>` build so re-runs are
reproducible.  The latest URL is fine for first install; for
unattended re-installation pick a sha from a known-good release.

## GPU runtime (Vulkan loader)

The Vulkan builds need the Vulkan **loader** present: `libvulkan.so.1`
(Linux) or `vulkan-1.dll` (Windows).  We can't auto-provision it — it
ships with the GPU driver (Windows) or a system package + driver
(Linux).  The engine preflights it and, when missing, fails the job
with the remedy: on Debian/Ubuntu `sudo apt install libvulkan1
mesa-vulkan-drivers` (plus the NVIDIA/AMD vendor driver), then verify
with `vulkaninfo --summary`.  macOS uses Metal — no Vulkan loader
needed.

## Resolution order (worker side)

On the first image job the engine resolves `sd-cli` in this order:

1. `$STUDIO_WORKER_SD_CLI` env var (absolute path; operator override)
2. `<models_root>/bin/sd-cli` - where the auto-provisioner installs,
   and where you can drop your own binary (default `~/models/bin/`)
   for a PATH-free override
3. `~/.local/bin/sd-cli` (matches the playbook above)
4. `sd-cli` on `$PATH`

If none resolve, the engine **auto-provisions** into
`<models_root>/bin/` (download + extract the platform Vulkan build),
then runs it.  Provisioning can be steered with:

- `STUDIO_WORKER_SDCPP_RELEASE` - a `master-<n>-<sha>` upstream tag to
  fetch instead of the pinned default
- `STUDIO_WORKER_SDCPP_URL` - a full zip URL (air-gapped mirror) that
  skips tag + asset resolution

Provisioning only fails on a target with no prebuilt Vulkan asset
(Linux arm64, Intel macOS) or with no network; the error names the
missing asset and points back here (on Windows the binary is
`sd-cli.exe`).

## Verifying the install

```bash
sd-cli -h | head -3
# stable-diffusion.cpp version unknown, commit <sha>
# Usage: /home/.../sd-cli [options]
# CLI Options:

# Smoke-test a real generation if you've staged Z-Image-Turbo files
cd ~/models
sd-cli --diffusion-model z_image_turbo-Q4_K.gguf \
  --vae ae.safetensors --llm Qwen3-4B-Instruct-2507-Q4_K_M.gguf \
  -p "a stone golem" --cfg-scale 1.0 --steps 8 \
  -W 512 -H 512 --diffusion-fa \
  -o /tmp/smoke.webp
file /tmp/smoke.webp
# /tmp/smoke.webp: RIFF (little-endian) data, Web/P image
```

If `sd-cli` panics with a Vulkan symbol error, your distro's Vulkan
loader is missing.  `apt install vulkan-tools libvulkan1` on Ubuntu;
`vulkaninfo --summary` is the smoke test.

## See also

- [`docs/engines/sdcpp.md`](../engines/sdcpp.md) \u2014 the worker engine that drives this binary
- [`docs/runtime/model-source.md`](../runtime/model-source.md) \u2014 where the model files come from
- Z-Image-Turbo author's `4GB VRAM` guide: <https://github.com/leejet/stable-diffusion.cpp/wiki/How-to-Use-Z-Image-on-a-GPU-with-Only-4GB-VRAM>
