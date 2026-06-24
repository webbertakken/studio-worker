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
- [x] tiny_http (pure-Rust, sync) local server module `local_api`
- [x] `POST /image` -> image bytes (sync); `GET/POST /models`, `DELETE
      /models/:id`; `GET /jobs`, `GET /healthz`
- [x] Integration tests (synthetic engine): image, model add/list, bad model
      -> 400, bad json -> 400, healthz, jobs (7 green, real reqwest)
- [x] Always-on (no config flag, per request): default port 4787, env override
      `STUDIO_WORKER_LOCAL_API_PORT`, ephemeral fallback; bind `127.0.0.1`

### Wire into the running worker
- [x] `spawn_local_api` started from `runtime::run` (before the registration
      gate, so it works without a studio) and from `ui::run`; catalog at
      `config dir/models.json`; graceful join on stop
- [x] Runs on its own thread; never blocks the studio session loop
- [x] E2E verified in the real binary: listening on 4787 pre-registration,
      /healthz 200, /models seeded with Z-Image, models.json written

### UI: local queue
- [x] Jobs tab shows a "Local queue" section (API url + local job cards) built
      from `observers.local_jobs` / `local_api_url`; egui-free JobsView test

### Docs + CI
- [ ] `docs/` page for the local API + catalog; README mention
- [ ] `cargo fmt`/`clippy --tests -D warnings`/`cargo test` green; coverage >=90
- [ ] Conventional-commit PR (title <= 52 chars)
