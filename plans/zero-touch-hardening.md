# Zero-touch hardening: security, architecture, ease of use, CI

Findings from a deep repo review (2026-07-01) measured against the
product goal: **a user installs the worker once and never touches it
again**. The studio admin approves it; from then on it downloads its
own models, provisions its own runtimes/SDKs per platform, serves
studio jobs, and is also callable locally through its own
localhost API. The studio keeps adding models from Hugging Face and
everything keeps working out of the box.

Every task below names the evidence in code. Work each task with TDD
(red → green → commit → refactor → commit). Tick a checkbox the moment
its task is done — never batch ticks. Keep PR titles ≤ 52 chars,
conventional commits. All tests must pass on free-tier GitHub Actions
(no GPU, wiremock for the studio).

Execution note: tasks are grouped in phases by priority. Within a
phase, tasks are independent unless a dependency is called out.

---

## Phase 1 — security (highest priority)

### 1.1 Local API: authentication + CSRF/DNS-rebinding defence

The local API (`src/local_api.rs`) binds `127.0.0.1:4787` with **no
auth, no Origin check, no Host check**. A malicious web page can fire
`fetch("http://127.0.0.1:4787/models", {method:"POST", body: json,
mode:"no-cors"})` — a `text/plain` simple request needs no CORS
preflight, and the handler parses the body as JSON regardless of
content type. That lets any website: (a) inject a catalog model with an
attacker-controlled download URL (`POST /models`), (b) delete models,
(c) burn the GPU via `POST /image`, (d) with DNS rebinding, also *read*
responses (prompts in `/jobs`, catalog contents). Any other local OS
user can do all of the above trivially.

- [x] 1.1.1 Generate a per-install local API token (32 random bytes hex
      via the existing `getrandom` path in `auto_register.rs`; extract
      `rand_bytes`/`new_secret_hex` into a shared helper). Persist it in
      `config.toml` (internal field, redacted from logs like
      `auth_token`) so it survives restarts. TDD: config round-trip +
      redaction tests mirroring `config_tracing.rs`.
- [x] 1.1.2 Require `Authorization: Bearer <token>` on every route
      except `GET /healthz`. Wrong/missing token → 401 with a
      one-line remedy pointing at the discovery file (1.1.4). TDD:
      table-driven tests per route × {no header, wrong token, good
      token}.
- [x] 1.1.3 Reject requests whose `Host` header is not
      `127.0.0.1[:port]`, `localhost[:port]`, or `[::1][:port]`
      (DNS-rebinding guard), and reject any request carrying an
      `Origin` header that is not a loopback origin (CSRF guard;
      absent `Origin` = non-browser client = allowed). TDD: unit tests
      for the pure validators + integration tests through the bound
      server.
- [x] 1.1.4 Write a discovery file `<config_dir>/local-api.json`
      (owner-only 0600, atomic write reusing `config::write_atomic` —
      make it `pub(crate)`) containing `{ "url": ..., "token": ... }`
      on every successful bind; remove it on clean shutdown. This is
      how local clients find the port (which can be ephemeral after
      the port-0 fallback in `runtime::spawn_local_api`). TDD: bind →
      file exists with correct mode + contents; shutdown → gone.
- [x] 1.1.5 Update `README.md` and `docs/local-api.md`: curl
      examples gain the token header, document the discovery file and
      the threat model (why the token exists).

### 1.2 Local API: request-body and robustness limits

- [x] 1.2.1 `read_body` (`src/local_api.rs`) reads to string unbounded
      — a request can OOM the worker. Cap at 1 MiB (413 on overflow),
      using `Request::body_length()` when present plus a hard cap on
      the reader. TDD: oversized body → 413, boundary body → 200.
- [x] 1.2.2 `STUDIO_WORKER_LOCAL_API_PORT` parse failures fall back
      silently (`.ok().and_then(...)` in `runtime::spawn_local_api`).
      Warn-log the invalid value + the fallback port. Also add
      `local_api_port` as a proper optional `Config` field (env var
      wins for compatibility). TDD: capture-based tracing test like
      `runtime_startup_tracing.rs`.

### 1.3 Model download integrity + transport

- [x] 1.3.1 `engine::download::download_file_verified` accepts any URL
      scheme — a plain-`http` model URL is silently fetched and is
      MITM-poisonable. Enforce `https` (loopback `http` allowed for
      wiremock tests), mirroring `update::validate_installer_download_url`.
      Extract one shared validator used by both. TDD: http remote URL
      → clear error before any request; loopback http still works in
      `engine_download.rs`.
- [x] 1.3.2 The seeded catalog (`src/catalog.rs::zimage_turbo`) ships
      `sha256: None` for all three files — the out-of-the-box model
      downloads have zero integrity pinning. Compute the real sha256
      of the three published HF files and pin them in the seed. TDD:
      seed test asserts every file carries a 64-hex sha256.
- [x] 1.3.3 Add a disk-space preflight to `ensure_file`: when
      `approx_bytes` is known and the free space on `models_root`'s
      filesystem is below `approx_bytes` + 10% headroom, fail fast
      with an actionable message (needed vs available) instead of
      streaming gigabytes into ENOSPC. Use a mockable free-space seam
      so it's unit-testable. TDD: injected low free space → error
      naming both numbers; unknown `approx_bytes` → no preflight.
- [x] 1.3.4 Resume interrupted downloads: on start, if `<dest>.part`
      exists and the server supports ranges (probe via `Accept-Ranges`
      on the GET response / a HEAD first), resume with a `Range`
      header instead of restarting a multi-GiB fetch; hash-verify the
      complete file after assembly (hash the pre-existing prefix
      first). If ranges are unsupported, truncate and restart as
      today. TDD: wiremock serving 206 with a Range assertion.

### 1.4 Auto-update supply-chain hardening

`update::RealRunner::download` verifies only Content-Length, then
hands the file to `sh` / `powershell`. A compromised release (or a
tampered feed) is remote code execution on every worker.

- [x] 1.4.1 Verify the installer's sha256 against the checksum asset
      cargo-dist publishes in the same release (each artifact has a
      sibling `.sha256`; confirm exact naming from a real release via
      `gh release view`). Download checksum + installer, verify before
      `run_installer`. TDD: wiremock feed + assets; mismatch → error,
      installer never executed (assert via fake runner).
- [x] 1.4.2 Pin allowed installer/checksum download hosts to
      `github.com` and `objects.githubusercontent.com` (still https),
      on top of the existing scheme check, so a poisoned feed can't
      redirect the download elsewhere. Keep the loopback-http test
      escape hatch. TDD: https non-GitHub host → rejected.
- [ ] 1.4.3 (Stretch, separate PR — deferred: checksum verification + host pinning landed in PR chunk 3; minisign left for a follow-up) Sign releases with minisign in
      `release.yml` (key in repo secrets, public key baked into the
      binary) and verify the signature before executing the installer.
      Fall back to checksum-only with a warn when the release predates
      signing. Skip if effort exceeds a day — 1.4.1/1.4.2 are the
      bulk of the win.

### 1.5 Catalog persistence safety

- [x] 1.5.1 `Catalog::save` uses plain `std::fs::write` — non-atomic; a
      crash mid-write corrupts `models.json`. Reuse the atomic
      temp-file writer from `config.rs`. TDD: mirror
      `save_atomically_replaces_existing_config_without_temp_litter`.
- [x] 1.5.2 Silent data loss: when `models.json` is corrupt,
      `runtime::spawn_local_api` falls back to `Catalog::seed()` while
      keeping `catalog_path` — the next `POST /models` persists the
      seed **over the user's file**. Instead: quarantine the corrupt
      file (rename to `models.json.corrupt-<ts>`), warn-log with the
      quarantine path, then seed fresh. TDD: corrupt file → renamed
      aside, seed written, warn emitted, original bytes preserved.

---

## Phase 2 — architecture correctness

### 2.1 One GPU, one job: shared job gate

The WS session guards concurrency with the `busy` CAS
(`ws::session::try_reserve_worker`) but the local API
(`local::run_image`) never touches it — a local job and a studio job
run concurrently on the same GPU and OOM each other. The auto-updater
also only *reads* `busy` once before a minutes-long download/install
(`runtime::auto_update_tick`), so a job accepted mid-apply is killed by
`restart_self()`.

- [x] 2.1.1 Introduce a `JobGate` (wrap the existing
      `Arc<AtomicBool> busy` in a small struct with
      `try_reserve() -> Option<Guard>`; RAII release). Move
      `try_reserve_worker` into it. TDD: contention tests incl. drop
      releasing the slot and panic-safety.
- [x] 2.1.2 Wire the gate into the local API: `POST /image` reserves
      before dispatch; when busy, respond `503` with `Retry-After` and
      a JSON body naming the current job kind (studio vs local). Pass
      the shared `busy` from `runtime::run` / `ui::run` into
      `spawn_local_api`. TDD: hold the gate, POST → 503; release →
      200.
- [x] 2.1.3 Wire the gate into the auto-updater: reserve **before**
      `update::apply` (offers arriving mid-install are rejected as
      busy, exactly like a job) and only `restart_self` while holding
      the reservation. TDD: updater holding the gate → simultaneous
      offer rejected (extend `runtime_ticks.rs` / session tests).

### 2.2 Local API responsiveness

`LocalApi::serve` is a single loop calling `route` synchronously — one
image generation (~10 s) or a first-use model download (minutes,
multi-GiB) blocks `/healthz`, `/models`, `/jobs` and every other
caller.

- [x] 2.2.1 Dispatch each request on a small thread pool (e.g. 4
      threads; `tiny_http::Server` is `Sync`, requests are `Send` —
      the standard tiny_http pattern of N worker threads looping on
      `server.recv_timeout`). Generation stays synchronous per
      request; cheap routes stay responsive. TDD: start a slow
      generation (synthetic engine + injected sleep or a blocking test
      engine), assert `/healthz` answers concurrently.
- [x] 2.2.2 Make `/healthz` honest: include `version`,
      `registrationState` (from `SharedRegistration`), `engines`
      roster, `busy`, and models-root free space. Keep it
      unauthenticated but read-only-minimal (no prompts, no token).
      TDD: assert fields present + no secret material in the body.

### 2.3 Session resilience (kills the zombie-UI state)

`DEFAULT_RECONNECT_ATTEMPTS = 5`: after 5 consecutive failed connects
`spawn_ws_session` returns `Err`. In `run` mode the process exits
(service manager restarts it — fine). In `ui` mode (`src/ui/mod.rs`
~95) the error is only logged: the window stays open with a
permanently dead session. A laptop that sleeps through wifi loss wakes
up as a zombie worker. That breaks "never touch it again".

- [x] 2.3.1 Default to infinite reconnects with the existing capped
      backoff (`ws_reconnect_attempts: 0` semantics) for **both**
      modes; keep the config knob for operators who want fail-fast
      under a service manager. Adjust docs + `config` default comment.
      TDD: session test asserting attempts keep going past 5 with
      backoff capped.
- [x] 2.3.2 Surface connection state in the UI: `WorkerObservers`
      gains a `session_state: Arc<Mutex<SessionState>>`
      (Connecting/Connected/Backoff{until}/AuthFailed) written by the
      session loop; Status tab renders it. AuthFailed stays terminal
      but must show a visible call-to-action in the UI (re-register)
      instead of a dead silence. TDD: observer transitions in
      `runtime_observers.rs`-style tests.

### 2.4 Registration polish

- [x] 2.4.1 `Pending.since` is reset to `Utc::now()` on every poll
      (`auto_register::poll_existing`), so "pending since" lies.
      Preserve the first-seen instant: read the previous state from
      `observers` and keep its `since` when already Pending. TDD:
      two consecutive pending polls → same `since`.
- [x] 2.4.2 First-run VRAM-aware default threshold: `Config::default`
      hardcodes `vram_threshold_gb: 12.0`; on an 8 GB card the worker
      over-advertises and OOMs (currently only a warning at
      handshake). When bootstrapping a fresh config (the
      `default_created` path in `config::load`), probe
      `sys::detect_vram_gb()` and clamp the initial threshold to
      detected VRAM when the probe returns > 0. Never touch existing
      configs. TDD: injectable probe seam; fresh config on a "6 GB"
      box → 6.0; probe 0 → stays 12.0.

### 2.5 Models-root layout (collision safety)

- [x] 2.5.1 Flat `models_root` collides filenames across models
      (documented in `docs/engines/sdcpp.md`). Move downloads to
      per-model subdirs: `<models_root>/<sanitised-model-id>/<file>`
      with a `manifest.json` (url, sha256, bytes, downloaded-at) per
      dir. Keep a read-fallback to the legacy flat path so existing
      caches aren't re-downloaded (check subdir first, then flat).
      Update `ensure_files` in the sdcpp/onnx/llama engines to pass
      the model id. TDD: two models with the same filename coexist;
      legacy flat file is found and not re-downloaded.

---

## Phase 3 — ease of use / zero-touch gaps

### 3.1 Turnkey install finish line

After `curl … installer.sh | sh` the user still has to know to run
`studio-worker ui` and optionally `install-service`. Zero-touch means
the install ends with a running, auto-starting worker.

- [x] 3.1.1 Add `studio-worker setup`: one-shot that (a) installs the
      OS autostart (service on headless/`--headless`, autostart entry
      + tray UI otherwise), (b) starts the worker now, (c) prints the
      approval hint (studio URL + machine name the admin will see) and
      the local API URL. Idempotent — safe to re-run. TDD: drive
      through the `ServiceOps` seam like `service.rs` tests.
- [x] 3.1.2 Document `setup` as **the** post-install step in README's
      quick-install section (one copy-paste block per OS: install +
      setup). Explore cargo-dist's installer hooks (`install-success-msg`
      or postinstall) to at least print "now run: studio-worker setup"
      at the end of the shell/PS installers; use it if supported at
      our pinned cargo-dist 0.30.4.

### 3.2 GPU runtime provisioning ("install missing SDKs")

- [x] 3.2.1 Vulkan-loader preflight currently happens on the **first
      image job** (`sd_provision` / `sdcpp.rs`), failing a real user
      job. Move a non-fatal probe to startup: run the loader check at
      engine build, log + expose the result in `WorkerObservers` (and
      `/healthz` runtime block from 2.2.2, plus the UI Status tab)
      with the exact per-distro remedy (`apt install libvulkan1`,
      etc.). Job-time behaviour unchanged. TDD: probe seam injected,
      status text asserted.
- [ ] 3.2.2 Bundle `libvulkan.so.1` into **our** `sdcpp-prebuilt`
      Linux zips (loader is Apache-2.0 — redistributable; confirm
      licence file included) and extend `sd_provision` to extract it
      next to `sd-cli` (LD_LIBRARY_PATH already points there). Result:
      image generation works on a fresh Linux box with only a GPU
      driver installed, no root package needed. Windows drivers ship
      `vulkan-1.dll` already; macOS uses Metal (no-op). Requires a
      `sdcpp-prebuilt.yml` change + re-run and a fallback when the
      asset predates bundling. TDD: provisioner extracts the extra lib
      from a fixture zip; loader preflight passes when the bundled lib
      is present (point the probe at it).

### 3.3 Windows LLM parity

`llama-cpp-2` is compiled out on Windows (MSVC CRT clash, documented in
`Cargo.toml`), so Windows workers silently can't serve LLM jobs —
platform parity broken for the turnkey story.

- [ ] 3.3.1 Add a subprocess llama engine for Windows mirroring the
      sd-cli pattern: auto-provision the official
      `llama.cpp` release binary (`llama-server` or `llama-cli`,
      pinned tag, Vulkan build) into `<models_root>/bin/`, drive
      chat-completions through it, register it in `engine::build` on
      Windows only. Reuses `download.rs` + the zip extraction helpers.
      TDD: provisioning against a wiremock release zip (mirror
      `sd_provision.rs` tests); engine tests with a fake binary.
      This is the biggest single task in the plan — timebox it; if the
      subprocess contract turns out unstable, ship the provisioning +
      a clear "LLM via llama-server" doc and revisit.

### 3.4 VRAM detection beyond NVIDIA

`sys::detect_vram_gb` probes NVIDIA sysfs + `nvidia-smi` only; AMD,
Intel and Apple report 0.0, which disables the threshold sanity warning
and blinds studio matching.

- [x] 3.4.1 Add an AMD probe: sum
      `/sys/class/drm/card*/device/mem_info_vram_total` (bytes) on
      Linux. TDD: fixture sysfs tree like the existing NVIDIA sysfs
      tests.
- [x] 3.4.2 Add an Apple probe: unified memory via
      `sysctl hw.memsize` scaled (document the heuristic — e.g. 75% of
      unified memory usable as VRAM); macOS only. TDD: parse seam.
- [x] 3.4.3 Windows non-NVIDIA: query
      `Win32_VideoController.AdapterRAM` via `wmic`/PowerShell CIM as
      a fallback after `nvidia-smi`. TDD: stdout-parser seam.

### 3.5 Local API becomes the generic worker API

Only `POST /image` exists. The goal says the worker is generically
callable locally for whatever engines are compiled in.

- [x] 3.5.1 `POST /v1/chat/completions` (OpenAI-compatible subset:
      `model`, `messages`, `max_tokens`, `temperature`; non-streaming
      first) routed to the llama engine via the catalog; 501 with a
      clear message when no LLM engine is compiled in. Reuses the
      shared `JobGate`. TDD: synthetic-engine round-trip asserting the
      OpenAI response shape.
- [x] 3.5.2 `POST /tts` (text → audio) and `POST /stt` (audio →
      text) mirroring the studio task params, gated on compiled
      engines; `POST /video` likewise. Keep each endpoint thin —
      resolve catalog model, build `Task`, dispatch, map result. TDD:
      per-kind round-trips through `SyntheticEngine` (mirrors
      `multi_modal.rs`).
- [x] 3.5.3 Catalog: add per-kind defaults (`default_image_model` →
      `default_model_for(kind)`) and seed sensible entries for kinds
      whose engines self-provision. TDD: default resolution per kind.
- [x] 3.5.4 Update `docs/local-api.md` → `docs/local-api.md`
      covering all endpoints, auth, discovery file, and the busy/503
      contract; link from README.

### 3.6 Studio models flow into the local catalog

Models the studio admin adds from Hugging Face reach the worker only
inside job offers; the local catalog never learns them, so local API
users can't use studio-registered models.

- [x] 3.6.1 On every offer carrying a `ModelSource`, upsert a
      catalog entry (id = offered model id, `origin: "studio"` — new
      optional field, default `"local"`) unless a local entry with the
      same id exists. Persist via the atomic save (1.5.1). Studio
      re-offers refresh studio-origin entries; local edits to
      local-origin entries are never clobbered. TDD: session-level
      test (extend `ws_session_full_loop.rs`) asserting the catalog
      file gains the offered model; local-origin conflict preserved.

---

## Phase 4 — CI + release integrity

- [ ] 4.1 Tests run only on `ubuntu-latest` (`checks.yml`), yet the
      worker ships platform-specific logic on three OSes (winreg
      autostart, scheduled-task XML, launchd plist, `ExeReplaceGuard`
      park/rename semantics on a real NTFS, path handling). Add
      `windows-latest` and `macos-latest` test legs running
      `cargo test --no-default-features` plus the cheap feature sets
      (skip the heavy candle/whisper legs there to stay inside the
      free tier; measure minutes before/after). Fix whatever falls
      out.
- [ ] 4.2 Add a pinned-release drift check: a tiny CI job (or a step
      in `checks.yml`) that asserts the `sdcpp-prebuilt-<ref>` release
      matching `DEFAULT_RELEASE_TAG` in `sd_provision.rs` exists and
      contains the linux-arm64 asset (plus, after 3.2.2, the bundled
      loader), so a bumped pin without a re-run of
      `sdcpp-prebuilt.yml` fails in CI instead of 404ing on user
      machines. Same idea for the pinned `ORT_VERSION` asset names in
      `onnx_provision.rs` (a HEAD against the Microsoft release URL).
      Network-dependent → run on a schedule + on changes to the
      pinned constants, not on every PR.
- [ ] 4.3 After 1.4.1: add a release-time assertion in `release.yml`
      that checksum assets are present for every installer artifact
      (fail the release, not the updater in the field).
- [ ] 4.4 Add `cargo-deny` (licence + duplicate-major gate) to the
      weekly audit workflow — low effort, catches licence drift in the
      big engine dep trees. Keep advisories in `cargo-audit` as-is.

---

## Decisions taken (recorded for the executor)

- Local API auth = bearer token + Host/Origin validation, discovery
  file for clients. mTLS/unix-socket rejected as overkill for
  loopback.
- Reconnects default to infinite: a zero-touch worker must never
  permanently give up while unattended; fail-fast stays available via
  `ws_reconnect_attempts`.
- Windows LLM via subprocess llama.cpp release binaries (mirrors the
  proven sd-cli pattern) rather than fighting the MSVC CRT link.
- Vulkan loader gets bundled in our own prebuilt zips (Apache-2.0)
  instead of asking users to `apt install` — that's the "install
  missing SDKs" goal, done the only way that works without root.
- OpenAI-compatible chat endpoint chosen so existing local tooling
  (editors, scripts, LangChain-style clients) can point at the worker
  with zero glue.

## Explicitly out of scope

- `sd-server` persistent-process amortisation (perf, tracked in
  `docs/engines/sdcpp.md`).
- Streaming responses on the local API (follow-up once 3.5 lands).
- Studio-side changes (registry sha256 columns, rate limits) — worker
  consumes them when present (`verify_sha256` already does).
