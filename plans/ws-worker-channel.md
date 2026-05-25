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

- [ ] `src/ws/client.rs`:
  - [ ] `connect(base_url, worker_id, token) -> Result<WsStream>`. Build the upgrade
        request with `Authorization: Bearer <token>` + `Sec-WebSocket-Protocol:
        studio-worker-v1`. Coerce `https://` → `wss://` and `http://` → `ws://`.
  - [ ] `WsSession` wrapper exposing `send(WorkerInbound)` + `recv() -> WorkerOutbound`
        + `close(code, reason)`. Uses an mpsc to serialise writes from multiple tasks
        (heartbeat task + dispatcher task).
- [ ] Error mapping: surface 401 upgrade response as a clear "auth failed, re-register"
      message (mirrors the existing register friendly-error hint).
- [ ] Test against a `tokio-tungstenite` server in `tests/ws_client_contract.rs`:
  - [ ] Successful upgrade + hello round-trip.
  - [ ] 401 upgrade → friendly auth error returned to caller.
  - [ ] Server closes with 4001 → client surfaces a typed `AuthFailed` error.

## Phase 3 — Runtime rewrite

- [ ] Delete `spawn_heartbeat`, `spawn_claim_loop`, `spawn_log_shipper` from
      `src/runtime.rs` along with their `LoopSchedule` fields. Keep `spawn_auto_updater`
      untouched.
- [ ] New `spawn_ws_session(api, config, engine, logs, busy, stop)` owning the WS for
      the lifetime of the run. Internal structure:
  - [ ] `tokio::select!` over: incoming WS frame, heartbeat tick (every 5 s),
        log-flush tick (every 1 s), shutdown signal.
  - [ ] On `Offer`: spawn a `tokio::task::spawn_blocking` for engine dispatch
        (matches today's pattern). On success, send `CompleteJson` for JSON kinds or
        run `ApiClient::complete` for binary kinds, then send `ReadyForMore`.
  - [ ] On engine error: send `Fail` with `retryable` derived from the error type.
  - [ ] On `error` frame from server (e.g. 4003 duplicate worker): log + exit non-zero.
- [ ] Refactor `claim_tick` → `dispatch_offer` taking a `JobClaim` directly (no HTTP
      call inside; the WS supplies the claim). Keep the public outer signature so the
      existing dispatch tests in `tests/runtime_ticks.rs` still cover engine error
      paths.
- [ ] Reconnect policy:
  - [ ] `ws_reconnect_attempts` (default 5, config-toml + `--ws-reconnect-attempts`),
        `ws_reconnect_backoff_ms_base` (default 1000) with `2^n` jitter.
  - [ ] After N failures: emit a final ERROR log, `exit(1)`. The systemd / launchd unit
        restarts us.

## Phase 4 — Delete dead HTTP code

- [ ] `src/http.rs`: delete `heartbeat`, `claim`, `complete_json`, `fail`,
      `ship_logs` methods. Keep `register` + `complete` (multipart).
- [ ] Delete `tests/http_contract.rs` for the deleted endpoints (the multipart
      contract test for `complete` survives, slimmed down).
- [ ] Delete `tests/runtime_loops.rs` (covers the three deleted loops); fold the
      still-useful assertions about engine dispatch into a new
      `tests/ws_session_loop.rs`.
- [ ] `rg "heartbeat|claim_idle|claim_after_null|/logs|complete-json|/fail" src` →
      zero hits outside `update.rs` (auto-update uses HEAD/GET only).

## Phase 5 — Full-loop test

- [ ] `tests/full_loop.rs` rewritten:
  - [ ] Boot a tiny tungstenite server that mimics the DO's protocol: accept hello,
        push an `offer` for a synthetic image, expect a `completeJson` (for an LLM
        offer) and a multipart HTTP `complete` (for image — served by a sibling
        `wiremock` instance), push a `readyForMore` follow-up offer, then close 1000.
  - [ ] Drive a real `studio-worker` process via PM2 (already the test-harness
        pattern) and assert all four offers complete in < 5 s.

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
