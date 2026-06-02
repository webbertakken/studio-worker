# Vulkan preflight + first-class Linux ARM / macOS Intel sd-cli

## Goals

1. Detect a missing Vulkan loader and surface a clear, actionable
   message (we can't auto-provision system drivers). No more cryptic
   sd-cli crash.
2. Make Linux ARM (aarch64) and macOS Intel (x86_64) first-class: ship
   our own prebuilt stable-diffusion.cpp binaries (upstream has none),
   so the provisioner downloads them like any other platform.

## Point 1 - Vulkan loader preflight

- [x] Detect the Vulkan loader without a heavy dep: `libvulkan.so.1`
      (Linux) / `vulkan-1.dll` (Windows) via `libloading::Library::new`.
      macOS uses Metal -> always OK.
- [x] `sd_provision::vulkan_runtime_status()` -> Ok / Err(actionable
      per-OS remedy message).
- [x] Preflight in `SdCppEngine::dispatch_image` before spawning sd-cli:
      WARN log + bail with the remedy when missing, so the operator sees
      exactly what to install instead of a linker crash.
- [x] Unit tests for the per-OS message + the macOS skip; verify real
      detection behaviour on this box.

## Point 2 - first-class Linux ARM + macOS Intel

- [x] Pin the sd.cpp source ref aligned to the upstream binary ref
      (`master-669-2d40a8b` -> commit `2d40a8b`); confirm cmake flags +
      target names by inspecting upstream + a local x86_64 Vulkan build.
- [x] `.github/workflows/sdcpp-prebuilt.yml`: native `ubuntu-24.04-arm`
      (Vulkan) + `macos-13` (Metal) build at the pinned ref, smoke-test
      `sd-cli --help`, package a zip mirroring upstream's layout,
      publish to a `sdcpp-prebuilt-<ref>` release.
- [x] Run the workflow; confirm green + assets published + smoke passes.
- [x] Provisioner: gap platforms (`linux/aarch64`, `macos/x86_64`)
      resolve to our hosted zip; covered platforms unchanged. Unit tests
      for the URL routing + asset names.
- [x] End-to-end verify a provision from the hosted asset (URL build +
      the workflow's own runner smoke test = the binary runs on-target).
- [x] Docs: sd-cli-install.md + engines/sdcpp.md - full platform matrix
      + the Vulkan-loader requirement and remedy.
- [x] Quality gate (fmt, clippy, check, test, audit) + release.
