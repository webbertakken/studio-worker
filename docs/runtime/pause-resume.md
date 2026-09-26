# Pause / Resume

Runtime-only operator toggle that tells the worker to stop accepting
new jobs without disconnecting from the studio.  Replaces the legacy
`auto_enabled` persisted config field — see
[`docs/architecture/overview.md`](../architecture/overview.md#config--persisted-state)
for the rest of the config simplification.

## Shape

```rust
let paused: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
```

Owned by [`runtime::run`](../../src/runtime.rs) (headless) and
[`ui::run`](../../src/ui/mod.rs) (egui), passed down into
`run_loops` → `spawn_ws_session` → `SessionContext.paused`.  Cheap
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
- The current in-flight job (if any) is **not** interrupted —
  pausing only affects acceptance of new work.

The flag is **runtime-only** by design.  No persistence to
`config.toml`; no resurrection across restarts; a service-managed
worker that gets restarted by systemd comes back unpaused.  The
operator can re-pause from the tray UI (or `POST /daemon/pause`).

## Where the flag flips

| Surface | Mechanism |
|---|---|
| **Local API** | `POST /daemon/pause` / `POST /daemon/resume` → `DaemonControl::set_paused` (logs `op="control"`).  See [`src/control.rs`](../../src/control.rs). |
| **Tray UI** | Pause / Resume in the pulse header (every page) and on the Worker page; sends the route above.  See [`src/ui/chrome.rs`](../../src/ui/chrome.rs) and [`src/ui/pages/worker.rs`](../../src/ui/pages/worker.rs). |
| **Tray menu** | The "Pause" / "Resume" item (label follows the daemon's state); sends the same route. |
| **Programmatic** | Any holder of the daemon's `Arc<AtomicBool>` can flip it; the WS session reads via `paused.load(Ordering::SeqCst)`. |

## Where the flag is read

| Site | Behaviour |
|---|---|
| `build_capabilities_with` | Emits `auto_enabled: !paused` in every Hello / Heartbeat |
| `handle_offer` | Early-rejects new offers when `paused.load() == true` |
| `GET /daemon/status` | `paused`; the tray UI renders the **PAUSED** badge + button label from it.  See `StatusView::Registered.paused`. |
| Tray icon | Currently does **not** change variant based on pause (the variant is `idle / busy / disconnected` keyed off busy + last_heartbeat).  Future: add a paused variant. |

## Why not a config-persisted toggle

The persisted `auto_enabled` field had two problems:

1. **Surface confusion** — it was both an operator-facing setting
   (in the Config page) AND a runtime decision (read by the
   dispatcher).  Editing it required a worker restart in practice.
2. **Restart semantics** — a service-restart should always come
   back willing to take work.  A persisted `auto_enabled=false`
   silently kept the worker idle indefinitely.

Runtime-only flag fixes both.  There is no persistent pause: a worker that
should not claim studio jobs at all is left unregistered (the local API and
model host serve without a studio).
