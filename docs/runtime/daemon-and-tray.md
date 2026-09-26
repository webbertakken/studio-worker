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

`GET /models` additionally carries each model's `since` and, when failed, `error`.

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

### Thumbnails

- When a job returns an image, the daemon decodes it and keeps a PNG of at most 192 px on the
  longer side.
- Bounds: the thumbnails of the 100 most recent image jobs (the size of both job rings).
- A thumbnail that cannot be made is logged (`op="thumbnail"`) in the job's log and the job
  itself is unaffected.

## The tray UI

### Start-up

1. Resolve the config path (the daemon owns the config; the UI never writes it).
2. Install the login autostart entry for the tray UI; it is always installed.
3. Start the poller (below).
4. Open the window. When there is no usable display (e.g. started at login before the
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
so no tab shows stale data. If the daemon lock is free, no daemon is running: the poller starts
one (`studio-worker --config <path> run`, detached, output appended to
`<config dir>/daemon.log`) at most once every 10 s and logs `op="daemon_spawn"`. If the lock is
held, a daemon is starting or wedged, and the link reads `starting`.

### Window

- A status line at the top is always present: the link state, the daemon version and URL, and
  the result of the last action. It never changes height.
- While the link is not `connected`, the tabs are replaced by a "daemon not reachable" view
  that says what the UI is doing about it.
- **Status**: registration, session, heartbeat, GPU runtime, Pause/Resume, and, when rejected,
  Reset registration.
- **Jobs**: running jobs, recent studio jobs, the local queue. Each card shows its source,
  kind, model, prompt, outcome and duration, and its thumbnail when it has one. Selecting a
  card shows its log.
- **Models**: each catalogue model with kind, engine, memory estimate, state, residency and
  since; Load and Unload buttons per the lifecycle guards; a failed model shows its error.
- **Config**: the operator-editable fields; Save sends them to the daemon, which validates,
  saves and applies them.
- **Logs**: the worker log, filtered and searchable.
- **About**: UI and daemon versions, config path, update check.

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
| `link` | `studio_worker::ui::link` | the UI's link to the daemon changed state |
| `daemon_spawn` | `studio_worker::ui::link` | the UI started a daemon, or failed to |
| `display_wait` | `studio_worker::ui` | no usable display yet; retrying |
| `autostart` | `studio_worker::autostart` | login entry written / already current / failed |
