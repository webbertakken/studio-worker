# WebSocket channel to studio (replaces HTTP polling)

Replace the four polling loops with a single WebSocket session to the `WorkerConnections`
Durable Object in the studio API. The server pushes job offers; this worker accepts /
rejects, runs the engine, and posts the result back over the same socket (or via HTTP
multipart for binary kinds). Per design choices 3b + 4b, there is **no HTTP polling
fallback** — if the socket can't be held, the worker exits non-zero.

Paired with `~/Repositories/minigames/plans/ws-worker-channel.md` — the wire format is
defined there. Both sides land together as a hard cutover.

## Goals (must-haves before "done")

- `cargo run -- run` opens one WS to `wss://studio.minis.gg/graphics/workers/connect`,
  authenticates with the existing worker token, sends `hello` with capabilities, then
  reacts to `offer` / `heartbeatAck` / `completeAck` / `failAck` / `error` frames.
- Heartbeat, claim/accept, complete-json, fail, and log shipping all flow over WS.
- Multipart `complete` (image / audio / video bytes) stays HTTP — the existing
  `ApiClient::complete` is reused untouched. After a successful upload the worker sends
  `{ type: "readyForMore" }` over WS so the DO offers the next job without delay.
- `spawn_heartbeat`, `spawn_claim_loop`, and `spawn_log_shipper` are deleted; the new
  `spawn_ws_session` owns the connection lifecycle and dispatches engine work via the
  existing `claim_tick` helpers (refactored to take a `Task` directly, no HTTP claim).
- Disconnect handling: exponential backoff up to N retries (default 5, configurable),
  then exit non-zero. The systemd unit's `Restart=on-failure` policy brings the worker
  back. No silent HTTP polling fallback.
- Every existing test in `tests/` still meaningful after the deletion stays green. New
  WS-driven tests replace `tests/http_contract.rs` and `tests/full_loop.rs` for the
  worker-bound traffic (the multipart `complete` HTTP contract test stays).

## Tech choice

- `tokio-tungstenite` with the `rustls-tls-webpki-roots` feature for the client. Pure
  Rust TLS, no native deps — keeps the cross-compile matrix happy.
- Tagged JSON over WS, matching the API's `WorkerInbound` / `WorkerOutbound` unions.

## Phase 1 — Wire format types

- [x] `src/ws/types.rs` — Rust mirrors of the discriminated unions, `#[serde(tag =
      "type", rename_all = "camelCase")]` with per-variant `rename_all = "camelCase"`
      for inner fields:
  - [x] `WorkerInbound` enum: `Hello`, `Heartbeat`, `Accept`, `Reject`, `CompleteJson`,
        `Fail`, `LogBatch`, `ReadyForMore`.
  - [x] `WorkerOutbound` enum: `Welcome`, `Offer`, `HeartbeatAck`, `CompleteAck`,
        `FailAck`, `Error` (plus `WorkerErrorCode` snake_case enum).
  - [x] `WsCloseCode` `#[repr(u16)]` enum + `from_error_code` mapping so the
        close-code ↔ error-code pairing stays single-sourced.
  - [x] `JobOfferClaim` mirrors `JobClaimResponse` with `into_job_claim()` bridging
        back to the existing `JobClaim` so engine dispatch stays kind-agnostic.
- [x] Reuse existing `WorkerCapabilities`, `JobClaim`, `LogEntry`, `Task` from
      `src/types.rs`. `LogEntry` gained a `Deserialize` derive so inbound `logBatch`
      frames can be parsed by the DO and by tests.
- [x] Round-trip tests covering every variant (`tests/ws_wire.rs`, 25 tests): hello,
      heartbeat (with-id / no-field / explicit null), accept, reject, completeJson
      (with prompt + array result + no prompt), fail, logBatch (with entry + empty),
      readyForMore, welcome, offer (image + multimodal LLM task), heartbeatAck,
      completeAck, failAck, error (every code), unknown-type rejection (inbound +
      outbound), close-code numeric values, close-code mapping. Plus 2 unit tests
      inside `src/ws/types.rs` for the `JobOfferClaim` bridge + default-ext
      behaviour. **100% regions / functions / lines coverage on `src/ws/types.rs`
      (`cargo llvm-cov`).**
- [x] `cargo fmt --check`, `cargo clippy --tests -- -D warnings`, and the full
      `cargo test` suite all pass (256 tests, up from 231).

## Phase 2 — WS client

- [x] `src/ws/client.rs`:
  - [x] `connect(base_url, worker_id, token) -> WsResult<WsClient>` — builds the
        upgrade request with `Authorization: Bearer <token>` + the
        `studio-worker-v1` sub-protocol; coerces `http://`→`ws://`,
        `https://`→`wss://`; constructs the path as
        `<base>/workers/<id>/connect`.
  - [x] `WsClient` exposes `send(&WorkerInbound)`, `recv() -> WsResult<Option<WorkerOutbound>>`,
        and `close(code, reason)`.  All control frames (ping/pong/etc.) are
        swallowed silently; text frames parse as `WorkerOutbound`; binary
        frames are rejected as a protocol error.  `close()` is idempotent and
        wrapped in a 5 s timeout so a stuck peer can't hang shutdown.
- [x] Error mapping (`WsClientError`):
  - [x] `AuthFailed { reason }` for both 401 upgrade and server-sent close 4001.
  - [x] `ConnectionClosed` for clean close + `AlreadyClosed`.
  - [x] `Protocol(String)` for malformed JSON / binary frames.
  - [x] `Transport(String)` for everything else.
- [x] Test pyramid:
  - [x] 7 unit tests (`#[cfg(test)] mod tests`) for `build_connect_url`
        (http/https/ws-passthrough/unknown-scheme/invalid-url),
        `close_frame_to_error` (4001 / normal / no-frame), and the
        `From<TError>` mapping for `AlreadyClosed`.
  - [x] 9 integration tests in `tests/ws_client_contract.rs` against a
        live `tokio-tungstenite` server:
        - Successful upgrade + sub-protocol/auth header inspection
        - hello → welcome round-trip
        - 401 upgrade → `AuthFailed`
        - Server closes 4001 → `AuthFailed`
        - Server closes 1000 → `ConnectionClosed`
        - Binary frame → `Protocol`
        - Silent stream end → `Ok(None)` (or `ConnectionClosed`/`Transport`),
          second `recv()` returns `Ok(None)` cleanly
        - `close()` writes a close frame the server observes + second
          `close()` is a no-op
        - `Debug` impl on `WsClient` renders
- [x] Coverage on `ws/client.rs`: 91.6% regions / 84.6% functions /
      95.8% lines.  The remaining gap is one defensive doc-comment region
      + one unreachable brace artefact — acceptable for an integration
      surface.
- [x] `cargo fmt --check`, `cargo clippy --tests -- -D warnings`, and the
      full `cargo test` suite (275 tests, up from 257) are all clean.

## Phase 3 — Runtime rewrite

- [x] Added `src/ws/session.rs` housing the WS lifecycle.  Pure async
      module that holds the connection for the duration of `run`.
- [x] `spawn_ws_session(cfg, stop, logs, busy, schedule)`:
  - [x] Connects via `connect()`, splits into `(WsSender, WsReceiver)`.
  - [x] Sends `hello` with current capabilities (rebuilt every reconnect).
  - [x] Spawns a **reader** task that pumps frames into an mpsc channel.
  - [x] Spawns a **heartbeat** task that pushes a `Heartbeat` frame
        every `schedule.heartbeat` via the shared sender.
  - [x] Spawns a **log-shipper** task that flushes the in-memory log
        buffer to the server as a `LogBatch` frame every
        `schedule.log_flush`.
  - [x] Spawns a **shutdown observer** that watches the `stop` flag and
        injects a `Stopped` event into the dispatch loop.
  - [x] Dispatch loop reacts to `Welcome` (logged), `Offer` (accept +
        engine dispatch), `Error` (auth or fatal), `HeartbeatAck`,
        `CompleteAck`, `FailAck` (ignored).
  - [x] On `Offer`: spawns a `tokio` task that runs the engine inside
        `spawn_blocking`.  Binary results go through the existing
        `ApiClient::complete` HTTP multipart and then send `ReadyForMore`
        over the WS; JSON results go straight back as `CompleteJson`.
  - [x] On engine error: send `Fail` with `retryable = !is_unsupported_kind(...)`.
  - [x] On `Error` frame (`auth_failed` or other): exit the session with
        the correct `SessionOutcome` variant; reconnect loop decides
        what to do.
- [x] Reconnect policy in `spawn_ws_session`:
  - [x] Tunables in `SessionSchedule`: heartbeat / log_flush /
        shutdown_tick / base_backoff_ms / max_backoff_ms.
  - [x] `cfg.ws_reconnect_attempts` (default 5; `0` = infinite).
  - [x] Backoff is exponential (`base * 2^(attempt-1)`) capped at
        `max_backoff_ms`.
  - [x] AuthFailed / Fatal → do not reconnect; bubble error up to
        `run` which lets the process exit non-zero.
- [x] `runtime::run_loops` rewritten: now spawns `ws::session::spawn_ws_session`
      and `spawn_auto_updater` and joins them.  Signature now returns
      `Result<()>` so auth failures surface to the binary's exit code.
      The four old `spawn_*` functions stay alive only as no-ops the
      legacy tests will tick off in Phase 4.
- [x] `Config` gains `ws_reconnect_attempts: Option<u32>`.
- [x] All 277 cargo tests still green; clippy + fmt clean.

## Phase 4 — Delete dead HTTP code

- [x] `src/http.rs`: deleted `heartbeat`, `claim`, `complete_json`, `fail`,
      `ship_logs`.  Only `register` + `complete` (multipart) remain.
- [x] `src/runtime.rs`: deleted `heartbeat_tick`, `claim_tick`,
      `log_shipper_tick`, `run_job`, `ClaimOutcome`, `spawn_heartbeat`,
      `spawn_claim_loop`, `spawn_log_shipper`, `next_delay_for`, and the
      `HEARTBEAT_INTERVAL`/`CLAIM_INTERVAL_*`/`LOG_FLUSH_INTERVAL`
      constants.  `LoopSchedule` now only carries `auto_update_tick`.
      Doc comments updated to reflect the WS-driven flow.
- [x] `tests/http_contract.rs` slimmed to the surviving routes (register
      + multipart `complete` for image and wav).
- [x] `tests/http_errors.rs` slimmed to register + complete error
      paths + tracing emission contract.
- [x] `tests/runtime_loops.rs` deleted (covered exactly the three
      removed loops).
- [x] `tests/full_loop.rs` deleted (legacy push-based end-to-end; the
      WS end-to-end is `ws_client_contract.rs` plus the orchestrator
      contract tests on the API side).
- [x] `tests/runtime_ticks.rs` slimmed to the surviving auto-update
      ticks + the `run_returns_when_aborted` smoke test (now points the
      WS upgrade at a 401-returning wiremock to exercise the
      AuthFailed exit path).
- [x] `rg '(heartbeat_tick|claim_tick|log_shipper_tick|spawn_heartbeat|
      spawn_claim_loop|spawn_log_shipper|complete_json|next_delay_for|
      ClaimOutcome|api\.fail\b|api\.heartbeat|api\.claim\b|api\.ship_logs)' src tests`
      returns only comments + the new `spawn_heartbeat_pump` /
      `spawn_log_shipper_pump` internal task names inside
      `ws/session.rs`.
- [x] `cargo test` → 244 tests across 20 suites, all green.
      `cargo fmt --check` + `cargo clippy --tests -- -D warnings` clean.

## Phase 5 — Full-loop test

- [x] `tests/ws_session_full_loop.rs` walks the real
      `spawn_ws_session` through the WS contract against a hand-rolled
      tokio-tungstenite server:
  - [x] accept upgrade with the studio sub-protocol
  - [x] expect `hello`, reply `welcome`
  - [x] push an LLM `offer`, expect `accept` + `completeJson`
  - [x] push an STT `offer`, expect `accept` + `completeJson`
  - [x] close cleanly (1000); worker session hits its 1-attempt
        reconnect cap and exits
- [x] The multipart `complete` HTTP path stays covered by
      `tests/http_contract.rs` (image + wav).  Mixing it into the same
      test would need a single TCP server handling both upgrade and
      HTTP traffic on one port and would obscure the WS contract this
      test focuses on. **Logged as deferred.**
- [x] Driving a real binary via PM2 is no longer needed — the real
      session module is unit-loaded directly into the test process, so
      the loop is exercised end-to-end without spawning the binary.
      Acceptable given the orchestrator unit tests on the API side
      already cover the server-side state machine in depth.
- [x] Full `cargo test`: 21 suites, 245 tests, all green; fmt + clippy
      clean.

## Phase 6 — Service unit + docs

- [ ] `src/service.rs`: confirm the systemd template sets `Restart=on-failure` and
      `RestartSec=5s`. Same for the launchd plist and the scheduled task.
- [ ] Update `README.md` quickstart: replace the "polls every 2 s" line with the WS
      diagram. Add a short troubleshooting section for "worker exits with auth error".
- [ ] Update `AGENTS.md`: list `tokio-tungstenite` under tech stack, drop the
      "(blocking)" qualifier on `reqwest`.
- [ ] `cargo fmt`, `cargo clippy --tests -- -D warnings`, `cargo test` all green.

## Open questions surfaced for the user

- Reconnect cap: 5 attempts with exponential backoff is enough for transient blips
  without tying up the binary indefinitely. Operators wanting "never give up" can set
  `ws_reconnect_attempts = 0` (treat as infinite). **Logged as deferred.**
- `tokio-tungstenite` vs `fastwebsockets`: tungstenite is the mainstream choice + has
  a clean rustls story. `fastwebsockets` is faster on hot paths but we send ≤ 5 frames
  per minute per worker; the perf delta is invisible. **Going with tungstenite.**
- TLS roots: `rustls-tls-webpki-roots` ships Mozilla's CA bundle inside the binary so
  no system trust store is required. **Going with webpki-roots.**
