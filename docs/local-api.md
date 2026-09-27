# Local image API

The worker exposes an always-on local HTTP API so you can generate images
(e.g. Z-Image) **without the studio**. The daemon (`studio-worker run`, which the tray UI
starts when none runs) serves it, before the studio-registration gate, so it
works even when the worker is not registered with any studio.  The tray UI is
itself a client of this API (see [daemon control](#daemon-control)).

- Bind: `127.0.0.1` only.
- Auth: every route except `GET /healthz` requires
  `Authorization: Bearer <token>`.  The token is generated once per
  install and published — together with the bound URL — in the
  owner-only discovery file `<config dir>/local-api.json`
  (`~/.config/minis-studio-worker/local-api.json` on Linux), so local
  clients can pick both up without parsing logs.  Requests with a
  non-loopback `Host` or `Origin` header are rejected with `403`
  (DNS-rebinding / CSRF guards — loopback alone is not enough against
  a hostile web page).
- Port: `4787` by default. Override with `STUDIO_WORKER_LOCAL_API_PORT`
  (or `local_api_port` in `config.toml`; the env var wins); if the
  preferred port is taken the worker falls back to an ephemeral port and logs
  the chosen URL (also shown on the tray UI's Worker page and under the Jobs page's
  Local filter, and published in the discovery file).
- Request bodies are capped at 1 MiB (`413` beyond that).
- Synchronous: `POST /image` blocks until the engine finishes and returns the
  image bytes. Each job is recorded in the in-app **Local queue**.

## Endpoints

| Method | Path            | Auth | Body / params                              | Returns |
| ------ | --------------- | ---- | ------------------------------------------ | ------- |
| POST   | `/image`        | yes  | JSON image request (below)                 | image bytes (`image/webp` etc.) |
| POST   | `/v1/chat/completions` | yes | OpenAI-compatible chat body (`model?`, `messages`, `max_tokens?`, `temperature?`, `top_p?`, `stop?`, `chat_template_kwargs?`, `stream?`, `deadline_ms?`) | `chat.completion` JSON |
| POST   | `/tts`          | yes  | `{text, model?, voice?, speed?, language?, ext?}` | audio bytes (`audio/wav` etc.) |
| POST   | `/stt`          | yes  | `{inputUrl, model?, language?}`            | transcript JSON |
| POST   | `/video`        | yes  | `{prompt, model?, negativePrompt?, seconds?, width?, height?, ext?}` | video bytes (`video/mp4` etc.) |
| POST   | `/tokenize`     | yes  | `{content, model?, add_special?}` (llama-server shape) | `{tokens: [...]}` from a loaded chat model |
| GET    | `/models`       | yes  | —                                          | catalog as JSON array, each entry with `state`, `resident`, `since`, `loadable` (+ `error` when failed) |
| GET    | `/models/:id/state` | yes | —                                        | `{id, state, resident, since, error?}` |
| POST   | `/models/:id/load`  | yes | —                                        | `202` loading / `200` loaded; marks it resident |
| POST   | `/models/:id/unload`| yes | —                                        | `202` unloading / `200` unloaded; clears residency |
| POST   | `/models`       | yes  | a catalog model (same `ModelSource` shape) | `{"ok":true}` |
| DELETE | `/models/:id`   | yes  | —                                          | `{"ok":true}` / 404; unloads it first |
| GET    | `/jobs`         | yes  | —                                          | recent local jobs as JSON |
| GET    | `/jobs/:id/log` | yes  | —                                          | the job's captured log (`404 unknown_job`) |
| GET    | `/jobs/:id/thumbnail` | yes | —                                     | `image/png` thumbnail of an image job (`404 no_thumbnail`) |
| GET    | `/daemon/status` | yes | —                                          | the tray UI's snapshot (below) |
| GET    | `/daemon/logs`  | yes  | `?after=<seq>`                             | `{entries, seq}`: worker log entries newer than `seq` |
| POST   | `/daemon/pause`, `/daemon/resume` | yes | —                      | `{paused}` |
| GET / PUT | `/daemon/config` | yes | PUT: the editable config              | the editable config (`400 invalid_config`, `500 config_not_saved`) |
| POST   | `/daemon/registration/reset` | yes | —                             | `202` / `409 not_rejected` |
| POST   | `/daemon/shutdown` | yes | —                                        | `202`; the daemon stops gracefully |
| POST   | `/stream-tokens` | yes | `{model, ttlSecs?}`                     | `{token, model, expiresAt, port, path}` for the LAN stream listener |
| GET    | `/healthz`      | no   | —                                          | runtime snapshot (below) |

### Model lifecycle

Models can be kept loaded (resident) for warm answers and unloaded to free device memory; see
[model lifecycle](runtime/model-lifecycle.md) for states, residency and admission.

Lifecycle routes answer JSON. Errors carry a stable `error` code plus a `message`:

| Status | `error` | When |
| --- | --- | --- |
| 404 | `unknown_model` | no catalogue model with that id |
| 400 | `model_disabled` | the model is disabled in the catalogue |
| 409 | `insufficient_memory` | admission refused the load; carries `neededGib`, `freeGib`, `marginGib` |
| 500 | `residency_not_saved` | the residency file could not be written; nothing changed |

```bash
curl -s -X POST "$(jq -r .url $DISCOVERY)/models/qwen3.5-0.8b/load" \
  -H "authorization: Bearer $(jq -r .token $DISCOVERY)"
# {"id":"qwen3.5-0.8b","state":"loading","resident":true,"since":"..."}
```

`loadable` is `false` for engines without an in-process loader (sd-cpp, ONNX,
synthetic): those models run per job and cannot be kept loaded.

### Daemon control

The tray UI (`studio-worker ui`) is a client of the daemon over these routes;
design in [daemon and tray UI](runtime/daemon-and-tray.md).  They carry the same
Host / Origin / token guards as every other route.  Errors answer
`{"error": <code>, "message": <text>}`.

`GET /daemon/status` answers everything the UI shows except the logs; it never
carries a credential:

```jsonc
{
  "version": "0.4.8", "pid": 4242, "configPath": "/home/you/.config/minis-studio-worker/config.toml",
  "paused": false,
  "busy": false,                       // the one-job gate is taken
  "registered": true, "workerId": "w-…",
  "registration": { "state": "approved" },   // pristine | pending{requestId,since} | approved | rejected{reason}
  "config": { "apiBaseUrl": "…", "vramThresholdGb": 12.0, "startMinimised": true,
              "autoUpdateEnabled": true, "autoUpdateIntervalSecs": 1800,
              "autoUpdateFeed": "…", "autoUpdatePrerelease": false, "modelsRoot": "…" },
  "session": { "state": "connected" },       // waiting_for_approval | connecting | connected | reconnecting{attempt} | auth_failed{reason} | fatal{reason} | stopped
  "heartbeat": { "outcome": "ok", "lastAttemptAt": "…" },
  "gpuRuntime": { "ok": true, "detail": "GPU runtime available" },
  "vramTotalGb": 24.0,
  "localApiUrl": "http://127.0.0.1:4787",
  "currentJobId": null,                      // the studio job the heartbeat reports
  "activeJobs": [ /* running jobs */ ],
  "recentJobs": [ /* finished studio jobs, newest first, up to 50 */ ],
  "localJobs":  [ /* finished local jobs, newest first, up to 50 */ ],
  "logsSeq": 1234                            // newest worker log entry
}
```

A job: `{jobId, kind, model, prompt, source: studio|local|lane|stream,
status: running|completed|failed, reason?, startedAt, finishedAt?, hasThumbnail}`.

`GET /daemon/logs?after=<seq>` answers the worker log entries (`{ts, level,
category, message, jobId?}`) newer than `seq` and the newest `seq`; pass it back
next time.  An `after` beyond the newest (the daemon restarted) answers the
whole ring (1 000 entries).

`GET /jobs/:id/log` answers `{lines: [{ts, level, target, message}], dropped}`:
the events emitted while the job ran (400 lines per job, the 128 most recent
jobs).  `GET /jobs/:id/thumbnail` answers a PNG of at most 384 px (the 100 most
recent image jobs).

`PUT /daemon/config` takes the `config` object above; the daemon validates it
(http(s) URLs, a threshold of 0 or more, an interval of at least 60 s, a
non-empty models root), saves it, then applies it.  A changed `modelsRoot`
applies to engines built after a restart.

### Streaming speech-to-text

Loaded streaming speech models (`engine: "parakeet"`) are served on a second
listener that binds the **LAN** (`0.0.0.0:4798`; `stream_port` in config or
`STUDIO_WORKER_STREAM_PORT`), so a phone on the same network can stream to it.
It accepts only short-lived **stream tokens**, minted here with the install
token, so the install token never leaves the host:

```bash
curl -s -X POST "$(jq -r .url $DISCOVERY)/stream-tokens" \
  -H "authorization: Bearer $(jq -r .token $DISCOVERY)" \
  -H 'content-type: application/json' -d '{"model":"nemotron-3.5-stream"}'
# {"token":"…","model":"nemotron-3.5-stream","expiresAt":"…","port":4798,"path":"/transcribe"}
```

A token lives 10 minutes by default (`ttlSecs`, clamped to 30 s–1 h); the holder
mints a fresh one before it expires.  Errors: `404 unknown_model`,
`400 not_a_stream_model`, `503 stream_listener_down`.

Then `ws://<host>:4798/transcribe?token=<token>`:

| Direction | Frame | Meaning |
| --- | --- | --- |
| client -> worker | binary | 16 kHz mono s16le PCM |
| client -> worker | text `end` | finalise: flush, send the final, close |
| client -> worker | text `cancel` | close without a final |
| worker -> client | `{"partial":true,"text":…}` | the transcript so far |
| worker -> client | `{"final":true,"text":…}` | the settled transcript, then close |
| worker -> client | `{"error":…}` | e.g. not loaded, busy, model unloaded |

After speech, 1.5 s of silence finalises by itself (energy VAD, RMS 0.018 over
200 ms windows).  One session per model at a time; a second gets
`busy`.  Unloading the model ends a session with `model unloaded`.  A bad or
expired token is refused at the handshake (401); any other path is 404.
Every session is recorded in the local queue.

### Health snapshot

`GET /healthz` is unauthenticated (liveness + a read-only snapshot, no
secrets or prompts) and answers even while a generation is in flight
(requests are served on a small worker pool):

```jsonc
{
  "ok": true,
  "version": "0.4.9",
  "busy": false,                 // true while a job (studio or local) runs
  "engine": "multi",
  "modelsRoot": "/home/you/models",
  "modelsRootFreeBytes": 812345678900
}
```

### Image request

```jsonc
{
  "prompt": "a red fox in snow",
  "model": "z-image-turbo-q4_k_m.gguf", // optional; default image model if omitted
  "negativePrompt": "blurry",           // optional
  "width": 1024, "height": 1024,         // optional; fall back to the model's cliDefaults
  "steps": 8,                            // optional
  "seed": 42,                            // optional
  "ext": "webp"                          // optional; webp/png/jpg/...
}
```

Example (reading the URL + token from the discovery file with `jq`):

```bash
DISCOVERY=~/.config/minis-studio-worker/local-api.json
curl -s "$(jq -r .url $DISCOVERY)/image" \
  -H "authorization: Bearer $(jq -r .token $DISCOVERY)" \
  -H 'content-type: application/json' \
  -d '{"prompt":"a red fox in snow"}' --output fox.webp
```

Errors: unknown / wrong-kind model or a bad request body return `400`; a
missing/wrong token returns `401`; a non-loopback `Host`/`Origin` returns
`403`; a body over 1 MiB returns `413`; a busy worker (a studio or local
job already running) returns `503` with `Retry-After`; an engine failure
returns `500`.

### Studio-synced models

When the worker is registered with a studio and claims a job, the
model on that offer is mirrored into the local catalog (marked
`origin: "studio"`), so a model a studio admin adds from Hugging Face
becomes usable through the local API too. Your own entries
(`origin: "local"`, the default for anything you `POST /models` or edit
by hand) are never overwritten by this sync.

### Non-image kinds

Every endpoint resolves a catalog model of the matching kind (an
explicit `model`, else the first enabled model of that kind).  Add
per-kind models the same way as image models via `POST /models`; a
request with no model of that kind in the catalog returns `400`.  The
`/v1/chat/completions` endpoint returns the engine's JSON verbatim, so
an OpenAI-style client can point straight at
`http://127.0.0.1:4787/v1/chat/completions` with the bearer token.

Chat renders the model's own chat template.  `chat_template_kwargs` (as in
llama-server) overrides the model's `chatTemplateKwargs`, e.g.
`{"enable_thinking": false}` for a hybrid-reasoning model; a `<think>` block
in the answer is returned as `reasoning_content`.  `usage` carries real token
counts, and `finish_reason` is `length` when the budget ran out.  A prompt that
does not fit the model's `contextSize` is refused, never truncated.

**Streaming.** `"stream": true` answers with server-sent events in OpenAI's
`chat.completion.chunk` shape: `delta.content` as the answer is generated (a
leading `<think>` block arrives as `delta.reasoning_content`), then a chunk with
`finish_reason` and `usage`, then `data: [DONE]`.  Stop strings are never
streamed, even when split across tokens.  Streaming needs the model loaded
(`409 model_not_loaded` otherwise); a client that disconnects ends the
generation.  An error after the stream started arrives as a final
`{"error":{"message":…}}` event.

**Deadline.** `deadline_ms` is how long the caller will wait, counted from
when the request arrived. A non-streamed chat only ever sees the client
leave once it answers, so this is how an abandoned one is freed: past the
deadline a chat still queued on the lane is dropped without running, and a
running one stops at its next prompt batch or token. The answer is `504` (a
stream ends on its error event), and the job is recorded as failed with
`deadline passed`. Without it a chat runs to the end.

**Token counts.** `POST /tokenize` returns the loaded model's token ids for
`content` (`404 unknown_model`, `409 model_not_loaded`), so a client can size
prompts against the model's `contextSize` (in its `cliDefaults` on `GET /models`).

**Model files.** An LLM file already at the top of `models_root` (same name,
same size) is used where it is; only a missing file is downloaded.

A **loaded** model answers on its own lane, next to any running job and
without the one-job gate; an unloaded model runs as a transient job (loaded
for the request, freed after).  Keep a chat model loaded
(`POST /models/:id/load`) for warm answers.  GPU offload needs a build with
the `cuda` feature; release builds run LLMs on the CPU.

## Local model catalog

Models live in a local catalog at `<config dir>/models.json`
(`~/.config/minis-studio-worker/models.json` on Linux). It mirrors the studio's
model registry: each entry carries the same `ModelSource` (engine + files +
`cliDefaults`) the studio would send on a job. The catalog is **seeded** with
Z-Image-Turbo (image), Qwen3.5 0.8B (small LLM, reasoning off, 32K context),
Nemotron 3.5 streaming and Parakeet EOU (streaming speech, one loaded at a time)
— seeds a catalogue lacks are added at startup, except ones the operator deleted
(`dismissedSeeds`) — and the files are downloaded on demand into
`models_root` (`~/models`) the first time a model is used — exactly as a
studio-driven job would.

Add a model the same way the studio does (a `ModelSource` plus a little
metadata), either by editing `models.json` or via the API:

```bash
curl -s http://127.0.0.1:4787/models \
  -H "authorization: Bearer $(jq -r .token ~/.config/minis-studio-worker/local-api.json)" \
  -H 'content-type: application/json' \
  -d '{
    "id": "my-model.gguf",
    "displayName": "My Model",
    "kind": "image",
    "vramGbEstimate": 8,
    "source": {
      "engine": "sd-cpp",
      "files": [
        {"role":"diffusion-model","url":"https://.../model.gguf","filename":"model.gguf"}
      ],
      "cliDefaults": {"cfgScale":1.0,"steps":8,"width":1024,"height":1024,"samplingMethod":"euler"}
    },
    "enabled": true
  }'
```

## Local queue in the app

Local jobs are kept in their own ring (`WorkerObservers::local_jobs`), separate
from studio-claimed jobs, and shown in the tray UI's Jobs page history (the
**Local** filter shows them alone, with the API URL), each with its log and, for
images, a thumbnail that opens larger.
Chats served on a loaded model's lane (`source: lane`) and streaming speech
sessions (`source: stream`) are local jobs too.

## Notes

- The local API runs on its own thread and never blocks the studio session
  loop. Heavy generation (sd.cpp) runs the same engine path as studio jobs.
- It does not serialise GPU access with the studio session; if you both run
  studio jobs and call the local API on the same box, avoid overlapping heavy
  generations to stay within VRAM.
