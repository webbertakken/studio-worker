# ModelSource: studio-driven download spec

The studio is the single source of truth for which files a worker
needs to download to serve a given model.  When a job is queued (or
promoted, or retried), the studio resolves the job's model name
against its D1-backed registry (the `studioModels` table, via
`apps/studio/src/worker/modules/graphics/resolveModelSource.ts`)
and persists the resolved `ModelSource` JSON onto the row's
`modelSource` column.  Every Offer frame the studio sends over WS
includes that JSON.  The worker is dumb: it downloads what it's
told, runs the engine the source names, returns the bytes.

This design supersedes the earlier worker-side approach where each
engine had to hardcode a list of model names + HF URLs it could
serve.  See [`docs/architecture/overview.md`](../architecture/overview.md#engine-abstraction)
for how the multi-engine routes by `source.engine`.

## Wire shape

JSON-serialised on the WS Offer, mirrored on both sides:

```jsonc
{
  "engine": "sd-cpp",          // | "llama-cpp" | "onnx" | "synthetic"
  "files": [
    {
      "role": "diffusion-model",  // | "text-encoder" | "vae" | "lora" | "model"
      "url": "https://huggingface.co/.../z_image_turbo-Q4_K.gguf",
      "filename": "z_image_turbo-Q4_K.gguf",
      "approxBytes": 2_700_000_000  // optional; lets the UI show progress
    },
    { "role": "text-encoder", "url": "...", "filename": "..." },
    { "role": "vae",          "url": "...", "filename": "..." }
  ],
  "cliDefaults": {
    "cfgScale": 1.0,
    "steps": 8,
    "width": 1024,
    "height": 1024,
    "samplingMethod": "euler"
  }
}
```

- TS source of truth: `apps/studio/src/shared/types/worker.ts`
  (`WorkerModelSource` + `WorkerModelFile` + `WorkerModelCliDefaults`).
- Rust mirror: [`src/types.rs`](../../src/types.rs) (`ModelSource`,
  `ModelFile`, `ModelFileRole`, `ModelEngine`, `ModelCliDefaults`).

`role` maps onto `sd-cli` / `llama-cpp` CLI flags:

| Role | sd-cpp CLI flag |
|---|---|
| `diffusion-model` | `--diffusion-model` |
| `text-encoder`    | `--llm`              |
| `vae`             | `--vae`              |
| `lora`            | `--lora-model-dir`   |
| `model`           | `--model` (single-file packaging) |

## Studio side

The registry lives in the `studioModels` D1 table (originally a
hand-maintained in-tree `modelRegistry.ts`; migrated to D1 with
migration `0017_seed_registry.sql` + an admin CRUD surface in
`routes/models.ts`).  Rows are keyed by the `model` field the studio
writes onto a `graphicsJobs` row; a missing or disabled row is a 400
at queue time.

The studio calls `resolveModelSourceFromDatabase(database, model)` at
three write sites:

| Site | Where |
|---|---|
| `POST /jobs/create` | `apps/studio/src/worker/modules/graphics/routes/queue.ts` |
| `POST /jobs/promote` | same file |
| `POST /jobs/:id/retry` | same file |

Each stuffs `JSON.stringify(modelSource)` into the row's `modelSource`
column.  When the orchestrator's `commitOffer` builds an outgoing
`offer` frame, it parses the JSON via `parseModelSourceJson` (which
defensively returns `undefined` on legacy / malformed rows) and
attaches it to the claim.

Schema: `graphicsJobs.modelSource TEXT` added in migration
`0015_model_source.sql`.

## Worker side

The WS session passes the source through to
`engine.dispatch_with_source(model, task, source)`.  The MultiEngine
inspects `source.engine`:

```rust
match source.engine {
    ModelEngine::SdCpp => "sdcpp",
    ModelEngine::LlamaCpp => "llama",
    ModelEngine::Synthetic => "synthetic",
}
```

\u2026and forwards to the matching sub-engine.  See
[`docs/engines/sdcpp.md`](../engines/sdcpp.md) for what `sd-cpp`
actually does with the source (download, cache, invoke `sd-cli`).

If the offer arrives **without** a `modelSource` (legacy rows queued
before migration `0015`, or a new engine the studio hasn't taught yet),
sdcpp `dispatch_with_source` bails with a clear error message; the
worker reports the fail and the job lands in terminal `failed`
status.  This is by design \u2014 we'd rather surface "the studio's
registry doesn't have this model" than silently fall back to
synthetic and upload placeholder bytes.

## Backwards-compatibility

The `JobClaim` (HTTP shape) carries `model_source: Option<ModelSource>`
with `#[serde(default, skip_serializing_if = "Option::is_none")]`,
and the TS side's `JobClaimResponse.modelSource` is `optional`.  An
older worker / older studio that doesn't know about the field stays
silent.  Once both sides land the change the offer just gets
richer.

## Adding a new model

1. Add a `studioModels` row via the studio's Models admin page (or a
   seed migration), keyed by the model name the operator will type
   into the studio UI / promote with.
2. Verify each `url` resolves to a public HF / GitHub asset
   (`curl -I` should return `200` or `302` without auth).
3. No worker rebuild and no deploy needed \u2014 the worker reads the
   entry off every offer.

If a brand-new **engine** is needed (e.g. a Whisper-based STT
backend), add the `WorkerModelEngine` enum value on both sides AND a
matching engine implementation in `src/engine/*.rs` AND register it
in the MultiEngine's routing table.

## Where this came from

Originally the worker shipped a `'*'` wildcard sentinel and a
hardcoded per-engine model registry: synthetic claimed everything,
sdcpp claimed only `z-image-turbo-q4_k_m.gguf` (hardcoded paths).
Two problems:

1. Synthetic happily fulfilled real-model jobs with placeholder
   bytes, which was destructive on the live queue (we lost ~150 jobs
   to that during a test run).
2. The worker had to be rebuilt + redeployed for every new model
   the operator wanted to queue.

The registry-on-the-studio design was settled in the same session;
the wildcard support shipped briefly (PR #425 in `minigames`) then
got reverted via PR #427.  See `LESSONS_LEARNED.md` for the
debugging timeline.
