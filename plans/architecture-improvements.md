# Architecture, idiomacy + best-practice improvements

Findings from a deep code review (2026-06-11) of studio-worker and its
integration with the studio (`minigames/apps/studio`).  Each item names
the evidence in code.  Worker-local items are implemented here with
TDD; cross-repo / design-fork items are listed at the bottom and need
a product decision before implementation.

## Worker-local tasks

- [x] 1. **Fix llama backend init race** (PR #46).
      `global_backend()` raced `LlamaBackend::init()`: the loser saw
      `BackendAlreadyInitialized` before the winner published, and a
      bounded spin-wait flaked on loaded CI runners.  Init is now
      serialised behind a mutex with a re-check under the lock, plus a
      32-thread contention regression test.

- [x] 2. **Heartbeat capabilities go stale for a whole session.**
      `run_one_session` builds `WorkerCapabilities` once
      (`src/ws/session.rs` ~line 369) and the heartbeat pump only
      mutates `auto_enabled` per tick.  A Config-tab save (e.g.
      `vram_threshold_gb`) or `set-threshold` therefore never reaches
      the studio until the next reconnect, and `pickWorkerForJob`
      keeps filtering on the stale threshold.  Fix: pass the
      `SharedConfig` into the heartbeat pump and rebuild the
      capability snapshot from the live config each tick (reusing the
      session's engine handle so the per-build roster log doesn't
      fire every 5s).  TDD: session test that changes
      `vram_threshold_gb` mid-session and asserts the next heartbeat
      frame carries the new value.

- [ ] 3. **WS log ship-queue is unbounded.**
      `push_log_with_observers` does `logs.lock().push(entry)`
      (`src/runtime.rs` ~line 825) but the shipper pump only drains
      while a session is connected.  Long approval waits / reconnect
      backoff grow the queue without bound.  Fix: cap the ship queue
      (e.g. 5000 entries, drop-oldest) with a dropped-count
      breadcrumb so loss is visible.  TDD: unit test that pushes past
      the cap and asserts size + the drop marker.

- [ ] 4. **Error classification by string sniffing.**
      `is_unsupported_kind(e) = e.to_string().contains("cannot serve")`
      (`src/runtime.rs` ~line 704) decides `retryable` on the wire.  A
      reworded engine message silently flips terminal failures into
      infinite retries.  Fix: a typed `UnsupportedTask` error
      (thiserror) returned by engines and detected via
      `anyhow::Error::downcast_ref`, with the string check kept as a
      fallback for one release.  TDD: classification tests for typed,
      legacy-string, and unrelated errors.

- [ ] 5. **`run_offered_job` outcome bookkeeping is fragile.**
      The function pre-seeds `let mut outcome = JobOutcome::Failed
      {...}` under `#[allow(unused_assignments)]` and relies on every
      match arm assigning (`src/ws/session.rs` ~line 700).  Fix:
      extract the result-delivery into a helper that *returns*
      `JobOutcome`, so the compiler enforces exhaustiveness and the
      lint allow disappears.  Pure refactor — behaviour pinned by
      existing tests.

- [ ] 6. **Failed log batches are dropped.**
      The shipper `std::mem::take`s the buffer and, when the send
      fails, the batch is gone (warn only).  Fix: on send failure,
      push the batch back to the front of the queue (the cap from
      task 3 bounds the requeue).  TDD: pump test with a failing
      sender asserting entries survive for the next session.

- [ ] 7. **Transient upload failure costs a full regeneration.**
      A single multipart `/complete` 5xx → `Fail { retryable: true }`
      → studio requeues → the GPU re-renders (~10s) for what was a
      2s upload blip.  Fix: bounded retry (2 attempts, 1s/2s backoff,
      stop-aware) around `ApiClient::complete` before reporting Fail.
      TDD: wiremock test (500, 500, 200 → completes; 3×500 → Fail).

## Cross-repo / design forks (need a decision before implementing)

- [ ] 8. **Structured reject reasons.**  The studio's
      `isTransientReject` regex-matches the worker's free-text reason
      strings (`orchestrator.ts`); rewording `"worker paused by
      operator"` on the worker silently turns a no-attempt requeue
      into an attempt-burning release.  Proposal: add an optional
      `code: 'busy' | 'paused'` field to the `Reject` frame on both
      sides, regex kept as fallback.

- [ ] 9. **Model download integrity.**  `engine/download.rs` verifies
      `Content-Length` only — a compromised or corrupted mirror still
      lands in the cache.  Proposal: optional `sha256` per
      `ModelFile` in `studioModels` rows, verified by the worker when
      present.  (Already sketched in `plans/real-models-on-demand.md`.)

- [ ] 10. **Dashboard `WorkerView.engine` mapping is stale.**
      `routes/workers.ts` maps `engine === 'gradio' ? 'gradio' :
      'synthetic'` — every modern worker advertises `multi` and shows
      up as "synthetic" in the dashboard.  Proposal: pass the engine
      string through (and widen the TS union).

- [ ] 11. **Heartbeat D1 write amplification.**  `persistHeartbeat`
      writes a `studioWorkers` row every 5s per worker.  Fine at the
      current fleet size; at ~50 workers it's ~860k writes/day.
      Proposal: persist only when capabilities changed or >30s since
      the last write (in-memory freshness already lives in the DO).

- [ ] 12. **Blocking HTTP client inside an async runtime.**
      `reqwest::blocking` + `spawn_blocking` everywhere
      (`src/http.rs`, callers in session/auto-register/update).
      Works, but each call burns a blocking-pool thread and the
      client is rebuilt per call.  Proposal: migrate `ApiClient` to
      async reqwest (or at minimum cache one client per base URL).

## Worth noting, deliberately not planned

- `sd-cli` per-job process spawn (model reload each job) — known,
  documented in `docs/engines/sdcpp.md`; operator tolerance is fine.
- The multipart field is named `image` even for wav/gif — cosmetic,
  both sides agree, a rename is pure churn.
- CLI `pause` subcommand — needs IPC to the running process; deferred
  until an operator actually asks for headless pause.
