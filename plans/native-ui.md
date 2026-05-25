# Native UI (egui / eframe)

Give the worker a Rust-native desktop window that shows the live job
queue, the current job in flight, the recent-job history, the rolling
log tail, and every `config.toml` field as an editable widget.

## Goals (must-haves before "done")

- New subcommand `studio-worker ui` opens a single window that runs the
  existing `run_loops` background tasks in-process and installs a
  system-tray icon.  Closing the window hides it to the tray; the
  worker keeps running.  Quitting (and signalling the same `stop`
  token the existing `run` path uses on Ctrl-C) happens through the
  tray menu.
- Tray icon reflects worker state via three variants (idle / busy /
  disconnected) and exposes: **Open Window**, **Pause / Resume
  claiming** (toggles `auto_enabled`), **Quit**.
- OS-native notifications (via `notify-rust`) on job completion and
  failure, each toggle-able independently from the Config tab.
- Tabs: **Status**, **Jobs**, **Config**, **Logs**, **About**.
- Every field on `Config` is reachable from the **Config** tab.  Edits
  persist via the existing `config::save` path and become visible to
  the next loop tick because all loops snapshot the shared
  `Arc<Mutex<Config>>` on each iteration.
- Live data: the current job (kind, model, prompt preview, elapsed
  time), the last N completed / failed jobs, the busy flag, the most
  recent heartbeat outcome, the rolling log buffer with level filter.
- Feature-gated behind `ui` cargo feature so headless `cargo install
  studio-worker` and the systemd service path stay free of GL / winit.
- All new code stays runnable without a display in tests: view-model
  unit tests + headless `egui::Context` frame tests, no eframe in CI.

## Open design forks (need user sign-off before code lands)

1. **Default value of the `ui` cargo feature**.
   - (A) **off** — headless install + service stay lean; desktop
     installer flips it on explicitly.  Recommended.
   - (B) **on** — `cargo install studio-worker` "just works" with a
     window everywhere, at the cost of egui / winit / glow deps even
     on headless rigs.
2. **First-run / unregistered behaviour**.  When `worker_id` /
   `auth_token` are missing:
   - (A) UI shows an in-window Register form (`bootstrap_token`,
     `api_base_url`, "Register" button calling the existing
     `runtime::register` code path).  Recommended.
   - (B) UI refuses to launch, prints a hint to run
     `studio-worker register` first.
3. **Config writes that need a runtime restart**.  The engine
   selection (`engine`, `engines`, `gradio_endpoint_url`) is consumed
   by `engine::build` at startup and not re-read on tick.  Options:
   - (A) UI exposes a "Restart worker loops" button alongside the
     engine fields, which cancels and re-spawns the loops in-process.
   - (B) Field is editable but a yellow banner tells the user a
     binary restart is required.
4. **Service-attached mode** (v2 scope).  When a systemd / launchd
   service is already running and the user launches `studio-worker
   ui`, v1 will simply refuse with a clear message ("a worker is
   already running as a service").  Cross-process IPC to attach the
   UI to a service-managed worker is deferred.
5. **Tray notifications default**: which event triggers an OS
   notification by default?
   - (A) Neither — user opts in per-event in Config.  *(my pick)*
   - (B) Failures only.
   - (C) Both.
6. **Close-button semantics with tray active**:
   - (A) Close → hide to tray, loops keep running, Quit via tray.
     *(my pick — standard tray convention)*
   - (B) Close → quit entirely, tray is purely a status indicator.
7. **Autostart-on-login toggle in Config tab**.
   - (A) Yes — toggle that registers `studio-worker ui` as the
     desktop autostart entry, coexisting with the existing
     `install-service` flow (the two serve different deployments).
     *(my pick)*
   - (B) No — v1 only exposes window + tray; autostart stays a CLI
     responsibility.

## Existing surface the UI reuses (no rewrites)

- `Arc<Mutex<Config>>` — already shared across loops.
- `Arc<Mutex<Vec<LogEntry>>>` — already drained by `log_shipper_tick`;
  the UI subscribes to the same buffer (read-only).
- `Arc<AtomicBool> busy` — already flipped by `claim_tick` around the
  in-flight job.  UI renders idle / busy from this.
- `runtime::register`, `runtime::set_enabled`, `runtime::set_threshold`,
  `runtime::format_status`, `runtime::format_check_outcome` — invoked
  from button handlers / About tab; no new business logic in the UI
  layer.

## New shared state the UI needs (added to `runtime`)

- `Arc<Mutex<Option<CurrentJob>>>` — populated by `claim_tick` on
  successful claim, cleared on completion / failure.
- `Arc<Mutex<VecDeque<RecentJob>>>` — bounded ring (default 50) of
  finished jobs with outcome + duration.
- `Arc<Mutex<Option<HeartbeatStatus>>>` — last heartbeat outcome +
  timestamp, populated by `heartbeat_tick`.

These are added behind the same in-process plumbing the existing loops
use; no wire-format change, no API change.

---

## Phase 1 — Plumb the new shared state into the runtime (no UI yet)

- [x] Add `CurrentJob`, `RecentJob`, `HeartbeatStatus` types to
      `src/runtime.rs` (or a new `src/runtime/state.rs` if size
      warrants).  Each derives `Clone + Debug`.  No serde unless we
      end up exposing them over IPC.
- [x] Failing unit test: a fake `claim_tick` invocation that succeeds
      populates `current_job` for the duration of the dispatch, then
      moves the entry into `recent_jobs` with the right outcome
      (`Completed`).
- [x] Failing unit test: a fake `claim_tick` that fails moves the
      entry into `recent_jobs` with outcome `Failed { reason }`.
- [x] Failing unit test: `heartbeat_tick` writes the most recent
      outcome (`Ok` / `Err(reason)`) + timestamp into
      `last_heartbeat`.
- [x] Implement the three state slots and thread them through
      `run_loops`, `spawn_heartbeat`, `spawn_claim_loop`.  Keep the
      existing `Arc<Mutex<…>>` style — no new sync primitive.
- [x] Tests green; commit "feat(runtime): expose current-job / recent
      / heartbeat state for the UI".

## Phase 2 — Cargo feature + subcommand wiring

- [ ] Add `ui` feature in `Cargo.toml` with `egui` and `eframe`
      (`default-features = false`, `glow` backend) as optional deps.
      Default of the feature is the answer to **fork #1** above.
- [ ] Failing CLI parse test: `studio-worker ui` parses into a new
      `Command::Ui` variant.  Test compiles with and without the
      feature (subcommand is always parseable; only the dispatch is
      gated).
- [ ] Add `Command::Ui` to `src/cli.rs` and dispatch in
      `lib::run_cli`.  When the feature is off, dispatch prints a
      friendly "this binary was built without the `ui` feature; run
      `cargo install studio-worker --features ui` or use the desktop
      installer" message and exits non-zero.
- [ ] Tests green; commit "feat(cli): add `ui` subcommand stub behind
      cargo feature".

## Phase 3 — App skeleton + tab shell

- [ ] New module `src/ui/mod.rs` (gated `#[cfg(feature = "ui")]`).
      Exports `pub fn run(config_path: Option<&str>) -> Result<()>`
      that loads config, spawns the runtime loops on a background
      tokio task, and hands control to eframe on the main thread.
- [ ] Headless test: instantiate the eframe `App` with mock shared
      state, drive one frame via `egui::Context::run`, assert the
      five tab labels are present (egui exposes its widget tree via
      `Context::accesskit_node_builders` or similar — pick whatever
      is stable on the egui version we pin).
- [ ] Implement the tab shell (top tab bar + central panel) wired to
      a `Tab` enum.  Default tab on startup: **Status**.
- [ ] Tests green; commit "feat(ui): tab shell + Status placeholder".

## Phase 4 — Status tab

- [ ] Failing test: status-tab view model formats `worker_id`,
      `api_base_url`, busy flag, last heartbeat, VRAM (probed via
      `sys::detect_vram_gb`), threshold.  When `worker_id` is `None`
      the view model carries an `Unregistered` variant.
- [ ] Failing test: a render with `Unregistered` state shows a
      "Register…" button; **fork #2** determines what clicking it
      does (modal form vs. error message).
- [ ] Implement Status tab using the view model.
- [ ] Tests green; commit "feat(ui): Status tab".

## Phase 5 — Jobs tab

- [ ] Failing test: with `current_job = Some(…)` and three
      `recent_jobs` entries the view model produces one
      "Current" card + three "Recent" rows in chronological order
      (newest first).
- [ ] Failing test: elapsed time formatter renders sub-minute as
      `12s`, sub-hour as `3m 04s`, longer as `1h 12m`.
- [ ] Implement Jobs tab (Current card with prompt preview + kind +
      model + elapsed; Recent list with outcome icon, duration,
      finished-at).
- [ ] Tests green; commit "feat(ui): Jobs tab".

## Phase 6 — Config tab

- [ ] Failing test: editing `vram_threshold_gb` via the view model
      and pressing **Save** writes a config.toml on disk whose
      `vram_threshold_gb` matches the new value.  Use `tempdir` +
      `--config` override (same pattern as `tests/config_tracing.rs`).
- [ ] Failing test: editing `engine` to a new value flips a "restart
      required" flag in the view model (per **fork #3**).
- [ ] Failing test: editing `bootstrap_token` is supported but the
      widget masks the value by default (no plaintext leak in
      screenshots).
- [ ] Implement Config tab: one widget per `Config` field, grouped
      into sections (Connection / Worker / Engine / Auto-update /
      Models).  Save button calls `config::save`.  Reset button
      reloads from disk.
- [ ] Tests green; commit "feat(ui): Config tab with full
      read / write coverage".

## Phase 7 — Logs tab

- [ ] Failing test: with a 1 000-entry log buffer the view model
      windows to the last 500 by default and supports a level filter
      (`info` / `warn` / `error`).
- [ ] Failing test: when "Auto-scroll" is on, the view model reports
      `scroll_to_end = true` on every render.  Toggling it off
      preserves position.
- [ ] Implement Logs tab using a `egui::ScrollArea` with virtualised
      rows, level filter combo, free-text search box, auto-scroll
      toggle, "Copy all visible" button.
- [ ] Tests green; commit "feat(ui): Logs tab with filter + search".

## Phase 8 — About tab

- [ ] Failing test: About view model carries `AGENT_VERSION`,
      `RELEASE_NAME`, the resolved config path, and the last
      `update::check_update` outcome (via `format_check_outcome`).
- [ ] Implement About tab: version + release + config path +
      "Check for updates now" button (calls `runtime::check_update`)
      + "Open log file" / "Open config file" buttons that launch the
      platform-native opener.
- [ ] Tests green; commit "feat(ui): About tab".

## Phase 9 — Window lifecycle (hide-to-tray) + graceful shutdown

- [ ] Failing test: closing the window with tray active flips the
      app's `window_visible` flag to `false` without touching the
      `stop` token — background loops keep ticking.
- [ ] Failing test: invoking `Quit` from the tray menu (simulated via
      the same channel the tray callbacks use) cancels the `stop`
      token and the tokio runtime drains cleanly within 5s (use
      `tokio::time::timeout`).
- [ ] Failing test: an in-flight job at quit time is awaited up to
      the same 5s budget, then forcibly cancelled with a warn log.
- [ ] Implement `on_close_event` (hide-to-tray) and the tray-Quit
      handler (signal `stop`, await loops, drop the eframe app).
- [ ] Tests green; commit "feat(ui): hide-to-tray + graceful shutdown".

## Phase 10 — Tray icon + notifications

- [ ] Add `tray-icon` and `notify-rust` to the `ui` feature.  Bundle
      three SVG / PNG icon variants (idle / busy / disconnected) in
      `assets/tray/`.
- [ ] Failing unit test: tray view model derives the icon variant
      from `(busy, last_heartbeat_ok)` — idle when not busy + last
      heartbeat ok, busy when `busy=true`, disconnected when last
      heartbeat failed or older than `3 × heartbeat_interval`.
- [ ] Failing unit test: tray menu factory produces the right
      labels (`Open Window`, `Pause claiming` ↔ `Resume claiming`
      based on `auto_enabled`, `Quit`).
- [ ] Failing unit test: notification gate — with both toggles off,
      a fake claim-tick completion does not emit a notification;
      with the completion toggle on it emits exactly one with the
      job kind + model in the body.
- [ ] Implement the tray:
      - spawn `tray-icon` on the same winit event loop eframe owns
        (use the documented `with_user_event` integration or run the
        tray on a parallel thread that posts custom user events back
        into eframe);
      - swap the icon when state transitions;
      - hook **Open Window** to `window_visible = true`;
      - hook **Pause / Resume** to `runtime::set_enabled`;
      - hook **Quit** to the shutdown path defined in Phase 9.
- [ ] Implement `notify-rust` calls in `claim_tick`'s success /
      failure terminals, gated on the two Config toggles.  Notifier
      lives behind a thin trait so tests can inject a fake.
- [ ] Linux note: document `libxdo-dev` + `libayatana-appindicator3-dev`
      as build-time deps in README + checks.yml.
- [ ] Tests green; commit "feat(ui): tray icon + completion / failure
      notifications".

## Phase 11 — Autostart-on-login (Config tab toggle)

- [ ] Failing test: enabling the autostart toggle on Linux writes a
      `~/.config/autostart/studio-worker-ui.desktop` entry whose
      `Exec=` line invokes `studio-worker ui` with the current
      binary path; disabling removes it.
- [ ] Failing test: same behaviour on macOS (LaunchAgent plist) and
      Windows (Run-key registry entry), reusing the existing
      `service.rs` plumbing patterns where possible.
- [ ] Implement the autostart writer (separate from
      `service::install` because that still owns systemd / launchd
      / Scheduled-Task lifecycles — different artefacts).
- [ ] Surface the toggle in the Config tab under a "Background mode"
      group with a one-line explainer ("Run in tray on login").
- [ ] Tests green; commit "feat(ui): autostart-on-login toggle".

## Phase 12 — CI + docs

- [ ] `.github/workflows/checks.yml`: add `cargo build --features ui`
      to the matrix and `apt-get install` the Linux tray build deps
      (`libxdo-dev`, `libayatana-appindicator3-dev`,
      `libgtk-3-dev`).  Skip `cargo test --features ui` if it would
      need a display; the headless `egui::Context` tests live behind
      the same feature so they need explicit thought — likely
      runnable without a display via `xvfb-run` if it comes to it.
- [ ] `.github/workflows/build.yml`: confirm the release matrix builds
      `--features ui` for the desktop targets.  Headless targets keep
      the default-features build.
- [ ] `cargo-dist` config in `Cargo.toml`: ensure the installer
      ships the `--features ui` build for desktop targets.
- [ ] README: new "Desktop UI" section with screenshot placeholder,
      `studio-worker ui` invocation, tray-icon behaviour notes, and
      the feature-flag note.
- [ ] `AGENTS.md` Tech stack table: add `egui` + `eframe` +
      `tray-icon` + `notify-rust` rows.
- [ ] Tick the box, commit "docs: native UI usage + screenshot
      placeholder".

## Non-goals (explicitly out of scope)

- IPC between a service-managed worker and an attached UI (see
  **fork #4** + `AMBIGUITIES.md`).
- Per-job artefact preview inside the UI (image thumbnails, audio
  scrub bar).  The studio's React dashboard owns that surface.
- Theming beyond egui's built-in dark mode (default-on, per project
  design rules).
- A taskbar / dock badge with job count (separate API from tray,
  doable later via `tray-icon`'s extended surface or platform-native
  calls).
- Click-through tray actions for per-modality pausing (e.g. pause
  only video jobs).  v1 pauses all claiming via the single
  `auto_enabled` flag.
