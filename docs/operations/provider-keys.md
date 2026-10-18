# Provider keys

The worker holds no third-party provider key. It runs local models only; hosted providers are
called by the studio's Cloudflare Worker (`apps/studio` in the minigames repository), which owns
their keys as Worker secrets.

| Key                  | Provider        | Used for              | Lives in                                      |
| -------------------- | --------------- | --------------------- | --------------------------------------------- |
| `ELEVENLABS_API_KEY` | ElevenLabs      | sound effects (`sfx`) | the studio Worker's secrets (studio.minis.gg) |
| `APIFRAME_API_KEY`   | apiframe (Suno) | music                 | the studio Worker's secrets (studio.minis.gg) |

- Rotating a key means setting the new value as the studio Worker secret
  (`wrangler secret put <KEY>` in `apps/studio`); the worker needs no change and no release.
- The studio Worker reports to the Sentry project `studio-worker`, so a provider error such as
  `[SFX] Generation failed: ElevenLabs API error (401)` comes from the studio Worker, not from
  this worker.
- A local `wrangler dev` of the studio reads keys from `apps/studio/.dev.vars` (gitignored); a tool
  whose key is absent there is off.
- Prove a rotation with one generation through the studio CLI:
  `yarn studio sfx generate <game> --prompt "<sound>" --duration 1`, then
  `yarn studio sfx download <game> <jobId> 1 --out <file>`.
