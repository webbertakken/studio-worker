# Auto-provision sd-cli so image gen works out of the box

## Problem

`SdCppEngine::try_new` only registers when the `sd-cli`
(stable-diffusion.cpp) binary is already installed. A fresh Windows
prod install has the worker but not `sd-cli`, so every image / inpaint
job fails with "no sdcpp engine compiled into this worker". Model
weights already download on demand; the binary does not. It must be
auto-provisioned.

## Design

- Lazily download + extract the platform's stable-diffusion.cpp Vulkan
  build (universal across NVIDIA / AMD / Intel, ~37 MB) into
  `<models_root>/bin/` on first image job, then cache.
- Pin a known-good upstream release for reproducibility; allow override
  via `STUDIO_WORKER_SDCPP_RELEASE` (tag) and `STUDIO_WORKER_SDCPP_URL`
  (full zip URL, for tests / air-gapped installs).
- Windows finds the sibling DLL automatically; Linux/macOS get
  `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH` set to the binary's dir at
  invocation when a sibling shared library is present.
- The engine registers unconditionally now (it can provision on
  demand); resolution order (env / models_root/bin / ~/.local/bin /
  PATH) is unchanged and still wins over provisioning.

## Tasks

- [x] Add `zip` (minimal deflate feature, pure-Rust miniz_oxide) dep.
- [x] `src/engine/sd_provision.rs`: pure helpers + unit tests
      (asset-for-target, sha-from-tag, download URL, zip extraction,
      install-into-dir, binary/library names, library-path env).
- [x] `provision(models_root)` orchestrator: download pinned/overridden
      zip via `download::download_file`, extract, install, return the
      sd-cli path.
- [x] Rework `SdCppEngine`: always register; resolve-or-provision
      `sd-cli` lazily (`ensure_sd_cli`), cache it; set the library-path
      env on the per-job `Command`. TDD.
- [x] `mod.rs` `build()`: register sdcpp unconditionally (provision on
      demand); update the roster-breadcrumb comment + test note.
- [x] Integration test: `provision` against a wiremock-served fake zip
      (end-to-end, no GPU, free-tier CI safe).
- [x] Update docs (`sd-cli-install.md`, `engines/sdcpp.md`) for
      auto-provisioning; manual install stays as override path.
- [x] Quality gate: `cargo fmt --check`, `cargo clippy --tests -D
      warnings`, `cargo check`, `cargo test` all green.
