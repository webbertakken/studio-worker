# Flows inventory: studio-worker ↔ minigames studio

Goal: identify and document every flow in studio-worker's architecture and
implementation, including how each integrates with the minigames studio
(`~/Repositories/minigames/apps/studio`). Output: a single flows document
in `docs/architecture/flows.md` with sequence-level detail per flow,
verified against both codebases.

## Tasks

- [x] 1. Read existing studio-worker docs (`docs/architecture/overview.md`,
      `docs/runtime/*.md`, `docs/operations/*.md`, `docs/index.md`) to
      collect already-documented flows and avoid re-deriving them.
- [x] 2. Trace the registration + approval flow: `src/auto_register.rs`,
      `src/http.rs` (`/register`), `src/config.rs` persistence, and the
      studio side (register route, pending-worker approval) in
      `apps/studio`.
- [x] 3. Trace the WebSocket session flow: `src/ws/{client,session,types}.rs`
      vs studio `WorkerConnections` Durable Object +
      `src/shared/types/workerWs.ts` — hello/welcome, heartbeat, offer →
      accept/decline, progress, completeJson, pause/resume, close codes.
- [x] 4. Trace the job execution flow: offer → engine dispatch
      (`src/engine/` MultiEngine, synthetic, sdcpp + feature-gated
      backends), ModelSource on-demand download (`engine/download.rs`),
      sd-cli auto-provision (`engine/sd_provision.rs`).
- [x] 5. Trace the result upload flow: `TaskResult` → completeJson over WS
      vs multipart HTTP `/complete` (`src/http.rs`), and the studio's
      complete route + R2 storage.
- [x] 6. Trace the lifecycle/ops flows: startup (`main.rs`/`cli.rs`/
      `runtime.rs`), auto-update (`src/update.rs`), autostart
      (`src/autostart.rs`), service install (`src/service.rs`), telemetry
      (`src/telemetry.rs`), host probes (`src/sys.rs`).
- [x] 7. Trace the UI flows: `src/ui/` (tab shell, observers, tray,
      notifications) and how they hook into the runtime via
      `WorkerObservers`.
- [x] 8. Trace the studio-side job lifecycle around the worker: job
      creation/queueing, VRAM-threshold matching, offer dispatch, timeout/
      requeue, result consumption in `apps/studio`.
- [x] 9. Write `docs/architecture/flows.md`: one section per flow with a
      concise sequence (and a plantuml diagram for the core job flow),
      cross-linked to existing docs; link it from `docs/index.md`.
- [x] 10. Verify: every flow section cites real files/types on both sides;
      run `cargo check` untouched-code sanity not needed, but ensure doc
      links resolve; commit.
      Done: links verified, merged as PR #44; follow-up commit adds the
      `onnx` engine (PR #42) to the engine-routing lists.
