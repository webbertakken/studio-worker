# Pause / Resume

Runtime-only operator toggle that tells the worker to stop accepting
new jobs without disconnecting from the studio.  Replaces the legacy
`auto_enabled` persisted config field \u2014 see
[`docs/architecture/overview.md`](../architecture/overview.md#config--persisted-state)
for the rest of the config simplification.

## Shape

```rust
let paused: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
```

Owned by [`runtime::run`](../../src/runtime.rs) (headless) and
[`ui::run`](../../src/ui/mod.rs) (egui), passed down into
`run_loops` \u2192 `spawn_ws_session` \u2192 `SessionContext.paused`.  Cheap
clone (it's an `Arc`).

## Semantics

`paused = false` (default after every restart) means:

- WS heartbeats advertise `autoEnabled = true` in the
  `WorkerCapabilities` snapshot.
- Incoming `Offer` frames are accepted and dispatched normally.

`paused = true` means:

- Heartbeats advertise `autoEnabled = false`.  The studio's
  `pickWorkerForJob` filter skips us, so no new offers are pushed.
- If an `Offer` arrives anyway (race window between flipping the
  flag and the next heartbeat the studio acts on), the session sends
  `Reject { jobId, reason: "worker paused by operator" }` and the
  studio requeues.
- The current in-flight job (if any) is **not** interrupted \u2014
  pausing only affects acceptance of new work.

The flag is **runtime-only** by design.  No persistence to
`config.toml`; no resurrection across restarts; a service-managed
worker that gets restarted by systemd comes back unpaused.  The
operator can re-pause from the UI or by `studio-worker pause`
(future \u2014 not yet a CLI subcommand).

## Where the flag flips

| Surface | Mechanism |
|---|---|
| **UI Status tab** | Pause / Resume button.  `paused_flag.fetch_xor(true, SeqCst)`.  See [`src/ui/tabs/status.rs`](../../src/ui/tabs/status.rs). |
| **Tray menu** | The "Pause" / "Resume" item (label flips based on current state).  Same `fetch_xor`. |
| **Programmatic** | Any holder of the `Arc<AtomicBool>` can flip it; the WS session reads via `paused.load(Ordering::SeqCst)`. |

## Where the flag is read

| Site | Behaviour |
|---|---|
| `build_capabilities_with` | Emits `auto_enabled: !paused` in every Hello / Heartbeat |
| `handle_offer` | Early-rejects new offers when `paused.load() == true` |
| UI Status tab | Renders the **PAUSED** badge + button label.  See `StatusView::Registered.paused`. |
| Tray icon | Currently does **not** change variant based on pause (the variant is `idle / busy / disconnected` keyed off busy + last_heartbeat).  Future: add a paused variant. |

## Why not a config-persisted toggle

The persisted `auto_enabled` field had two problems:

1. **Surface confusion** \u2014 it was both an operator-facing setting
   (in the Config tab) AND a runtime decision (read by the
   dispatcher).  Editing it required a worker restart in practice.
2. **Restart semantics** \u2014 a service-restart should always come
   back willing to take work.  A persisted `auto_enabled=false`
   silently kept the worker idle indefinitely.

Runtime-only flag fixes both.  If you want persistent pause across
restarts, install the worker without auto-start (`auto_start =
false` in config), so the service won't relaunch on boot.
