# Local image API

The worker exposes an always-on local HTTP API so you can generate images
(e.g. Z-Image) **without the studio**. It starts automatically whenever the
worker runs (`run` or the desktop UI), before the studio-registration gate, so
it works even when the worker is not registered with any studio.

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
  the chosen URL (also published in the UI's Jobs tab and the discovery file).
- Request bodies are capped at 1 MiB (`413` beyond that).
- Synchronous: `POST /image` blocks until the engine finishes and returns the
  image bytes. Each job is recorded in the in-app **Local queue**.

## Endpoints

| Method | Path            | Auth | Body / params                              | Returns |
| ------ | --------------- | ---- | ------------------------------------------ | ------- |
| POST   | `/image`        | yes  | JSON image request (below)                 | image bytes (`image/webp` etc.) |
| GET    | `/models`       | yes  | —                                          | catalog as JSON array |
| POST   | `/models`       | yes  | a catalog model (same `ModelSource` shape) | `{"ok":true}` |
| DELETE | `/models/:id`   | yes  | —                                          | `{"ok":true}` / 404 |
| GET    | `/jobs`         | yes  | —                                          | recent local jobs as JSON |
| GET    | `/healthz`      | no   | —                                          | runtime snapshot (below) |

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

Errors: unknown / non-image model or a bad request body return `400`; a
missing/wrong token returns `401`; a non-loopback `Host`/`Origin` returns
`403`; a body over 1 MiB returns `413`; an engine failure returns `500`.

## Local model catalog

Models live in a local catalog at `<config dir>/models.json`
(`~/.config/minis-studio-worker/models.json` on Linux). It mirrors the studio's
model registry: each entry carries the same `ModelSource` (engine + files +
`cliDefaults`) the studio would send on a job. The catalog is **seeded with
Z-Image-Turbo** on first run, and the files are downloaded on demand into
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
from studio-claimed jobs, and shown under **Local queue** in the desktop UI's
Jobs tab alongside the API URL.

## Notes

- The local API runs on its own thread and never blocks the studio session
  loop. Heavy generation (sd.cpp) runs the same engine path as studio jobs.
- It does not serialise GPU access with the studio session; if you both run
  studio jobs and call the local API on the same box, avoid overlapping heavy
  generations to stay within VRAM.
