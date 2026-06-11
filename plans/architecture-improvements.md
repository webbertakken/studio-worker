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

- [x] 3. **WS log ship-queue is unbounded.**
      `push_log_with_observers` does `logs.lock().push(entry)`
      (`src/runtime.rs` ~line 825) but the shipper pump only drains
      while a session is connected.  Long approval waits / reconnect
      backoff grow the queue without bound.  Fix: cap the ship queue
      (e.g. 5000 entries, drop-oldest) with a dropped-count
      breadcrumb so loss is visible.  TDD: unit test that pushes past
      the cap and asserts size + the drop marker.

- [x] 4. **Error classification by string sniffing.**
      `is_unsupported_kind(e) = e.to_string().contains("cannot serve")`
      (`src/runtime.rs` ~line 704) decides `retryable` on the wire.  A
      reworded engine message silently flips terminal failures into
      infinite retries.  Fix: a typed `UnsupportedTask` error
      (thiserror) returned by engines and detected via
      `anyhow::Error::downcast_ref`, with the string check kept as a
      fallback for one release.  TDD: classification tests for typed,
      legacy-string, and unrelated errors.

- [x] 5. **`run_offered_job` outcome bookkeeping is fragile.**
      The function pre-seeds `let mut outcome = JobOutcome::Failed
      {...}` under `#[allow(unused_assignments)]` and relies on every
      match arm assigning (`src/ws/session.rs` ~line 700).  Fix:
      extract the result-delivery into a helper that *returns*
      `JobOutcome`, so the compiler enforces exhaustiveness and the
      lint allow disappears.  Pure refactor — behaviour pinned by
      existing tests.

- [x] 6. **Failed log batches are dropped.**
      The shipper `std::mem::take`s the buffer and, when the send
      fails, the batch is gone (warn only).  Fix: on send failure,
      push the batch back to the front of the queue (the cap from
      task 3 bounds the requeue).  TDD: pump test with a failing
      sender asserting entries survive for the next session.

- [x] 7. **Transient upload failure costs a full regeneration.**
      A single multipart `/complete` 5xx → `Fail { retryable: true }`
      → studio requeues → the GPU re-renders (~10s) for what was a
      2s upload blip.  Fix: bounded retry (2 attempts, 1s/2s backoff,
      stop-aware) around `ApiClient::complete` before reporting Fail.
      TDD: wiremock test (500, 500, 200 → completes; 3×500 → Fail).

## Cross-repo items

- [x] 8. **Structured reject reasons.**  Done both sides: the worker
      sends `code: busy | paused` on every Reject (PR #48); the
      studio's `isTransientReject` branches on the code with the regex
      kept as fallback for old workers, and unknown future codes
      degrade to the regex path (minigames branch
      `worker-protocol-improvements`, local — push awaits user
      permission per repo rules).

- [x] 9. **Model download integrity.**  Done both sides: optional
      `sha256` per `ModelFile`; the worker hashes the body while
      streaming and refuses to cache a mismatch (PR #48).  TS types
      widened so registry rows can carry the hash (same minigames
      branch).  Backfilling hashes onto existing `studioModels` rows
      is operator data-entry, not code.

- [x] 10. **Dashboard `WorkerView.engine` mapping is stale.**  Fixed:
      `routes/workers.ts` passes the self-reported engine through
      (same minigames branch).

- [x] 11. **Heartbeat D1 write amplification.**  Decision: deferred.
      The dashboard's `deriveStatus` freshness buckets (online ≤ 10s,
      idle ≤ 30s) are calibrated to the 5s write cadence; throttling
      writes would degrade every worker to "idle"/"stale" without a
      matching UI change, and the write volume is irrelevant at the
      current fleet size.  Revisit alongside a dashboard freshness
      redesign if the fleet grows past ~20 workers.

- [x] 12. **Blocking HTTP client inside an async runtime.**  Partial:
      the process now shares one `reqwest::blocking::Client` (TLS
      setup + connection pool no longer rebuilt per call).  Decision:
      the full async-reqwest migration is deferred — it touches every
      HTTP call site for no observable behaviour change at the
      worker's request rate (a handful of calls per job).

## Worth noting, deliberately not planned

- `sd-cli` per-job process spawn (model reload each job) — known,
  documented in `docs/engines/sdcpp.md`; operator tolerance is fine.
- The multipart field is named `image` even for wav/gif — cosmetic,
  both sides agree, a rename is pure churn.
- CLI `pause` subcommand — needs IPC to the running process; deferred
  until an operator actually asks for headless pause.
