# Model lifecycle

The worker is a multi-purpose model host. Besides running one-off jobs for the studio and the
local API, it keeps chosen models **loaded** in memory so local clients get warm, low-latency
answers, and it can **unload** them on request to give the memory back.

## Terms

| Term | Meaning |
| --- | --- |
| catalogue model | an entry in the local catalogue (`models.json`): id, kind, `ModelSource`, `vram_gb_estimate` |
| loaded model | a catalogue model whose weights are in memory, owned by the worker, ready to serve |
| resident | persisted wish that a model is loaded; restored when the daemon starts |
| transient job | a job whose model is loaded for that job only and released after it (studio offers, local API requests for a model that is not loaded) |
| lane | the serving path of one loaded model; requests to it queue on its lane |
| admission | the check that a load or transient job fits in free device memory |
| exclusive group | catalogue models of which at most one may be loaded at a time |

## States

Every catalogue model has exactly one state. Only the worker changes it.

```text
            load                ok
 unloaded ────────▶ loading ──────────▶ loaded
    ▲                  │                  │
    │                  │ error            │ unload
    │                  ▼                  ▼
    │               failed ◀──error── unloading
    │                  │                  │
    └───── load ───────┘                  │ done
    ▲                                     │
    └─────────────────────────────────────┘
```

| State | Meaning | Serves requests |
| --- | --- | --- |
| `unloaded` | not in memory | transient only |
| `loading` | weights being read and placed | no (callers wait or get `model_loading`) |
| `loaded` | resident on its lane | yes |
| `unloading` | draining in-flight requests, then freeing memory | no (new requests get `model_unloading`) |
| `failed` | the last load or unload failed; carries the reason | transient only |

### Transitions and guards

- `load` from `unloaded` or `failed`: admission first; on refusal the state is unchanged and the
  call is rejected with `insufficient_memory` (needed and free bytes attached). Otherwise the
  state becomes `loading`, then `loaded` or `failed`.
- `load` while `loading` or `loaded`: no-op, answers the current state.
- `load` of a model in an exclusive group first unloads the loaded member of that group.
- `unload` from `loaded`: `unloading`, drain in-flight requests (bounded wait), free, `unloaded`.
- `unload` while `unloaded`, `unloading` or `failed`: no-op, answers the current state (a `failed`
  model is moved to `unloaded`).
- `unload` while `loading`: the load completes first, then the unload runs.
- A load or unload never runs concurrently with another for the same model.

### Invariants

- A model is in exactly one state; `loaded` implies its weights are in memory.
- At most one member of an exclusive group is `loading` or `loaded`.
- Admission never lets the sum of loaded estimates plus a new load exceed free memory minus the
  safety margin.

## Residency

- `load` through the API sets `resident = true`; `unload` sets `resident = false`.
- Residency is stored per model in `<config dir>/residency.json`, separate from the catalogue, so
  a studio catalogue sync never changes it.
- When the daemon starts it loads every resident model, in catalogue order. A resident model that
  fails to load stays resident and shows `failed`; one that admission refuses stays resident and
  `unloaded`. Either is logged, and the next start tries again. A resident id missing from the
  catalogue is logged and skipped.
- Swapping within an exclusive group clears the outgoing model's residency; the incoming load
  waits for it to unload (bounded) and is admitted against the memory the swap frees.
- Deleting a catalogue model unloads it and drops its residency.

## Concurrency

- Transient jobs keep the existing one-at-a-time job gate: at most one transient job runs.
- Each loaded model serves on its own lane, alongside transient jobs and other lanes; admission,
  not the job gate, keeps them inside device memory.
- A lane serves one request at a time; a streaming session holds its lane until it closes.
- A transient job frees its model when it ends; nothing stays in memory that the host cannot
  see. Keeping a model warm is what residency is for.
- A transient job is admitted only if its model fits next to the loaded models; otherwise it is
  refused (`insufficient_memory`) and, for a studio offer, rejected so another worker takes it.

## Admission

- Needed: the catalogue model's `vram_gb_estimate`.
- Free: the device's free memory as probed now (`sys.rs`), minus a safety margin.
- A load that is refused changes nothing and says why; there is no silent fallback to CPU.

## Local API

All routes require the bearer token (see [local API](../local-api.md)).

| Method | Path | Answers |
| --- | --- | --- |
| `GET` | `/models` | the catalogue, each entry with `state`, `resident`, `since`, `loadable` and, when failed, `error` |
| `GET` | `/models/:id/state` | `{ id, state, resident, error?, since }` |
| `POST` | `/models/:id/load` | `202` loading / `200` already loaded / `409 insufficient_memory` / `404` unknown / `400` disabled |
| `POST` | `/models/:id/unload` | `202` unloading / `200` already unloaded / `404` unknown |

## Tray UI

The tray UI's Models page lists every catalogue model with its state, residency and since,
and offers Load / Unload per the guards above; Load only for `loadable` models (engines
with an in-process loader).  See [daemon and tray UI](daemon-and-tray.md).

## Observability

Every transition and every refusal logs one line under target `studio_worker::lifecycle` with
`op` (`load`, `unload`, `admit`, `restore`), `model`, `from`, `to`, and on failure `error`.
