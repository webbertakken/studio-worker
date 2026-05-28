# studio-worker wiki

Project-level documentation. Architecture deep-dives, concept
pages, operator playbooks, contributor reference.

For day-to-day install / register / first-run instructions, see the
top-level [`README.md`](../README.md).  For active plans, see
[`plans/`](../plans).

## Architecture

- [Overview](architecture/overview.md) — what the worker is, how it
  boots, the lifecycle of one job, every Rust module's role, the
  HTTP + WebSocket surfaces it speaks to the studio.

## Runtime

- [ModelSource](runtime/model-source.md) — studio-driven download
  spec attached to every Offer.  The contract that lets the worker
  serve any model the studio adds without a rebuild.
- [Pause / Resume](runtime/pause-resume.md) — runtime-only operator
  toggle that replaces the legacy persisted `auto_enabled` field.
- [No-fallback policy](operations/no-fallback.md) — every offer
  carries `task` + `modelSource`; the synthetic engine only serves
  models whose source engine is `synthetic`.

## Engines

- [sd-cpp engine](engines/sdcpp.md) — real image inference via
  `stable-diffusion.cpp` as a subprocess.  Per-offer downloads,
  cached in `cfg.models_root`.

## Operations

- [Installing sd-cli](operations/sd-cli-install.md) — playbook for
  staging the `stable-diffusion.cpp` binary that the sdcpp engine
  drives.
- [Dev loop (PM2 + cargo-watch)](operations/dev-loop.md) — the two
  flavours of dev process and when to use each.
- [Job-state recovery](operations/recovery.md) — D1 reset SQL for
  zombie / synthetic-bad / failed rows.

## Screenshots

[Status](screenshots/status.png) ·
[Jobs](screenshots/jobs.png) ·
[Config](screenshots/config.png) ·
[Logs](screenshots/logs.png) ·
[About](screenshots/about.png) ·
[Status (unregistered / pre-approval)](screenshots/status-register.png)
