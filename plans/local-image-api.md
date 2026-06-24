# Local image API + local queue + local model catalog

Let the worker generate images locally (e.g. Z-Image) without the minis
studio. Decisions from the requester:

- **Always on**: no separate mode/subcommand. Whenever the worker runs
  (`run`, `ui`), a local API is available.
- **Local queue in the app**: locally-submitted jobs appear in the UI alongside
  studio jobs.
- **Local model catalog** the user can edit and add to, mirroring how the studio
  registry works (same `ModelSource` shape). Seeded with Z-Image.
- **Synchronous + localhost-only** (`127.0.0.1`, no auth): the call blocks and
  returns the image bytes.

Architecture notes (verified):
- Engine entry point: `Engine::dispatch_with_source(model, Task, &ModelSource)`.
- Studio job flow to mirror: `ws::session::run_offered_job` -> dispatch ->
  `runtime::record_recent_job(observers, RecentJob)` (the UI's `recent_jobs`).
- Config dir via `ProjectDirs("gg","minis","minis-studio-worker")`.
- Z-Image `ModelSource` canonical seed: studio migration `0017_seed_registry.sql`.

## Tasks

### Local model catalog (GPU-free, TDD)
- [x] `catalog`: `CatalogModel { id, displayName, kind, vramGbEstimate,
      description, source: ModelSource, enabled }` (serde camelCase JSON)
- [x] Built-in seed containing the exact Z-Image `ModelSource`
- [x] `Catalog::load_or_seed(path)`, `save`, `get`, `list`, `upsert`,
      `remove`, `default_image_model`
- [ ] Catalog file path in the config dir (`models.json`) — wired in next step
- [x] Unit tests (round-trip, seed, CRUD, load-missing-seeds) — 7 green

### Local job execution (mirror the studio path)
- [x] `local::run_image(engine, catalog, observers, LocalImageRequest) ->
      Result<TaskResult>`: resolve model, build `Task::Image` + `ModelSource`,
      dispatch, record into the local-queue ring
- [x] Dedicated `WorkerObservers::local_jobs` ring + `record_local_job`
      (separate "local queue", no churn to the studio path)
- [x] Tests against the synthetic engine (no GPU): submit -> bytes -> recorded;
      unknown/non-image/no-default error paths (5 green)

### Local HTTP API (inbound, 127.0.0.1, always on)
- [ ] Pick a light server (tiny_http, pure-Rust, sync — matches blocking engine)
- [ ] `POST /image` { prompt, model?, width?, height?, steps?, seed?,
      negativePrompt?, ext? } -> image bytes (sync)
- [ ] `GET /models`, `POST /models` (add a model, like studio), `DELETE
      /models/:id`
- [ ] `GET /jobs` (recent local jobs), `GET /healthz`
- [ ] Config: `[local_api] enabled=true, port` (default), bind `127.0.0.1`
- [ ] Integration tests (synthetic engine): image happy-path, model add/list,
      bad model -> 400, bad json -> 400

### Wire into the running worker
- [ ] Start the server thread from `runtime::run` and `ui::run`, sharing the
      engine + observers + catalog; graceful shutdown
- [ ] Ensure it never blocks the studio session loop

### UI: local queue
- [ ] Jobs view shows a `local` badge / source column; verify the local jobs
      ring renders (egui test like the existing jobs-tab tests)

### Docs + CI
- [ ] `docs/` page for the local API + catalog; README mention
- [ ] `cargo fmt`/`clippy --tests -D warnings`/`cargo test` green; coverage >=90
- [ ] Conventional-commit PR (title <= 52 chars)
