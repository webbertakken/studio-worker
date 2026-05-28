# Job-state recovery playbook

How to reset stuck / mis-completed / failed jobs in the studio's
production D1 so the worker re-processes them.  Cross-repo \u2014 the
SQL lives in the studio's D1, the worker just picks the resets up
on the next Offer.

All commands assume:

- `cd ~/Repositories/minigames/apps/studio` (or any worktree pointing
  at the same wrangler config)
- `yarn dlx wrangler` works (the studio's wrangler config knows about
  STUDIO_DB)
- Your wrangler is logged in (`yarn dlx wrangler whoami`)

## Inspect first

Count by `(status, model)` to spot zombies and bad batches:

```bash
yarn dlx wrangler d1 execute STUDIO_DB --env production --remote --json \
  --command "SELECT status, model, COUNT(*) as n FROM graphicsJobs GROUP BY status, model" \
  | python3 -c "import json,sys,re; m=re.search(r'\[\s*\{[\s\S]*\]', sys.stdin.read()); print(json.dumps(json.loads(m.group(0))[0]['results'], indent=2))"
```

For one specific worker (e.g. our dev rig):

```bash
WORKER_ID='b1adff14-a81c-4404-a65b-b56060fb2e32'
yarn dlx wrangler d1 execute STUDIO_DB --env production --remote --json \
  --command "SELECT status, model, COUNT(*) as n FROM graphicsJobs WHERE lastWorkerId='$WORKER_ID' GROUP BY status, model" \
  | python3 ...
```

## Common conditions + fixes

### Zombie `claimed` rows (worker died mid-dispatch)

`claimedAt` is more than a few minutes old, `status='claimed'`,
worker is no longer in flight.  The DO's heartbeat-sweep should
catch these eventually but only if the WS session is still alive
(stale-heartbeat timeout kicks the session, releases the claim).
After a worker restart the in-memory state is rebuilt from
hibernation attachments \u2014 a row claimed by a session that's gone
stays claimed.

Reset everything claimed by a specific worker that's been
abandoned for >5 minutes:

```bash
yarn dlx wrangler d1 execute STUDIO_DB --env production --remote \
  --command "UPDATE graphicsJobs
             SET status='queued', claimedBy=NULL, claimedAt=NULL, startedAt=NULL,
                 updatedAt=strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE status='claimed'
               AND lastWorkerId='$WORKER_ID'
               AND claimedAt < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-5 minutes')"
```

### Synthetic-bytes upload on a real-model job

This used to happen when the worker's synthetic engine advertised
the `'*'` wildcard.  Symptom: `status='done'`, `model='flux1-dev'`
(or any real model), bytes in R2 are deterministic placeholder
images.  See [LESSONS_LEARNED](../../LESSONS_LEARNED.md) for the
post-mortem.

Reset everything that worker `b1adff14-...` marked `done` for a
real (non-`synthetic*`) model, switching the model to whatever the
operator wants this time (typically `z-image-turbo-q4_k_m.gguf`
since that's what the registry knows about right now):

```bash
cat > /tmp/reset-bad-done.sql <<'EOF'
UPDATE graphicsJobs
SET
  status = 'queued',
  model = 'z-image-turbo-q4_k_m.gguf',
  modelSource = '{"engine":"sd-cpp","files":[{"role":"diffusion-model","url":"https://huggingface.co/leejet/Z-Image-Turbo-GGUF/resolve/main/z_image_turbo-Q4_K.gguf","filename":"z_image_turbo-Q4_K.gguf"},{"role":"text-encoder","url":"https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf","filename":"Qwen3-4B-Instruct-2507-Q4_K_M.gguf"},{"role":"vae","url":"https://huggingface.co/Comfy-Org/Lumina_Image_2.0_Repackaged/resolve/main/split_files/vae/ae.safetensors","filename":"ae.safetensors"}],"cliDefaults":{"cfgScale":1.0,"steps":8,"width":1024,"height":1024,"samplingMethod":"euler"}}',
  claimedBy = NULL, claimedAt = NULL, startedAt = NULL, completedAt = NULL,
  generatedAt = NULL, lastError = NULL, attempts = 0,
  updatedAt = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE lastWorkerId = '$WORKER_ID'
  AND status = 'done'
  AND model NOT LIKE 'synthetic%';
EOF
yarn dlx wrangler d1 execute STUDIO_DB --env production --remote \
  --file=/tmp/reset-bad-done.sql
```

Keep the modelSource JSON in sync with
`apps/studio/src/worker/modules/graphics/modelRegistry.ts` \u2014 a
mismatch causes "downloaded a file that doesn't match what sd-cli
expects" errors.

### `status='failed'` because the worker rejected the model

Pre-modelSource workers would fail every `flux1-dev-i2i` job with
"sdcpp engine cannot serve model flux1-dev-i2i" because the engine
hardcoded a model whitelist.  Reset them the same way as
synthetic-bad-done above; the WHERE clause changes to
`status='failed' AND lastError LIKE '%cannot serve model%'`.

### Recompose prompts (truncated by the worker bug)

The worker had a `truncate_prompt(200)` bug that overwrote the row's
`prompt` column during multipart `/complete`.  The actual `sd-cli`
invocation got the full prompt (so the images are correct), but the
DB display string is `<first 200 chars>…`.  Fix: re-run
`promptComposer(job)` for each row and `UPDATE` the prompt column.

There's no D1-side helper for this yet; the recompose script lives
in `apps/studio/scripts/recompose-prompts.ts` (TODO once we ship
that).

## Mass operation safety

- D1 batch UPDATEs are atomic per transaction.  The wrangler
  `--file=...` mode runs the SQL as one batch.
- Be careful with the `WHERE` clause when targeting `status='done'`
  rows \u2014 a typo can re-queue thousands of correctly-generated
  assets.  Always count first.
- The `notifyJobCreated` DO RPC is **not** triggered by a raw D1
  UPDATE \u2014 the studio's WorkerConnections DO won't immediately
  notice the reset rows.  But: a connected worker's next
  `notifyJobCompleted` (after finishing its current job) calls
  `offerNextFor`, which queries D1 fresh \u2014 it'll pick up the
  reset rows then.  In practice this means resets land within
  seconds of the next completion.

## Hibernation / DO state caveats

The WorkerConnections DO keeps an in-memory session map AND
persists each session's attachment onto the WebSocket itself.
After hibernation, `getStore()` rebuilds the map from
`state.getWebSockets()`.  A D1 UPDATE doesn't touch any of this \u2014
session.currentJob is in-memory only, and currentJobId on the
studioWorkers row is updated by heartbeats.  If you reset a job
that a session thinks it's still working on, the next
`notifyJobCompleted` (when the worker uploads the real result for
the OTHER job it's actually serving) reconciles.

## See also

- [`docs/runtime/model-source.md`](../runtime/model-source.md) \u2014 the JSON shape you're writing into the modelSource column
- [`LESSONS_LEARNED.md`](../../LESSONS_LEARNED.md) \u2014 why we're recovering from synthetic-bad runs in the first place
