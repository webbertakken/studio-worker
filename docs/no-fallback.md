# No-fallback policy

> Reference for operators + future contributors.  This worker
> deliberately refuses to serve a non-synthetic model with the
> synthetic engine.

## What changed

Before: when a queued job named a real model (e.g. `flux1-dev`,
`z-image-turbo-q4_k_m.gguf`) and the worker had no real engine
compiled in to serve it, `MultiEngine::dispatch_with_source` would
fall through to the synthetic engine.  The synthetic engine produces
deterministic placeholder bytes (real WebP, but the wrong image).  The
studio happily accepted those bytes and the operator never saw an
error \u2014 silent destruction of a live job queue.

Now: `MultiEngine::dispatch_with_source` routes strictly by
`ModelSource.engine`:

- `sd-cpp` \u2192 only the `sdcpp` engine.
- `llama-cpp` \u2192 only the `llama` engine.
- `synthetic` \u2192 only the synthetic engine.

If the matching backend isn't compiled into this worker (the engine
wasn't enabled via cargo feature flag), the dispatch returns an
`anyhow::Error` with `no \`sdcpp\` engine compiled into this worker
(model {} requires it)`.  The session loop surfaces that as a
`Fail(retryable=false)` frame, the studio marks the job `failed` with
the diagnostic string, and the operator sees exactly which engine the
worker needs.

The legacy `JobClaim::resolved_task()` fallback (synthesising
`Task::Image` from a top-level `prompt + ext` pair when `task` was
absent) is **gone**.  `task: Task` and `model_source: ModelSource`
are required fields on the wire; deserialisation fails clearly when
either is missing.

## What synthetic still does

The synthetic engine is **not** a fallback any more.  It is an explicit
engine option, useful for:

- **Unattended CI**: the worker repo runs on free-tier GitHub Actions
  with no GPU.  The synthetic engine produces real, decodable
  WebP / WAV / JSON bytes deterministically from the prompt hash, so
  end-to-end WS-contract tests don't need a model.
- **Live verification**: the studio's `POST /models/:id/verify` flow
  enqueues a real generation job against the named model.  Operators
  can keep a `synthetic-image` model row in the studio registry to
  separately confirm "the pipeline works at all" before paying for
  real GPU time.
- **Bootstrap**: a worker starting up with no real models downloaded
  yet can still respond to claims for synthetic models, which the
  operator uses to verify auth + heartbeat + offer + complete
  end-to-end before the first multi-GB download.

The synthetic engine's `Engine::capabilities()` only advertises
`synthetic`, `synthetic-image`, `synthetic-llm`, `synthetic-stt`,
`synthetic-tts`, and `synthetic-video`.  It never advertises a real
model id, so the studio's claim filter cannot accidentally route a
real-model job to it.

## Operator diagnosis

If the studio shows a job as `failed` with
`no \`X\` engine compiled into this worker`, you need to:

1. Open a terminal where you build / install the worker.
2. Rebuild with the matching cargo feature:

       cargo install --path . --features sdcpp,llama,whisper,image-candle,tts,video

3. Restart the worker.

## Operator diagnosis (the other side)

If the studio shows a job as `failed` with `missing task + modelSource
on graphicsJobs row`, the row was queued by a code path that didn't
populate the registry resolver.  Open the Studio's Models tab, confirm
the model id used by the job is registered + enabled, then:

- For jobs whose model is missing: re-promote them via `/jobs/promote`.
  The promote route runs the resolver and bakes the result onto the
  row.
- For jobs whose model is disabled: re-enable the model row, or
  retarget the job to a different model.
