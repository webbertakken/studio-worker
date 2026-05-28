# No-fallback policy

> Companion to the studio-side
> [`apps/studio/docs/model-registry.md`](https://github.com/webbertakken/minigames/blob/main/apps/studio/docs/model-registry.md).

This worker used to silently route any job to its `SyntheticEngine`
when no real engine claimed the requested model.  That fallback was
destructive on a live queue: real-model jobs returned placeholder
bytes that looked indistinguishable from real renders.  As of the
"per-game image config" PR the fallback is gone.

## What changed

1. **`MultiEngine::dispatch_with_source` routes strictly by
   `ModelSource.engine`.**  The TS studio resolves the model row from
   D1 (`studioModels.engine`) and attaches it to every offer.  The
   worker picks the engine whose `name()` matches the chosen
   `engine` slug and bails out otherwise:

   ```
   no `sdcpp` engine compiled into this worker (model z-image-turbo-q4_k_m.gguf requires it)
   ```

2. **`task` + `model_source` are required on every offer.**  Top-level
   `prompt` + `ext` on the claim are gone; the worker reads kind-aware
   payloads through the `Task` enum and refuses (with a
   `protocol_violation`) any claim that lacks either field.

3. **`JobClaim::resolved_task` is deleted.**  There is no legacy
   `prompt`-to-`Task::Image` synthesis path on the worker side.

## What stays

The `SyntheticEngine` is still compiled into the worker by default.
It serves models whose `ModelSource.engine == Synthetic` and only
those.  Use cases:

- **CI**.  The GitHub Actions matrix has no GPU; the full WS contract
  + multi-modal round-trip suite use synthetic to exercise the wire
  contract on free-tier runners.
- **Bootstrap**.  A fresh worker install responds immediately, before
  the 5 GB of real-model weights finishes downloading.
- **Pipeline-only verification**.  Operators with a small GPU can
  confirm the offer / claim / complete loop works against
  `synthetic-image` before troubleshooting their real-model VRAM
  budget.

## Diagnosing a refused offer

When the orchestrator sends a `Fail` because no engine accepted, the
WS log line looks like:

```
worker fail jobId=… reason="no `sdcpp` engine compiled into this worker"
```

Action items in order of likelihood:

1. The worker binary was built without the `sd-cpp` feature flag.
   Rebuild with `cargo build --release --features image-sdcpp` (or
   the engine's actual feature gate).
2. The studio's `studioModels.<id>.engine` value doesn't match any
   engine compiled into this worker (`sd-cpp`, `llama-cpp`,
   `synthetic`).  Open the Models page and confirm.
3. The studio's offer carries `engine: 'synthetic'` but the worker
   has explicitly removed the synthetic engine via a custom build.
   Re-add it or stop offering synthetic models.

## Why this matters

Silent fallback to synthetic produced two failure modes that were
particularly hard to diagnose from operator logs:

- **Asset poisoning** — placeholder WebP bytes landed in R2 under a
  real asset name + version, marked `done`, and were served to game
  clients indistinguishably from real renders.
- **Tail-eating** — a worker that had advertised support for a real
  model but couldn't actually serve it would keep claiming jobs,
  returning synthetic bytes, and starving the queue of any real
  worker that could take them.

Both are now compile-time impossible: the dispatch trait takes a
non-`Option<&ModelSource>`, and `MultiEngine` never falls back.
