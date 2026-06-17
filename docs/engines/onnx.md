# ONNX engine (LaMa object removal)

The `onnx` engine (`ModelEngine::Onnx`, cargo feature `image-onnx`) runs ONNX
models via [pykeio/ort](https://ort.pyke.io). Its first model is **LaMa**
(`Carve/LaMa-ONNX`) — the Find-the-Differences object-removal engine: it
reconstructs the background under a mask, never hallucinating a replacement, and
the studio composites the result so outside-mask pixels stay byte-identical.

## Why load-dynamic (and what that means)

`ort` is built with the **`load-dynamic`** feature, not static `download-binaries`.
The binary therefore links **no** native ONNX Runtime at build time, so it
cross-compiles cleanly on every cargo-dist target with none of the
prebuilt-static-link failures we hit otherwise:

- linux-x64: glibc 2.38 `__isoc23_*` undefined symbols
- linux-arm64: glibc + libstdc++ (`__cxa_call_terminate`) ABI
- windows: unresolved MSVC CRT imports (`__imp_strncpy`, `__imp___timezone`, …)
- macOS-Intel: no prebuilt published at all

Instead, the **ONNX Runtime shared library is downloaded per-platform at
runtime** — exactly like `sd-cli` and model weights are provisioned on demand.

## Runtime provisioning (`onnx_provision.rs`)

On the first onnx job the worker:

1. picks the Microsoft ONNX Runtime release asset for the host platform
   (version pinned to **`ORT_VERSION` = 1.24.2**, the build `ort` 2.0.0-rc.12
   targets — `ORT_API_VERSION` 24);
2. downloads it from
   `https://github.com/microsoft/onnxruntime/releases/download/v<ver>/<asset>`
   and extracts the main shared library (pure-Rust `flate2`+`tar` for `.tgz`,
   `zip` for `.zip`) into `<models_root>/onnxruntime/`;
3. points `ort` at it via the `ORT_DYLIB_PATH` env var (set once, before the
   first session).

The download is cached, so subsequent jobs reuse the local library. A different
ONNX model id just needs a `studioModels` row with `engine = "onnx"`; the engine
downloads the `.onnx` (role `model`) on demand like any other.

| target | asset | runtime onnx |
| ------ | ----- | ------------ |
| linux-x64 | `onnxruntime-linux-x64-<ver>.tgz` | ✅ |
| linux-arm64 | `onnxruntime-linux-aarch64-<ver>.tgz` | ✅ |
| macOS-arm64 | `onnxruntime-osx-arm64-<ver>.tgz` | ✅ |
| windows-x64 | `onnxruntime-win-x64-<ver>.zip` | ✅ |
| windows-arm64 | `onnxruntime-win-arm64-<ver>.zip` | ✅ |
| macOS-Intel | — (none published upstream) | binary builds; onnx jobs error clearly |

Bumping ONNX Runtime: change `ORT_VERSION` in `onnx_provision.rs` to a version
whose `ORT_API_VERSION` is ≥ the `api-NN` feature on the `ort` dependency.

## Where this came from

The LaMa engine shipped in #42 statically linking pyke's prebuilt ONNX Runtime,
which failed to release across the cargo-dist matrix (per-platform glibc /
libstdc++ / MSVC-CRT issues, and no macOS-Intel prebuilt). Switched to
load-dynamic + runtime provisioning so the release builds everywhere and the
runtime is fetched per-platform on demand.
