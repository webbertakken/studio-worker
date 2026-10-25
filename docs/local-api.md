# Local image API

The worker exposes an always-on local HTTP API so you can generate images
(e.g. Z-Image) **without the studio**. The daemon (which the tray UI
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
- Request bodies are capped at 1 MiB (`413` beyond that); `POST /image` and
  `POST /v1/chat/completions` at 64 MiB, room for inline input images.
- Synchronous: `POST /image` blocks until the engine finishes and returns the
  image bytes. Each job is recorded in the in-app **Local queue**.

## Endpoints

| Method | Path            | Auth | Body / params                              | Returns |
| ------ | --------------- | ---- | ------------------------------------------ | ------- |
| POST   | `/image`        | yes  | JSON image request (below)                 | image bytes (`image/webp` etc.) |
| POST   | `/v1/chat/completions` | yes | OpenAI-compatible chat body (`model?`, `messages`, `max_tokens?`, `temperature?`, `top_p?`, `stop?`, `chat_template_kwargs?`, `stream?`, `deadline_ms?`, `images?`, `response_format?`) | `chat.completion` JSON |
| POST   | `/tts`          | yes  | `{text, model?, voice?, speed?, language?, ext?}` | audio bytes (`audio/wav` etc.) |
| POST   | `/stt`          | yes  | `{inputUrl, model?, language?}`            | transcript JSON |
| POST   | `/video`        | yes  | `{prompt, model?, negativePrompt?, seconds?, width?, height?, ext?}` | video bytes (`video/mp4` etc.) |
| POST   | `/tokenize`     | yes  | `{content, model?, add_special?}` (llama-server shape) | `{tokens: [...]}` from a loaded chat model |
| POST   | `/mesh`         | yes  | a picture (raw, multipart or JSON) + `model?`, `seed?` | a 3D model, `model/gltf-binary` ([mesh](#mesh)) |
| POST   | `/vision/detect` | yes | a picture (raw, multipart or JSON) + `model?`, `minConfidence?`, `iou?` | animals, people, vehicles: boxes and `animalPresent` ([vision](#vision)) |
| POST   | `/vision/classify` | yes | a picture + `model?`, `minConfidence?`, `topK?` | ranked classes with confidence and taxonomy |
| POST   | `/vision/species` | yes | a picture + `detector?`, `classifier?`, `minConfidence?`, `iou?`, `topK?` | each detection; each animal with its ranked species |
| GET    | `/models`       | yes  | —                                          | catalog as JSON array, each entry with `state`, `resident`, `since`, `loadable`, `images` (+ `error` when failed) |
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
| POST   | `/voice-sample-tokens` | yes | `{ttlSecs?}` | `{token, expiresAt, scheme, host, port, path, certSha256, certPem}`: a grant to upload takes on the LAN listener ([recordings](#recordings-the-operators-voice)) |
| GET    | `/voice-samples` | yes | - | `{sets: [{set, positive, nearMiss, speech, seconds, bytes, updatedAt, inTraining}]}` |
| DELETE | `/voice-samples`, `/voice-samples/:set` | yes | - | `{deleted: [...]}`; `404 unknown_set`, `409 set_in_training` |
| POST   | `/wake-words`   | yes  | `{phrase, name?, displayName?, languages?, spellings?, negatives?, author?, website?, targetFalseAcceptsPerHour?, voiceSamples?, settings?}` | `202` the training: trains a microWakeWord model ([wake word training](engines/wake-word.md)) |
| GET    | `/wake-words`, `/wake-words/:id` | yes | - | the trainings, newest first; one training (`404 unknown_training`) |
| DELETE | `/wake-words/:id` | yes | -                                       | `202`; stops a running training (`409 not_running`) |
| GET    | `/wake-words/:id/files/:file` | yes | - | a completed training's `<name>.tflite`, `<name>.json`, `report.json` or `report.md` ([output files](engines/wake-word.md#output-files)); `404 unknown_training`, `404 unknown_file` |
| POST   | `/stream-tokens` | yes | `{model, ttlSecs?}`                     | `{token, model, expiresAt, scheme, host, port, path, certSha256, certPem}` for the LAN stream listener |
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
  "config": { "apiBaseUrl": "…", "vramThresholdGb": 12.0, "memoryMaxGb": 32, "autoStart": true, "startMinimised": true, "onlyWhenIdle": false,
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
(http(s) URLs, a threshold of 0 or more, a memory ceiling of 4 to 1024 GB, an
interval of at least 60 s, a non-empty models root), saves it, then applies it.
A changed `modelsRoot` applies to engines built after a restart; a changed
`memoryMaxGb` applies at once to a daemon inside `studio-worker.scope` (see
[memory ceiling](runtime/memory-ceiling.md)).

### Streaming speech-to-text

Loaded streaming speech models (`engine: "parakeet"`) are served on a second
listener that binds the **LAN** (`0.0.0.0:4798`; `stream_port` in config or
`STUDIO_WORKER_STREAM_PORT`), so a phone on the same network can stream to it.
It speaks **TLS only** (`wss://`), so neither the audio nor the token crosses the
LAN in the clear, and it accepts only short-lived **stream tokens**, minted here
with the install token, so the install token never leaves the host:

```bash
curl -s -X POST "$(jq -r .url $DISCOVERY)/stream-tokens" \
  -H "authorization: Bearer $(jq -r .token $DISCOVERY)" \
  -H 'content-type: application/json' -d '{"model":"nemotron-3.5-stream"}'
# {"token":"…","model":"nemotron-3.5-stream","expiresAt":"…","scheme":"wss","host":"192.168.1.133",
#  "port":4798,"path":"/transcribe","certSha256":"<64 hex>","certPem":"-----BEGIN CERTIFICATE-----…"}
```

A token lives 10 minutes by default (`ttlSecs`, clamped to 30 s–1 h); the holder
mints a fresh one before it expires.  Errors: `404 unknown_model`,
`400 not_a_stream_model`, `503 stream_listener_down` (the listener is not
listening, including when its certificate could not be loaded or made).

#### The listener's LAN address

- `host` is the address a LAN client dials: this machine's address on the network its default
  route leaves by, worked out afresh for every grant, so a new DHCP lease reaches the next grant
  without a restart.
- It is the source address the kernel picks for traffic to `192.0.2.1` (TEST-NET-1, a route
  lookup through a connected UDP socket; no packet is sent), never the first address of the first
  interface: a docker or LXC bridge (`docker0`, `br-…`, `lxcbr0`) holds private addresses too,
  but traffic for the outside never leaves by it.
- Only a private IPv4 address (`10/8`, `172.16/12`, `192.168/16`) is a LAN address. Loopback,
  link-local, carrier-grade NAT (`100.64/10`, a Tailscale address), a public address, or no route
  at all gives `host: null` and a warning naming why; the rest of the grant still holds, for a
  loopback client that dials `127.0.0.1`.
- A VPN that takes the default route with a private address is chosen like a LAN: the address it
  reports is then the tunnel's.

#### The listener's certificate

- The listener presents one self-signed certificate (ECDSA P-256, valid 25 years from its
  creation), made on the daemon's first start and kept next to the config:
  `stream-cert.pem` (public) and `stream-key.pem` (owner-only; never logged, never served).
- Its names are `studio-worker`, `localhost`, `127.0.0.1` and `::1`.  A LAN client dials an
  address the certificate does not name, so it **pins** the certificate rather than checking
  the name.
- `certSha256` is the lowercase hex SHA-256 of the certificate's DER encoding: the pin.
  `certPem` is the same certificate, for a loopback client that trusts it as its only root.
  Neither is a secret; both reach the holder over this loopback API, behind the install token.
- A pair that is missing, unreadable or mismatched is replaced with a new one at start (logged
  as a warning).  To rotate, delete both files and restart the daemon; every grant minted
  after that carries the new pin.

#### Opening a stream

`wss://<host>:4798/transcribe`, optionally with `?endpoint=server` (the default) or
`?endpoint=client`, with the token in the handshake's `Authorization: Bearer <token>`
header.  The client trusts exactly the certificate whose SHA-256 is `certSha256`, and
nothing else.  Refused at the handshake:

| Attempt | Outcome |
| --- | --- |
| plain `ws://` (no TLS) | the TLS handshake fails; nothing is served |
| a `token` in the query string, valid or not | `400` |
| no `Authorization: Bearer` header | `401` |
| a bad or expired token | `401` |
| any path but `/transcribe` | `404` |

The frames:

| Direction | Frame | Meaning |
| --- | --- | --- |
| client -> worker | binary | 16 kHz mono s16le PCM |
| client -> worker | text `end` | finalise: flush, send the final, close |
| client -> worker | text `cancel` | close without a final |
| worker -> client | `{"partial":true,"text":…}` | the transcript so far |
| worker -> client | `{"final":true,"text":…}` | the settled transcript, then close |
| worker -> client | `{"error":…}` | e.g. not loaded, busy, model unloaded |

Who ends the utterance:

- `endpoint=server` (default): after speech, 1.5 s of silence finalises by itself (energy
  VAD, RMS 0.018 over 200 ms windows).
- `endpoint=client`: pauses never finalise; the client sends `end` when the user has
  finished.  30 s of post-speech silence still finalises, so a client that never sends
  `end` cannot hold the model.

Any other `endpoint` value is refused at the handshake (400).  One session per model at a time; a second gets
`busy`.  Unloading the model ends a session with `model unloaded`.
Every session is recorded in the local queue; every refusal is logged as `stream refused`
(target `studio_worker::stt_stream`) with its reason, never with a token.

### Recordings (the operator's voice)

The operator's own takes, recorded on his phone for wake word training, reach the worker over
the same LAN listener, on its own route, with a **recording token** of its own. A stream token
never uploads, and a recording token never opens `/transcribe`. Design and rules:
[wake word training, "The operator's voice"](engines/wake-word.md#the-operators-voice).

#### Recording grant (loopback)

```bash
curl -s -X POST "$(jq -r .url $DISCOVERY)/voice-sample-tokens" \
  -H "authorization: Bearer $(jq -r .token $DISCOVERY)" \
  -H 'content-type: application/json' -d '{"ttlSecs":600}'
# {"token":"<64 hex>","expiresAt":"…","scheme":"https","host":"192.168.1.133","port":4798,
#  "path":"/voice-samples","certSha256":"<64 hex>","certPem":"-----BEGIN CERTIFICATE-----…"}
```

The body may be empty. `ttlSecs` defaults to 600 and is clamped to 30 s–1 h; the holder mints
a fresh grant before it expires. `host`, the certificate and its pin are those of the stream
grant (above). Errors: `400 bad_request` (an unknown field), `503 voice_samples_unavailable`,
`503 stream_listener_down`.

#### Uploading a take (LAN listener)

One take per request, to `https://<host>:<port><path>/<set>/<kind>/<take>.wav`, trusting
exactly the certificate whose SHA-256 is `certSha256`:

```http
PUT /voice-samples/2026-10-26/near_miss/hey_luna-002.wav HTTP/1.1
Authorization: Bearer <recording token>
Content-Type: audio/wav
Content-Length: 64044

<the WAV>
```

| Part | Rule |
| --- | --- |
| `set` | `[a-z0-9][a-z0-9_-]{0,63}`: one sitting of the recording guide |
| `kind` | `positive` (the phrase), `near_miss`, `speech` (talk without the phrase) |
| `take` of `positive`, `speech` | its number, 1 to 4 digits: `007` |
| `take` of `near_miss` | the near miss's slug and its number: `hey_luna-002` (slug `[a-z0-9]+(_[a-z0-9]+)*`) |
| body | a WAV: 16 kHz, mono, 16-bit PCM, 0.25 to 31 s, at most 1 MiB; `Content-Length` required |

The take's name is its identity: uploading the same take again replaces it (a redo).
`201` a new take, `200` a take replaced or unchanged:

```json
{"set":"2026-10-26","kind":"near_miss","take":"hey_luna-002","number":2,"phrase":"hey_luna",
 "seconds":2.0,"bytes":64044,"created":true}
```

Refusals answer `{"error", "message"}`: `400 bad_name`, `400 bad_wav`, `401 unauthorized`,
`404 not_found` (any other path), `405 method_not_allowed`, `409 set_in_training` (a training
holds the set: retry when it ends), `411 length_required`, `413 too_large`, `500 not_stored`,
`503 voice_samples_unavailable`. Every upload is logged (target `studio_worker::voice_samples`)
with its path and status, never its token or audio.

Takes live in `<config dir>/voice-samples/<set>/<kind>/<take>.wav`, directories 0700 and files
0600, and nowhere else.

#### Sets (loopback)

| Method | Path | Returns |
| --- | --- | --- |
| `GET` | `/voice-samples` | `{sets: [...]}`, newest first (below) |
| `DELETE` | `/voice-samples/<set>` | `{deleted: ["<set>"]}`; `404 unknown_set`, `400 bad_name` |
| `DELETE` | `/voice-samples` | `{deleted: [...]}`: every set |

```json
{"sets":[{"set":"2026-10-26","positive":60,"nearMiss":{"hey_luna":3,"runner":3},"speech":3,
  "seconds":228.0,"bytes":7296600,"updatedAt":"…","inTraining":false}]}
```

A delete that reaches a set a training holds answers `409 set_in_training` and deletes nothing.

#### Training with a set

`POST /wake-words` takes `voiceSamples: ["<set>", …]` (up to 8). Every 4th positive by take
number (a number divisible by 4) is held out and measured apart; the rest train beside the
synthetic voices; near-miss and speech takes train as negatives. The training's `result`
carries `ownVoiceRecall: {recall, accepted, heldOut}` (`null` without sets), and its report a
"Your own voice" section. Refusals: `400 invalid_voice_samples` (an unknown set, or not enough
positives to hold one out and train one), `503 voice_samples_unavailable`.

### Wake word output files

A completed training's files, for a client that cannot read `models_root` (Runa runs in a
container). Only the four outputs are served, found on disk by the training's id, so they
outlive a daemon restart (`GET /wake-words/:id` lists this daemon's trainings only):

```bash
for f in hey_runa.tflite hey_runa.json report.json report.md; do
  curl -sf -o "$f" "$(jq -r .url $DISCOVERY)/wake-words/$ID/files/$f" \
    -H "authorization: Bearer $(jq -r .token $DISCOVERY)"
done
```

| File | Content type |
| --- | --- |
| `<name>.tflite` | `application/octet-stream` |
| `<name>.json`, `report.json` | `application/json` |
| `report.md` | `text/markdown; charset=utf-8` |

`<name>` is the training's `name` (`hey_runa`). Anything else answers `404 unknown_file`; an
id that is not a training's, or a training without completed output, `404 unknown_training`.

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
  "ext": "webp",                         // optional; webp/png/jpg/...
  "initImageUrl": "data:image/png;base64,...", // optional; img2img base, or the original of a LaMa removal
  "refImageUrl": "data:image/png;base64,...",  // optional; instruction-edit reference (sd-cli -r)
  "maskUrl": "data:image/png;base64,...",      // optional; white = the region the model may change
  "denoise": 0.8                               // optional; img2img strength
}
```

Input images travel inline as `data:image/<type>;base64,` URLs, so an edit, a masked removal or an
inpaint runs on the local API through the same engine paths a studio job takes. Any other address
is refused (`400`): the local API never makes the worker fetch a URL a caller names. cfg and the
sampler come from the model's `cliDefaults`, as for a studio job.

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
`403`; a body over its cap returns `413`; a busy worker (a studio or local
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

Chat renders the model's own chat template, with llama.cpp's template helpers:
`raise_exception(message)` fails the render with the template's message, and
`strftime_now(format)` is the local time in a strftime format (Mistral Small 3.2
dates its system prompt with it); a format it cannot read is a named error.
`chat_template_kwargs` (as in
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

**Images.** `images` is a list of inline `data:image/<type>;base64,` pictures (PNG, JPEG, WebP),
read in order with the last user message, as a studio job's `images` are. Only a model whose
source carries a vision projector (a `text-encoder-vision` file; `images: true` on `GET /models`)
reads them; any other model answers `400 … reads no images` and nothing runs. Any other address is
refused (`400`): the local API never makes the worker fetch a URL a caller names. At most 16
pictures, 25 MiB each. See [vision](engines/llama.md#vision).

**Structured output.** `response_format: {"type": "json_schema", "json_schema": {"schema": {…}}}`
constrains the answer to the schema (a llama.cpp grammar; a studio job's `jsonSchema` does the
same); `{"type": "text"}` leaves it free; any other type is refused (`400`).

```bash
curl -s "$(jq -r .url $DISCOVERY)/v1/chat/completions" \
  -H "authorization: Bearer $(jq -r .token $DISCOVERY)" -H 'content-type: application/json' \
  -d "{\"model\":\"qwen3.5-9b-vision\",\"messages\":[{\"role\":\"user\",\"content\":\"What is this?\"}],
       \"images\":[\"data:image/webp;base64,$(base64 -w0 picture.webp)\"]}"
```

**Token counts.** `POST /tokenize` returns the loaded model's token ids for
`content` (`404 unknown_model`, `409 model_not_loaded`), so a client can size
prompts against the model's `contextSize` (in its `cliDefaults` on `GET /models`).

**Model files.** An LLM file already at the top of `models_root` (same name,
same size) is used where it is; only a missing file is downloaded.

A **loaded** model answers on its own lane, next to any running job and
without the one-job gate; an unloaded model runs as a transient job (loaded
for the request, freed after).  Keep a chat model loaded
(`POST /models/:id/load`) for warm answers.  GPU offload (every layer) needs a
build with the `cuda` feature: the CUDA release build on x86_64 Linux, which the
installer picks when the NVIDIA driver is present
([release: CUDA variant](operations/release.md#cuda-variant)).  Other builds run
LLMs on the CPU.

### Vision

Animal detection and species classification; models, pipeline and GPU use in
[vision](engines/vision.md).  Each route takes one picture (PNG, JPEG or WebP, at most 25 MiB)
in any of three ways:

- the raw body, `Content-Type: image/*` or `application/octet-stream`, options in the query;
- `multipart/form-data` with the picture as the `image` file and options as fields;
- JSON, `{"image": "data:image/jpeg;base64,…" | "<base64>", …options}`.

Options in the body override the query.  An option the route does not read is refused by name
(`400`), as is a threshold outside 0..1 or a `topK` outside 1..100.  Without `model` (or
`detector` / `classifier`) the first enabled model doing that task answers.

```bash
D=~/.config/minis-studio-worker/local-api.json
URL=$(jq -r .url $D); AUTH="authorization: Bearer $(jq -r .token $D)"

# Is anything there?  The raw picture, options in the query.
curl -s "$URL/vision/detect?minConfidence=0.3" -H "$AUTH" \
  -H 'content-type: image/jpeg' --data-binary @trap.jpg
# {"model":"megadetector-v5a","device":"cuda","picture":{"width":1920,"height":1080},
#  "animalPresent":true,"detections":[{"label":"animal","classId":0,"confidence":0.94,
#  "animal":true,"box":{"x":812.4,"y":430.1,"width":220.7,"height":180.2},
#  "boxNormalised":{"x":0.42,"y":0.40,"width":0.11,"height":0.17}}],"elapsedMs":180}

# Which bird?  A form.
curl -s "$URL/vision/classify" -H "$AUTH" -F image=@robin.jpg -F topK=3
# {"model":"bird-species-efficientnet-b2","device":"cuda","picture":{…},
#  "predictions":[{"label":"EUROPEAN ROBIN","classId":…,"confidence":0.97},…],"elapsedMs":25}

# Both: every animal found, each classified on its own crop.
curl -s "$URL/vision/species" -H "$AUTH" -H 'content-type: application/json' \
  -d "{\"image\":\"$(base64 -w0 trap.jpg)\",\"topK\":3}"
# {"detector":"megadetector-v5a","classifier":"bird-species-efficientnet-b2",
#  "animalPresent":true,"detections":[{"label":"animal",…,"species":[{"label":…}]}],…}
```

`animalPresent: false` means the picture is blank (no animal class at or above the threshold);
`/vision/species` then never runs the classifier.  Each `species` entry ranks one animal's crop;
a person or vehicle has none, nor an animal past the 20 strongest.  On `species`,
`minConfidence` is the detector's threshold and `topK` the classifier's.  A `taxonomy`
(`class`, `order`, `family`, `genus`, `species`, `scientificName`) comes with a prediction
when the classifier's labels carry one.

Errors are JSON `{error, message}`:

| Status | `error` | When |
| --- | --- | --- |
| 400 | `bad_request` | no picture, an undecodable one, a bad or unknown option |
| 400 | `wrong_model`, `model_disabled` | the model does not do this task, or is disabled |
| 404 | `unknown_model`, `no_model` | the model is not in the catalogue, or none does the task |
| 409 | `insufficient_memory` | a transient job does not fit next to the loaded models |
| 413 | `picture_too_large` | the picture is over 25 MiB (a body over 64 MiB is refused before it is read) |
| 415 | `unsupported_media_type` | a `Content-Type` other than the three above |
| 500 | `inference_failed` | the model failed to download, load or run; the message says why |
| 501 | `not_served` | the model is no vision model in this build (no `vision` feature) |
| 503 | `busy` | a transient job while another job holds the gate (`Retry-After: 2`) |

A loaded vision model answers on its lane without the gate; keep the detector loaded for a
batch of camera trap pictures.

### Mesh

A picture in, a 3D model out, made by the [torch engine](engines/torch.md) (Pixal3D, TRELLIS.2,
Hunyuan3D 2.1) or, for `synthetic-mesh`, a textured cube in the picture's average colour.  The
picture arrives in any of the three ways [vision](#vision) reads; the options are `model` and
`seed`.  It runs as a transient job (one at a time, admitted next to the loaded models): a real
model takes minutes, and its first job provisions its environment and weights (about 15 minutes
once), so give the client a long timeout.

```bash
D=~/.config/minis-studio-worker/local-api.json
curl -s --max-time 3300 "$(jq -r .url $D)/mesh?model=pixal3d&seed=42" \
  -H "authorization: Bearer $(jq -r .token $D)" -H 'content-type: image/png' \
  --data-binary @turtle.png -o turtle.glb
```

## Local model catalog

Models live in a local catalog at `<config dir>/models.json`
(`~/.config/minis-studio-worker/models.json` on Linux). It mirrors the studio's
model registry: each entry carries the same `ModelSource` (engine + files +
`cliDefaults`) the studio would send on a job. The catalog is **seeded** with
Z-Image-Turbo (image), Qwen3.5 0.8B and 2B (LLMs, reasoning off, 32K context; the 2B
needs about 3 GB of VRAM at 32K),
Nemotron 3.5 streaming and Parakeet EOU (streaming speech, one loaded at a time),
MegaDetector v5a, SpeciesNet v4.0.1a and a 525-species bird classifier ([vision](engines/vision.md))
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
