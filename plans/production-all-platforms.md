# Production hardening: all platforms just work

Goal: a person installs studio-worker on **Linux / macOS / Windows**, it
**builds/installs with no system-dep workarounds**, **prefers the UI by
default**, **auto-starts on login**, **auto-updates**, **auto-downloads
models**, and **ships all backends** so real jobs run without "engine not
packaged" errors.

## Findings from investigation (grounding)

- `ui` is opt-in; making it default is the explicit ask.
- The Linux `cargo install` failure is caused by:
  - **GTK/cairo/gdk/pango/atk** pulled in **only** by `tray-icon`
    (`libappindicator` + `muda`) and the explicit `gtk` dep. `eframe`,
    `rfd`, `notify-rust` are GTK-free.
  - **openssl-sys** pulled in by `reqwest` + `sentry` defaulting to
    `native-tls` (so even the *default* build needs libssl-dev).
- `eframe` (glow) + `notify-rust` (zbus) + `rfd` (ashpd/zbus) build via
  dlopen / pure-Rust, no pkg-config, no `-dev` packages.
- Engine routing: studio sends `ModelSource.engine` ∈ {`sd-cpp`,
  `llama-cpp`, `synthetic`}; `MultiEngine` routes strictly to the named
  backend. `image-candle` is unreachable for real offers.
- ggml conflict: `llama` (llama.cpp) + `whisper` (whisper.cpp) both
  static-link ggml and clash. **llama runs in-process; sd-cli runs as a
  subprocess** — deliberately sidestepping ggml conflicts. candle has no
  ggml, so `llama` + `image-candle` coexist.
- `llama-cpp-2` needs cmake + C/C++ toolchain at build time.
  cmake/clang/gcc/g++ present on dev box and CI runners; prebuilt
  release binaries need nothing at runtime.

## Key decisions (logged to DECISIONS.md)

1. **TLS = rustls** for reqwest + sentry → no OpenSSL build dep.
2. **Linux tray = ksni** (pure-Rust SNI), **mac/win tray = tray-icon**
   (native). Removes all GTK/cairo build deps on Linux. Tray stays
   best-effort; window UI works without it.
3. **`default = ["ui"]`** so `cargo install studio-worker` is UI-first
   and builds tool-light on any Linux.
4. **Release binaries ship all backends** via cargo-dist `features`
   (`llama` + media). The install script is the recommended turnkey path.
5. **Real image still needs sd-cli** (subprocess, avoids ggml clash);
   the worker auto-provisions it like a model + gives actionable errors.

## Phase 1 — builds & runs on any Linux, UI default (fully verifiable here)

- [x] Switch reqwest to rustls-tls-webpki-roots (drop native-tls/openssl).
- [x] Switch sentry to rustls transport (drop native-tls/openssl).
- [x] Restructure Cargo.toml tray deps: `tray-icon` mac/win only, add
      `ksni` linux only, drop the explicit `gtk` dep.
- [x] Make `tray` a submodule: keep pure-data (`TrayVariant`, menu ids,
      labels, `derive_variant`) shared; add per-platform backends
      (`tray_host`).
- [x] Implement Linux ksni tray backend (open/pause/quit + variant icon).
- [x] Keep tray-icon backend for mac/win behind the new abstraction.
- [x] Rework `ui/mod.rs::install_tray` + `app.rs::refresh_tray_variant`
      to drive the abstraction (no direct tray-icon types on Linux).
- [x] `default = ["ui"]` in Cargo.toml.
- [x] audit.toml: gtk-rs ignores **kept** (they are lock-resident via
      tray-icon's unused Linux backend; `cargo audit` reads the lock).
      Comment updated to reflect that nothing links them now.
- [x] Verify `cargo check` + `cargo build --release` succeed with no
      pkg-config/GTK on this box (default = ui).
- [x] `cargo fmt --check`, `cargo clippy --tests -- -D warnings`,
      `cargo test` (145) all green (default = ui now).
- [ ] Commit Phase 1.

## Phase 2 — backends packaged, models auto-download (verify build/tests)

- [x] Add an `all` convenience feature = ui + llama + image-candle +
      video + tts (the fully-loaded source build).
- [x] cargo-dist `[workspace.metadata.dist] features = [llama, video,
      tts]` + cmake system dep so release binaries ship llama + media
      (no whisper — ggml clash; image stays sd-cli subprocess).
- [x] Shared `engine::download` module (cache + length-verify + path
      traversal guard); sdcpp refactored onto it.
- [x] llama engine: download GGUF from `ModelSource` on demand via the
      shared module, advertise the `llama-cpp:*` wildcard (mirrors
      sd-cpp) so a fresh worker is claimable; `pick_gguf`/`as_llm` pure
      helpers, unit-tested.
- [x] Improve "no engine compiled" errors to name the install-script
      remedy (operator-actionable, no silent fallback).
- [x] sd-cli resolution: also check `<models_root>/bin`, `.exe` on
      Windows, and log an actionable skip when absent (no silent
      no-image worker).
- [x] Verified: `--features all` clippy-clean + llama tests (12);
      default fmt/clippy/test (274+) green.
- [ ] Commit Phase 2.
- [ ] (Deferred) sd-cli auto-download from a configured URL: needs a
      known-good per-platform binary source I can't validate here.

## Phase 3 — auto-start on login, all platforms

- [x] Windows autostart: real HKCU Run registry value via winreg (no
      console flash / admin / COM), replacing the marker file;
      enable/disable/is_enabled round-trip test gated to Windows.
- [x] First-run: `ui::run` reconciles autostart with `auto_start` via the
      pure `autostart::launch_sync_action` (enable / disable / noop),
      unit-tested for all four combinations.
- [x] Service install now registers the unit on macOS (launchctl load)
      + Windows (schtasks /Create) too, matching Linux's systemctl.
- [x] Tests: file backend round-trip + pure decision helpers green
      (441 tests, 0 failures); winreg path compiles on Windows.
- [x] Commit Phase 3.

## Phase 4 — release pipeline & CI for every platform

- [ ] cargo-dist: ensure runners install cmake (llama.cpp build) on all
      OSes; confirm targets list.
- [ ] CI checks matrix: default build is now ui+; ensure clippy/test on
      the fully-loaded feature set; keep free-tier (no GPU) green.
- [ ] Build matrix: build the default (ui) on win/mac/linux; build the
      `all` (or release) feature set where toolchain allows.
- [ ] Coverage: keep ≥90% with new modules excluded only where truly
      untestable (OS tray, registry).
- [ ] Commit.

## Phase 5 — integration tests + docs

- [ ] Integration tests: feature-gate matrix smoke, autostart round-trip,
      sd-cli/llama provisioning download contract (wiremock), tray data.
- [ ] README: install script as the turnkey path; `cargo install` =
      UI default; per-OS notes; remove the pkg-config workaround section.
- [ ] docs/ updates (engines, operations, autostart).
- [ ] PR_RESULTS.md deltas; AMBIGUITIES/DECISIONS updated.
- [ ] Final full verification pass; open PR.
