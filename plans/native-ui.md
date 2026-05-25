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
   - (A) **off** - headless install + service stay lean; desktop
     installer flips it on explicitly.  Recommended.
   - (B) **on** - `cargo install studio-worker` "just works" with a
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
   - (A) Neither - user opts in per-event in Config.  *(my pick)*
   - (B) Failures only.
   - (C) Both.
6. **Close-button semantics with tray active**:
   - (A) Close → hide to tray, loops keep running, Quit via tray.
     *(my pick - standard tray convention)*
   - (B) Close → quit entirely, tray is purely a status indicator.
7. **Autostart-on-login toggle in Config tab**.
   - (A) Yes - toggle that registers `studio-worker ui` as the
     desktop autostart entry, coexisting with the existing
     `install-service` flow (the two serve different deployments).
     *(my pick)*
   - (B) No - v1 only exposes window + tray; autostart stays a CLI
     responsibility.

## Existing surface the UI reuses (no rewrites)

- `Arc<Mutex<Config>>` - already shared across loops.
- `Arc<Mutex<Vec<LogEntry>>>` - already drained by `log_shipper_tick`;
  the UI subscribes to the same buffer (read-only).
- `Arc<AtomicBool> busy` - already flipped by `claim_tick` around the
  in-flight job.  UI renders idle / busy from this.
- `runtime::register`, `runtime::set_enabled`, `runtime::set_threshold`,
  `runtime::format_status`, `runtime::format_check_outcome` - invoked
  from button handlers / About tab; no new business logic in the UI
  layer.

## New shared state the UI needs (added to `runtime`)

- `Arc<Mutex<Option<CurrentJob>>>` - populated by `claim_tick` on
  successful claim, cleared on completion / failure.
- `Arc<Mutex<VecDeque<RecentJob>>>` - bounded ring (default 50) of
  finished jobs with outcome + duration.
- `Arc<Mutex<Option<HeartbeatStatus>>>` - last heartbeat outcome +
  timestamp, populated by `heartbeat_tick`.

These are added behind the same in-process plumbing the existing loops
use; no wire-format change, no API change.

---

## Phase 1 - Plumb the new shared state into the runtime (no UI yet)

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
      existing `Arc<Mutex<...>>` style - no new sync primitive.
- [x] Tests green; commit "feat(runtime): expose current-job / recent
      / heartbeat state for the UI".

## Phase 2 - Cargo feature + subcommand wiring

- [x] Add `ui` feature in `Cargo.toml` with `egui` and `eframe`
      (`default-features = false`, `glow` backend) as optional deps.
      Default of the feature is the answer to **fork #1** above.
- [x] Failing CLI parse test: `studio-worker ui` parses into a new
      `Command::Ui` variant.  Test compiles with and without the
      feature (subcommand is always parseable; only the dispatch is
      gated).
- [x] Add `Command::Ui` to `src/cli.rs` and dispatch in
      `lib::run_cli`.  When the feature is off, dispatch prints a
      friendly "this binary was built without the `ui` feature; run
      `cargo install studio-worker --features ui` or use the desktop
      installer" message and exits non-zero.
- [x] Tests green; commit "feat(cli): add `ui` subcommand stub behind
      cargo feature".

## Phase 3 - App skeleton + tab shell

- [x] New module `src/ui/mod.rs` (gated `#[cfg(feature = "ui")]`).
      Exports `pub fn run(config_path: Option<&str>) -> Result<()>`
      that loads config, spawns the runtime loops on a background
      tokio task, and hands control to eframe on the main thread.
- [x] Headless test: instantiate the eframe `App` with mock shared
      state, drive one frame via `egui::Context::run`, assert the
      five tab labels are present.  Implemented via
      `egui::__run_test_ctx` smoke tests; tab labels exposed via
      `Tab::ALL` + `Tab::label()` with their own unit tests.
- [x] Implement the tab shell (top tab bar + central panel) wired to
      a `Tab` enum.  Default tab on startup: **Status**.
- [x] Tests green; commit "feat(ui): tab shell + Status placeholder".

## Phase 4 - Status tab

- [x] Failing test: status-tab view model formats `worker_id`,
      `api_base_url`, busy flag, last heartbeat, VRAM (probed via
      `sys::detect_vram_gb`), threshold.  When `worker_id` is `None`
      the view model carries an `Unregistered` variant.
- [x] Failing test: a render with `Unregistered` state shows a
      "Register..." button; **fork #2** determines what clicking it
      does (modal form vs. error message) - picked A (in-window form).
- [x] Implement Status tab using the view model.
- [x] Tests green; commit "feat(ui): tabs, register form, tray,
      notifications, autostart" (combined Phase 4-11 commit).

## Phase 5 - Jobs tab

- [x] Failing test: with `current_job = Some(...)` and three
      `recent_jobs` entries the view model produces one
      "Current" card + three "Recent" rows in chronological order
      (newest first).
- [x] Failing test: elapsed time formatter renders sub-minute as
      `12s`, sub-hour as `3m 04s`, longer as `1h 12m`.
- [x] Implement Jobs tab (Current card with prompt preview + kind +
      model + elapsed; Recent list with outcome icon, duration,
      finished-at).
- [x] Tests green; commit (combined Phase 4-11).

## Phase 6 - Config tab

- [x] Failing test: editing `vram_threshold_gb` via the view model
      and pressing **Save** writes a config.toml on disk whose
      `vram_threshold_gb` matches the new value.
- [x] Failing test: editing `engine` to a new value flips a "restart
      required" flag in the view model (per **fork #3**, banner).
- [x] Failing test: editing `bootstrap_token` is supported but the
      widget masks the value by default (no plaintext leak in
      screenshots).
- [x] Implement Config tab: one widget per `Config` field, grouped
      into sections (Connection / Worker / Engine / Auto-update /
      Models / Notifications / Background mode).  Save button calls
      `config::save`.  Reset button reverts unsaved edits.
- [x] Tests green; commit (combined Phase 4-11).

## Phase 7 - Logs tab

- [x] Failing test: with a 1 000-entry log buffer the view model
      windows to the last 500 by default and supports a level filter
      (`info` / `warn` / `error`).
- [x] Failing test: when "Auto-scroll" is on, the view model reports
      `scroll_to_end = true` on every render (achieved via
      `egui::ScrollArea::stick_to_bottom(filter.auto_scroll)`).
- [x] Implement Logs tab using a `egui::ScrollArea` with virtualised
      rows, level filter combo, free-text search box, auto-scroll
      toggle.  ("Copy all visible" deferred to v1.1.)
- [x] Tests green; commit (combined Phase 4-11).

## Phase 8 - About tab

- [x] Failing test: About view model carries `AGENT_VERSION`,
      `RELEASE_NAME`, the resolved config path, and the last
      `update::check_update` outcome (via `format_check_outcome`).
- [x] Implement About tab: version + release + config path +
      "Check for updates now" button (calls a fresh
      `update::check` against the configured feed).  ("Open log file"
      / "Open config file" buttons deferred to v1.1.)
- [x] Tests green; commit (combined Phase 4-11).

## Phase 9 — Window lifecycle (hide-to-tray) + graceful shutdown

- [x] Hide-to-tray on OS close: `App::update` intercepts
      `viewport().close_requested()` and replies with `CancelClose +
      Visible(false)` so loops keep ticking.
- [x] Tray Quit handler: sets `quit_requested`, next frame stores
      `stop = true` and sends `ViewportCommand::Close`.
- [ ] Failing test for the 5s graceful-drain budget on quit + the
      "in-flight job is awaited" path — **deferred** to v1.1; the
      Phase 9 behaviour is verified by hand against the fake studio
      in the verification capture (see `docs/screenshots/`).

## Phase 10 - Tray icon + notifications

- [x] Add `tray-icon` and `notify-rust` to the `ui` feature.  Icons
      generated programmatically (3 coloured 16×16 RGBA disks); the
      `assets/tray/` directory exists for future bespoke art.
- [x] Failing unit test: tray view model derives the icon variant
      from `(busy, last_heartbeat_ok)` - idle when not busy + last
      heartbeat ok, busy when `busy=true`, disconnected when last
      heartbeat failed or older than `3 × heartbeat_interval`.
- [x] Failing unit test: tray menu factory produces the right
      labels (`Open Window`, `Pause claiming` ↔ `Resume claiming`
      based on `auto_enabled`, `Quit`).
- [x] Failing unit test: notification gate - with both toggles off,
      a fake claim-tick completion does not emit a notification;
      with the completion toggle on it emits exactly one with the
      job kind + model in the body.
- [x] Implement the tray: built inside the `eframe::run_native` app
      creator (when winit's event loop is established); muda menu
      events polled on a dedicated thread that routes Open / Toggle
      / Quit and calls `ctx.request_repaint()`.  Icon variant /
      tooltip refreshed every frame in `App::refresh_tray_variant`.
      Tray init failure logs a warn and the UI keeps running.
- [x] Implement notification routing in `App::drain_notifications`
      (called every frame); the notifier itself sits behind a
      `Notifier` trait so tests inject a `CapturingNotifier` and
      assert on what would have been shown.
- [x] Linux build deps tracked in `~/install.sh`:
      `libxdo-dev`, `libayatana-appindicator3-dev`, `libgtk-3-dev`,
      `libdbus-1-dev`.  README + checks.yml additions land in
      Phase 12.
- [x] Tests green; commit (combined Phase 4-11).

## Phase 11 - Autostart-on-login (Config tab toggle)

- [x] Test: pure-data renderers for the Linux `.desktop` entry and
      the macOS LaunchAgent plist (`render_desktop_entry`,
      `render_launch_agent`) carry the resolved `Exec=` path + the
      `ui` argument; full disk-write round-trip deferred to v1.1.
- [x] Implement the autostart writer (Linux `~/.config/autostart`,
      macOS `~/Library/LaunchAgents`, Windows marker file).
      Separate from `service::install` because that owns systemd /
      launchd / Scheduled-Task lifecycles for the headless `run`
      subcommand.
- [x] Surface the toggle in the Config tab under a "Background mode"
      group with a one-line explainer ("Run in tray on login").
- [x] Tests green; commit (combined Phase 4-11).

## Phase 12 — CI + docs

- [x] `.github/workflows/checks.yml`: added `ui` row to the matrix
      that `apt-get install`s `libgtk-3-dev` + `libdbus-1-dev` +
      `libxdo-dev` + `libayatana-appindicator3-dev` before running
      `cargo clippy --tests --features ui -- -D warnings`,
      `cargo check --features ui`, and `cargo test --features ui`.
      Headless egui tests run fine without a display.
- [ ] `.github/workflows/build.yml`: confirm the release matrix builds
      `--features ui` for the desktop targets.  **Deferred** —
      separate from the checks workflow; the release build can land
      once we cut a v0.2.x.
- [ ] `cargo-dist` config in `Cargo.toml`: ensure the installer
      ships the `--features ui` build for desktop targets.
      **Deferred** — same release-cut window.
- [x] README: new "Desktop UI" section with a status-tab screenshot,
      `studio-worker ui` invocation, tray-icon behaviour notes, and
      the cargo-feature note.
- [x] `AGENTS.md` Tech stack table: added `egui` + `eframe` +
      `tray-icon` + `notify-rust` + `gtk` rows.
- [x] `LESSONS_LEARNED.md`: recorded the two non-obvious gotchas
      (Linux tray needs its own GTK thread; Linuxbrew pkg-config
      shadows the system one).

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
