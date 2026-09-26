# Daemon and tray UI

The worker is two processes built from one binary:

- the **daemon** (`studio-worker run`) hosts everything: the studio session, the local API,
  the model host, the streaming speech listener, the auto-updater, and every job;
- the **tray UI** (`studio-worker ui`) is a client of the daemon over the
  [local API](../local-api.md). It shows what the daemon does and sends the operator's
  actions back. It never runs a job and never talks to the studio.

Closing or crashing the UI never stops a job. Stopping the daemon stops the worker.

## Terms

| Term | Meaning |
| --- | --- |
| daemon | the `run` process; one per config directory |
| daemon lock | `<config dir>/daemon.lock`, held exclusively by the running daemon |
| UI lock | `<config dir>/ui.lock`, held exclusively by the running tray UI |
| tray UI | the `ui` process: egui window + system tray, client of the daemon |
| link | the UI's view of the daemon: `connected`, `starting` or `unreachable` |
| snapshot | one `GET /daemon/status` answer: everything the UI shows except logs |
| replica | the UI's local copy of the daemon's observers, refilled from each snapshot |
| job source | where a job came from: `studio`, `local` (transient local API job), `lane` (a request on a loaded model), `stream` (a streaming speech session) |
| job log | the log lines emitted while a job ran, kept per job |
| thumbnail | a small PNG of an image job's output, kept per job |

## One daemon per config directory

- `run` takes the daemon lock before anything else. When another process holds it, `run`
  logs `op="daemon_lock"` "another daemon is already running for this config" and exits 0,
  so a service manager does not treat it as a crash.
- Under a supervisor (PM2, systemd), run `studio-worker run --wait-for-lock`: if another
  daemon holds the lock (for example one the tray UI started), it logs "waiting for the daemon
  lock" once, with the holder's pid, and takes over the moment that daemon ends, instead of
  exiting and being restarted in a loop.
- The daemon's pid is in `<config dir>/daemon.pid`, beside the lock (Windows locks are
  mandatory, so the locked file itself cannot be read).
- The lock is released when the process exits, however it exits.
- The lock is advisory and per config directory: two workers with different `--config`
  directories run side by side.

## Registration never stops local serving

- The local API, model host and stream listener start before the registration gate.
- An operator rejection no longer ends the daemon. The daemon logs the rejection
  (`op="registration"`), keeps serving locally, and waits until either it is stopped or the
  UI asks for a reset (`POST /daemon/registration/reset`). A reset clears the local
  registration state exactly like `register --reset` and starts a fresh request.

## Local API additions

All new routes carry the local API's existing guards (loopback `Host`, loopback `Origin`,
bearer token). Details and shapes: [local API](../local-api.md#daemon-control).

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/daemon/status` | the snapshot |
| `GET` | `/daemon/logs?after=<seq>` | worker log entries newer than `seq` |
| `POST` | `/daemon/pause`, `/daemon/resume` | the runtime pause toggle |
| `GET`, `PUT` | `/daemon/config` | the operator-editable config; `PUT` validates, saves, applies |
| `POST` | `/daemon/registration/reset` | clear a rejected registration and ask again |
| `POST` | `/daemon/shutdown` | stop the daemon gracefully |
| `GET` | `/jobs/:id/log` | the job's log lines |
| `GET` | `/jobs/:id/thumbnail` | the job's thumbnail (`image/png`) |

`GET /models` additionally carries each model's `since`, `loadable` (its engine has an
in-process loader, so it can be kept loaded) and, when failed, `error`.

The UI polls rather than holding a stream open: the local API serves on a small fixed pool of
threads, and a long-lived stream per UI would pin one of them for good.

## Jobs

Every job, whatever its source, is visible while it runs and after it ends.

- A running job is in the **active jobs** list; it leaves the list when it ends, on every
  exit path (a guard removes it on drop).
- A finished studio job goes into the recent-jobs ring; every other finished job goes into the
  local-jobs ring. Both rings hold 50 jobs, newest first.
- Each job carries its `source`.

### Job logs

- A job runs inside a tracing span named `job` with a `job_id` field. A tracing layer copies
  every event emitted inside such a span, or carrying a `job_id` field itself, into that job's
  log. Engine events (downloads, subprocess output, loads) therefore land in the job log
  without the engine knowing about jobs.
- The layer sees the same events as the stderr log: `RUST_LOG` filters both.
- Bounds: the logs of the 128 most recent jobs; 400 lines per job (oldest dropped, the drop
  counted); 2 000 characters per line.
- Every job logs `op="job"` "job started" and "job finished" (with its outcome and duration),
  so even a silent engine leaves a trace.

### Worker log

- The Logs tab shows the daemon's worker log ring (1 000 entries): the studio session's
  breadcrumbs (`runtime::push_log`) plus every other `studio_worker` event at info and
  up, copied in by a second tracing layer.  Each entry carries the job id when it was
  emitted inside a job.
- The UI reads it incrementally: `GET /daemon/logs?after=<seq>`.

### Thumbnails

- When a job returns an image, the daemon decodes it and keeps a PNG of at most 384 px on the
  longer side: sharp on a card, and large enough to recognise when the UI shows it larger.
- Bounds: the thumbnails of the 100 most recent image jobs (the size of both job rings).
- A thumbnail that cannot be made is logged (`op="thumbnail"`) in the job's log and the job
  itself is unaffected.

## The tray UI

### Start-up

1. Resolve the config path (the daemon owns the config; the UI never writes it; it only
   reads `start_minimised` from it before the daemon answers).
2. Take the UI lock (see [one tray UI per config directory](#one-tray-ui-per-config-directory)),
   or hand over to the tray UI that holds it and exit.
3. Install the login autostart entry for the tray UI; it is always installed.
4. Start the poller (below).
5. Open the window. When there is no usable display (e.g. started at login before the
   graphical session accepts clients), the UI logs `op="display_wait"` with the attempt and
   the error, waits (2 s doubling to 60 s) and starts itself again in place. It never exits
   for want of a display.

A failed display connection cannot be retried inside the same process (the windowing library
allows one event loop per process and caches a failed display connection), hence the restart
in place.

### Poller

Once a second the poller:

1. reads `<config dir>/local-api.json` for the daemon's URL and token;
2. fetches the snapshot, the new log entries, the model list, the selected job's log, and any
   thumbnail it does not have yet;
3. applies them to the replica.

When the daemon cannot be reached, the link becomes `unreachable` and the replica is emptied,
so no tab shows stale data. If the daemon lock stays free for 20 s (`SPAWN_GRACE`, longer than
a supervisor's restart gap, so a supervised daemon is never raced), no daemon is running: the
poller starts one (`studio-worker --config <path> run`, detached in its own process group, output
appended to `<config dir>/daemon.log`) at most once every 10 s and logs `op="daemon_spawn"`;
a thread reaps it and logs its exit. If the lock is held, a daemon is starting or wedged,
and the link reads `starting`.

A daemon the UI started outlives the UI.  (A process supervisor that kills whole process
trees, such as PM2, also stops it when it stops the UI.)

### One tray UI per config directory

- `ui` takes `<config dir>/ui.lock` exclusively before anything else, so one config never gets
  two tray icons.
- When another tray UI holds it, the new one writes `<config dir>/ui.raise`, logs
  `op="single_instance"` "another tray UI is running for this config" and exits 0.
- The running tray UI checks for `ui.raise` every 250 ms; when it appears, it deletes it, logs
  `op="raise"` and shows, un-minimises and focuses its window.
- A tray UI that restarts itself for the display (below) waits up to 5 s for the lock its
  predecessor still holds.
- When the lock file cannot be opened, the UI logs a warning and runs without the guard.

### Window

The window is a calm instrument panel: a navigation rail on the left, a pulse header on top, a
status bar at the bottom, and the page in between. The header, the status bar and the rail keep
their size whatever the state, so nothing moves when a job starts or ends.

- **Rail**: Jobs, Models, Worker, Logs, Config, each an icon with its name, in that order;
  `Ctrl+1` to `Ctrl+5` pick them, and each is reachable with `Tab` and activated with `Enter`
  or `Space`. The tray UI version sits at the foot of the rail.
- **Jobs is the page on open.**
- **Pulse header**, always present:
  - activity: `Idle`, `Paused`, or `Running <kind> · <model> · <elapsed>` (with `+N more`
    when several jobs run); the running dot breathes softly;
  - the daemon link: `Daemon connected`, `Daemon starting`, `Daemon unreachable`;
  - the studio: `Studio connected`, `Reconnecting (<n>)`, `Awaiting approval`,
    `Registration rejected`, `Studio auth failed`, …;
  - GPU memory: a bar and `≈ <loaded> / <total> GB`, the sum of the catalogue estimates of the
    models loaded (or loading) against the device total;
  - **Pause / Resume**, enabled while the daemon answers.
- **Status bar**: the daemon version and URL (or what the UI does about a missing daemon) on the
  left, the result of the last action on the right (red when it failed). Errors are shown
  where they happen, never as toasts.
- **Appearance**: dark by default; Light and Follow system in Config. **Reduce motion** holds
  the breathing glow steady. Both, and the notification toggles, are UI preferences stored in
  `<config dir>/ui.toml` and applied at once (the daemon never reads them).
- Text and state colours meet WCAG 2.2 AA contrast (4.5:1 for text, 3:1 for indicators and the
  focus ring) in both themes; tests hold the palettes to it.
- While the link is not `connected`, every page except Worker shows a "daemon not reachable"
  card that says what the UI does about it; Worker shows it above the About card.

#### Jobs

- Two panes. The left pane lists the jobs; the right pane shows the selected job.
- **Now running** is always reserved at the top of the list: one card per running job, its
  border glowing softly, or an empty card of the same height when nothing runs.
- **History** below: studio and local jobs together, newest first, grouped by day (`Today`,
  `Yesterday`, then the date), with `All`, `Studio` and `Local` filters showing their counts;
  the local API URL shows under the `Local` filter.
- A card shows a tile (the image thumbnail, or the job kind's glyph), the prompt as its title
  (two lines at most; `No prompt` when empty), `<kind> · <model>`, `<source> · <time> ·
  <duration>` and an outcome pill (`Running`, `Done`, `Failed`).
- The detail pane shows, for the selected job: the thumbnail (click it, or press `Enter` on
  it, to see it larger), the facts (id, source, kind, model, started, finished, duration,
  outcome), the whole prompt, the failure reason, and the job's log: monospace, levels
  coloured, wrapping, selectable, with a **Copy log** button. With nothing selected it says
  how to pick a job.
- Clicking a card selects it; clicking it again clears the selection. `↑` / `↓` move the
  selection, `Esc` closes the larger image or clears the selection.

#### Models

- A memory summary on top: the device total, a bar with one segment per loaded model, and the
  sum of their estimates.
- Two groups that never reorder: **Kept in memory** (models with an in-process loader) and
  **Loaded per job** (engines that load for each job, e.g. `sd-cli`).
- Each model is one row: its state (a coloured dot and word: `loaded`, `loading`,
  `unloading`, `unloaded`, `failed`), its name and id, `<kind> · <engine> · ≈ <GB>`, a
  `resident` pin, its exclusive group, since when it is in its state, and one action of fixed
  width: **Load**, **Unload**, **Retry** after a failure, or a disabled `Loading…` /
  `Unloading…`. A failed model shows its error in the row.

#### Worker

The worker's identity and health on one page (formerly Status and About):

- a hero card with the state (`Idle`, `Running`, `Paused`) and **Pause / Resume**;
- registration: `Initialising`, `Waiting for approval` (with the request id and a copy
  button), `Registration rejected` (with the reason and **Reset registration**), or
  registered (the worker id, copyable);
- Studio: connection, last heartbeat, API base URL;
- Hardware: GPU runtime, VRAM total, VRAM threshold per claim, memory held by loaded models;
- Local API: its URL;
- About: tray UI and daemon versions (they differ after the daemon updated itself until the
  UI restarts), Sentry release, config path (copyable), **Check for updates**.

#### Logs

- A toolbar: level (`All`, `Info`, `Warn`, `Error`), search (category, message, job id),
  **Follow** (stick to the newest line) and **Copy**.
- The log as selectable monospace text: local time, level (coloured), category, message
  (wrapping) and job id.

#### Config

- One card per section: Connection, Worker, Auto-update, Models, sent to the daemon with
  **Save** (it validates, saves and applies them); and This window (appearance, reduce motion,
  notifications), applied and stored at once.
- A footer that never changes height: Save, Reset, and the save state (`Unsaved changes`,
  `Saving…`, `Saved`, or the daemon's refusal).

#### Headless inspection

`STUDIO_WORKER_UI_PAGE=<page>` picks the first page and `STUDIO_WORKER_UI_JOB=<id|latest>`
selects a job once it shows up, for screenshots and headless inspection.

### Tray

- The icon reflects the daemon: busy, idle, or disconnected (also when the link is down).
- Menu: Open window, Pause/Resume (sent to the daemon), Quit.
- Quit stops the daemon (`POST /daemon/shutdown`) and closes the UI. Closing the window only
  hides it.

## Autostart

- The tray UI's login entry is always installed and kept pointing at the current executable
  (Linux `.desktop`, macOS LaunchAgent, Windows `HKCU\…\Run`). There is no setting to turn it
  off.
- The daemon is started by the UI when absent, or supervised by the OS service
  (`install-service`) on machines that want it running before anyone logs in.

## Observability

| `op` | Target | When |
| --- | --- | --- |
| `daemon_lock` | `studio_worker::daemon` | lock taken, or another daemon holds it |
| `registration` | `studio_worker::runtime` | rejection wait, reset |
| `control` | `studio_worker::local_api` | pause, resume, config save, reset, shutdown via the API |
| `job` | `studio_worker::job` | job started / finished |
| `thumbnail` | `studio_worker::job` | thumbnail could not be made |
| `link` | `studio_worker::daemon_link` | the UI's link to the daemon changed state |
| `daemon_spawn` | `studio_worker::daemon_link` | the UI started a daemon, failed to, or it exited |
| `action` | `studio_worker::daemon_link` | an operator action reached the daemon, or did not |
| `display_wait` | `studio_worker::ui` | no usable display yet; retrying |
| `single_instance` | `studio_worker::ui` | another tray UI holds the UI lock; this one hands over and exits |
| `raise` | `studio_worker::ui` | a second launch asked this tray UI to show its window |
| `prefs` | `studio_worker::ui` | UI preferences could not be read or saved |
| `enable` / `ensure` | `studio_worker::autostart` | login entry written / already current / failed |
