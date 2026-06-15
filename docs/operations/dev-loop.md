# Dev loop (PM2 + cargo-watch)

How to run the worker locally for the kind of iterate-watch-restart
loop you want during real development.  Production workers run the
release binary under systemd / launchd, not this dev loop — see
[`docs/architecture/overview.md`](../architecture/overview.md#service--autostart).

## Why PM2

PM2 owns the process tree and the log files.  We use it for every
long-running thing on the dev box (assistant agents, llama servers,
the worker itself) so the lifecycle story is uniform.  Per the
software-factory rules (`~/Repositories/software-factory/instructions/rules/process-management/RULES.md`):

> Always use PM2 to manage long-running processes. Never use bare
> `nohup`, `&`, or `screen`. Anything taking more than ~10 seconds
> MUST run via PM2 with logs to file.

## Two flavours of dev process

### Watching variant (good for source-iteration)

```bash
pm2 start /tmp/studio-worker-ui-dev.sh --name studio-worker-ui-dev --no-autorestart
```

Wrapper script:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd /home/webber/Repositories/studio-worker
export RUST_LOG="${RUST_LOG:-studio_worker=debug,info}"
export RUST_BACKTRACE=1
export DISPLAY="${DISPLAY:-:0}"
exec cargo watch \
  --why -w src -w Cargo.toml -w Cargo.lock -i target \
  -x 'run -- ui'
```

(The UI is the default build now — no `--features ui` and no
`PKG_CONFIG_PATH` dance: the GTK-free stack needs neither.)

`cargo-watch` rebuilds + restarts the worker on every source change.
Great for iterating on Rust code.  **Terrible** for letting a
long-running job complete: any agent (or you) touching `src/*.rs`
mid-job kills the child process, the WS session dies, the job goes
to terminal `failed`.

### Stable variant (good for chewing through a queue)

```bash
pm2 start /tmp/studio-worker-ui-stable.sh --name studio-worker-ui-stable --no-autorestart
```

Wrapper script:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd /home/webber/Repositories/studio-worker
export RUST_LOG="${RUST_LOG:-studio_worker=info,warn}"
export RUST_BACKTRACE=1
export DISPLAY="${DISPLAY:-:0}"
exec ./target/debug/studio-worker ui
```

No `cargo watch`.  Runs the binary you've already built with
`cargo build` (UI is default).  Source-tree edits don't restart it.
This is what you want when:

- You need the worker to complete a multi-hour batch (e.g. the
  ~1000 z-image-turbo runs we did to backfill assets).
- Another agent is editing source files in the same checkout.
- You're testing the auto-update flow.

## Gotchas

- **One worker per `worker_id`**.  Don't run both flavours
  simultaneously — the studio's DO closes the older session with
  `4003 duplicate_worker` and both workers thrash trying to
  reconnect.  `pm2 stop` one before starting the other.
- **Orphan child after cargo-watch restart**.  Killing the
  watcher's `cargo run` doesn't always reap the
  `target/debug/studio-worker` child.  Symptom: `pm2 stop` reports
  the process as down but `pgrep -af target/debug/studio-worker`
  shows it's still alive (and still claiming jobs!).  Hunt with
  `pgrep -af`, `kill <pid>` directly.
- **`PKG_CONFIG_PATH` on Linuxbrew machines**.  If `/home/linuxbrew/.linuxbrew/bin/pkg-config`
  is first on PATH it can't see system `.pc` files (cairo, gtk-3),
  and the UI build fails.  This is no longer needed — the UI stack is
  GTK-free (eframe/glow via dlopen, ksni tray, rustls), so the
  Linuxbrew `pkg-config` ordering doesn't matter.  For the in-process
  llama.cpp backend (`--features all`) you only need `cmake` + a C/C++
  compiler on PATH.
- **DISPLAY**.  The UI needs an X server.  Export
  `DISPLAY=:0` (or your session's display).  Headless workers run
  `studio-worker run` instead of `ui`; same wrapper minus the
  `ui` arg and the DISPLAY export.

## Tailing logs

```bash
tail -f /home/webber/.pm2/logs/studio-worker-ui-stable-out.log
tail -f /home/webber/.pm2/logs/studio-worker-ui-stable-error.log
```

PM2's own `pm2 logs --lines 50` works but if you want long greps
without TUI interference, tailing the file directly is cheaper.

## Clean shutdown

```bash
pm2 stop studio-worker-ui-stable
pm2 delete studio-worker-ui-stable    # if you want it gone from `pm2 list`
```

The worker handles SIGTERM gracefully — finishes the current job
(up to ~5 s), then exits.

## Where this came from

We discovered the cargo-watch-kills-the-job problem ~10 minutes into
the 1000-job z-image-turbo backfill.  Switching to the stable
variant kept the WS session alive for the full ~3 hours.  See
[LESSONS_LEARNED](../../LESSONS_LEARNED.md) for the timeline.
