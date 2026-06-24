# Local image API

The worker exposes an always-on local HTTP API so you can generate images
(e.g. Z-Image) **without the studio**. It starts automatically whenever the
worker runs (`run` or the desktop UI), before the studio-registration gate, so
it works even when the worker is not registered with any studio.

- Bind: `127.0.0.1` only, no auth (local-only by design).
- Port: `4787` by default. Override with `STUDIO_WORKER_LOCAL_API_PORT`; if the
  preferred port is taken the worker falls back to an ephemeral port and logs
  the chosen URL (also published in the UI's Jobs tab).
- Synchronous: `POST /image` blocks until the engine finishes and returns the
  image bytes. Each job is recorded in the in-app **Local queue**.

## Endpoints

| Method | Path            | Body / params                              | Returns |
| ------ | --------------- | ------------------------------------------ | ------- |
| POST   | `/image`        | JSON image request (below)                 | image bytes (`image/webp` etc.) |
| GET    | `/models`       | —                                          | catalog as JSON array |
| POST   | `/models`       | a catalog model (same `ModelSource` shape) | `{"ok":true}` |
| DELETE | `/models/:id`   | —                                          | `{"ok":true}` / 404 |
| GET    | `/jobs`         | —                                          | recent local jobs as JSON |
| GET    | `/healthz`      | —                                          | `{"ok":true}` |

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

Example:

```bash
curl -s http://127.0.0.1:4787/image \
  -H 'content-type: application/json' \
  -d '{"prompt":"a red fox in snow"}' --output fox.webp
```

Errors: unknown / non-image model or a bad request body return `400`; an engine
failure returns `500`.

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
