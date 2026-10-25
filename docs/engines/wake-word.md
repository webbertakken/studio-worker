# Wake word training: microWakeWord models from a phrase

Implemented in [`src/wake_word/`](../../src/wake_word/), [`src/local_api/wake_word.rs`](../../src/local_api/wake_word.rs)
and [`python/wakeword/`](../../python/wakeword/).
A local API job trains a [microWakeWord](https://github.com/kahrendt/microWakeWord) model for a phrase: a streaming,
quantised TensorFlow Lite Micro model in the format ESPHome and Home Assistant load.
It follows upstream's training notebook: synthetic speech from
[piper-sample-generator](https://github.com/rhasspy/piper-sample-generator), augmented with background audio and room
impulse responses, against upstream's published negative feature sets.

The job is local only: the studio never offers it, and the worker never advertises it.

## Terms

| Term | Meaning |
| --- | --- |
| training | one wake word job: a phrase in, a model, its manifest and its report out |
| phrase | what the user says, e.g. `hey runa` |
| spelling | text a synthetic voice reads to say the phrase, per language (`hey roona` reads /ɹuːnə/) |
| near miss | a phrase that sounds like the phrase and must not wake it (`hey luna`) |
| generator | a multi-speaker piper model that voices one language (`en_US-libritts_r-medium.pt`) |
| prosody | how a language's voices speak: speeds, duration noise, speaker mixing |
| stage | one step of a training in its own child process: `prepare`, `samples`, `features`, `train`, `evaluate` |
| ambient set | hours of real audio without the phrase, streamed through the model to count false accepts |
| cutoff | the probability the model's smoothed output must pass to wake (`probability_cutoff`) |
| recipe | every pinned input of a training: environment, sources, datasets, generators, settings |

## Local API

All routes carry the local API's guards (loopback `Host` and `Origin`, bearer token).
Errors answer `{"error": <code>, "message": <text>}`.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| `POST` | `/wake-words` | a training request (below) | `202` the training (below) |
| `GET` | `/wake-words` | | every training of this daemon's lifetime, newest first |
| `GET` | `/wake-words/:id` | | the training |
| `DELETE` | `/wake-words/:id` | | `202` the training, stopping |
| `GET` | `/jobs/:id/log` | | the training's log, as for every job |

### Request

```jsonc
{
  "phrase": "hey runa",                    // required: 2 to 64 letters, spaces, apostrophes, hyphens
  "name": "hey_runa",                      // optional: [a-z0-9_]{1,48}; default the phrase's slug
  "displayName": "Hey Runa",               // optional: the manifest's wake_word; default the phrase in title case
  "languages": ["en", "nl"],               // optional: each one of en, nl, de, fr; default ["en"]
  "spellings": {                           // optional: per language, 1 to 8 texts; default [phrase]
    "en": ["hey roona"],
    "nl": ["hej roena", "hey roena"]
  },
  "negatives": ["hey luna", "runner"],     // optional: up to 32 near misses, voiced in every language
  "author": "Webber Takken",               // optional: the manifest's author; default "studio-worker"
  "website": "https://example.org",        // optional: the manifest's website; default the repository
  "targetFalseAcceptsPerHour": 0.5,        // optional: above 0, at most 10; default 0.5
  "settings": { "training": { "steps": [40000] } }  // optional: merged over the recipe's settings
}
```

- Every spelling's phonemes, as each language's voice reads them, are logged and kept in the report: a spelling that
  reads wrong (espeak reads `hey runa` as /ɹʌnə/, "runner") shows there.
- `settings` is merged over the default settings (JSON merge patch) and the result is checked as a whole:
  an unknown field, a wrong type or a value out of range is refused by name.

### The training

```jsonc
{
  "id": "local-1761400000000-1",           // also its job id: /jobs/:id/log
  "name": "hey_runa", "phrase": "hey runa",
  "state": "running",                      // running | completed | failed | cancelled
  "stage": "train",                        // provision | download | prepare | samples | features | train | evaluate | package
  "startedAt": "…", "finishedAt": null,
  "outputDir": "<models_root>/wake-word/models/hey_runa/<id>",
  "failure": null,                         // {reason, message} when failed (below)
  "result": null                           // when completed:
  // {"model": ".../hey_runa.tflite", "manifest": ".../hey_runa.json", "report": ".../report.json",
  //  "reportMarkdown": ".../report.md", "cutoff": 0.93, "recall": 0.91, "falseAcceptsPerHour": 0.11,
  //  "targetMet": true}
}
```

### Refusals

| Status | `error` | When |
| --- | --- | --- |
| 400 | `invalid_request` | the body is not a valid request; the message names the field |
| 404 | `unknown_training` | no training with that id |
| 409 | `busy` | another job holds the worker (one job at a time) |
| 409 | `not_running` | `DELETE` on a training that already ended |
| 503 | `unsupported_platform` | not Linux x86_64 with an NVIDIA GPU of compute capability 6.0 or more |
| 507 | `insufficient_disk` | the models filesystem has less free space than the training needs; carries `neededGib`, `freeGib` |

## A training

A training holds the worker's job gate from start to end, so no studio job, transient job or update restart runs
beside it.
It runs on its own thread; the request answers `202` at once.
It is a job like any other: listed while it runs, recorded in the local jobs ring when it ends, its log under
`/jobs/:id/log` and in the tray's Jobs page (kind `wake_word`, model `microwakeword`, the phrase as its prompt).

1. **provision**: the `wakeword` Python environment (below), built once by the torch engine's provisioner.
2. **download**: every dataset and generator the recipe names that is not on disk yet, each checked against its
   sha256.
3. **prepare**: the datasets turned into what training reads, once per download (`python -m wakeword prepare`).
4. **samples**: synthetic speech of each spelling and each near miss, in every language.
5. **features**: the samples augmented and turned into spectrogram features, split 80/10/10.
6. **train**: microWakeWord's own `model_train_eval`: training, conversion to a quantised streaming model, its test.
7. **evaluate**: held-out audio streamed through the quantised model; TensorFlow Lite Micro loads it.
8. **package**: the worker chooses the cutoff and writes the model, its manifest and the report.

Stages 3 to 7 each run `python -m wakeword <stage> <request.json>` in the environment, as a child process
(`crate::process::command`) that dies with the worker (`PR_SET_PDEATHSIG` and a parent watch).
Its stdout carries events, one JSON object per line; its stderr is log.

| Event | Fields |
| --- | --- |
| `stage` | `name` |
| `progress` | `stage`, `done`, `total` |
| `phonemes` | `language`, `text`, `role` (`positive`, `near_miss`), `phonemes` |
| `done` | `stage`, `seconds`, `deviceMemoryPeakMib`, `summary` |
| `failed` | `class` (`out_of_memory`, `disk_full`, `bad_input`, `internal`), `message` |

Exit codes: 0 done, 1 anything else, 2 out of memory, 3 bad input, 4 disk full.

Every line of every stage is written to `<run>/logs/<stage>.log`.
The job log gets the events, the stage's warnings and errors, and upstream's validation lines; the per-step lines of
training (`Mini-Batch #`) stay in the stage log.

`DELETE` stops the running stage's child process and ends the training `cancelled`; a download in flight finishes
its file first.
A training does not survive the daemon: stopping the daemon stops its child processes, and the training is gone.

### Failures

Every failure ends the training `failed` with a reason and a message that names what failed.

| `reason` | When |
| --- | --- |
| `disk_full` | a stage or a download ran out of disk space |
| `download_stalled` | a download received nothing for `DOWNLOAD_STALL` |
| `download_failed` | a download answered an error, or its sha256 differed |
| `out_of_memory` | a stage ran out of device memory (CUDA) or host memory |
| `provision_failed` | building the environment failed; the message carries the step and its last lines |
| `bad_input` | a stage refused the request (e.g. a spelling no voice can speak) |
| `stage_timed_out` | a stage passed its time limit and was stopped |
| `stage_failed` | a stage failed otherwise; the message carries its exit and last lines |
| `invalid_output` | the stages ended without the files the next one reads |

## Layout

| Path | What |
| --- | --- |
| `<models_root>/wake-word/downloads/<dataset>/` | downloads, kept: about 7.1 GB |
| `<models_root>/wake-word/datasets/` | prepared datasets, each with its `.ready` marker: about 27 GB |
| `<models_root>/wake-word/trainer/` | the trainer's Python files, written from the binary |
| `<models_root>/wake-word/runs/<id>/` | one training's work: request, samples, features, checkpoints, stage logs |
| `<models_root>/wake-word/models/<name>/<id>/` | the result: `<name>.tflite`, `<name>.json`, `report.json`, `report.md` |
| `<models_root>/torch/envs/wakeword/` | the environment, beside the torch engine's |
| `<models_root>/torch/src/<source>-<rev>/` | microWakeWord and piper-sample-generator at pinned revisions |

A completed training removes its samples and features (the bulk of its run directory); a failed one keeps them.

### Disk

Before it starts, a training checks the models filesystem has room for what it still needs: the downloads not on disk,
the datasets not yet prepared, and its own run, estimated from its sample counts (`SPECTROGRAM_BYTES` per spectrogram).
At the default settings a first training needs about 70 GB, a later one about 30 GB.

## Recipe

Every input is pinned; the report carries all of them, so a training can be repeated.
Pins were the latest on 2026-10-25 unless the table says why not.

### Environment `wakeword`

| | Pin | Why |
| --- | --- | --- |
| Python | 3.12.15 | TensorFlow 2.21 supports 3.10 to 3.13 |
| torch | 2.14.1 + cu126 | piper-sample-generator's generators; cu126 runs on drivers from 560 |
| tensorflow[and-cuda] | 2.21.0 | microWakeWord's training; its CUDA libraries come as wheels |
| tensorboard | 2.21.0 | upstream's training logs through `tf.summary` |
| datasets | 3.6.0 | 4 and later decode audio through torchcodec; 3.6.0 decodes with soundfile |
| audiomentations | 0.43.1 | microWakeWord needs `AddColorNoise` (0.35 and later) |
| piper-tts | 1.3.0 | piper-sample-generator's pin |
| webrtcvad-wheels | 2.0.14.post1 | microWakeWord's pin; wheels, no compiler |
| tflite-micro | 0.dev20261023182759 | the evaluate stage loads the model in TensorFlow Lite Micro (nightly only) |
| others | `RECIPE.packages` | audio-metadata, mmap-ninja, pymicro-features, pyyaml, ai-edge-litert, pyarrow, soundfile, scipy |

piper-sample-generator and microWakeWord run from their checkouts on `PYTHONPATH`: microWakeWord's package misses
its subpackages, and the generator's models need `piper_train`, which its package leaves out.
TensorFlow finds its CUDA wheels only with their `lib` directories on `LD_LIBRARY_PATH`.

### Sources

| Source | Revision |
| --- | --- |
| [kahrendt/microWakeWord](https://github.com/kahrendt/microWakeWord) | `4665173cd35f1cff9a61e06fc427f124766c488e` (2026-07-06) |
| [rhasspy/piper-sample-generator](https://github.com/rhasspy/piper-sample-generator) | `2971426a55072f7d22fec416ca7800df8bd23207` (3.2.0, 2026-03-12) |

### Datasets

| Dataset | Use | Licence |
| --- | --- | --- |
| [kahrendt/microwakeword](https://huggingface.co/datasets/kahrendt/microwakeword) `speech`, `dinner_party`, `no_speech` | negative features for training | CC BY-NC 4.0 |
| same, `dinner_party_eval` | CHiME-6 dev and eval (validation ambient, 9.7 h), DiPCo (testing ambient, 5.3 h) | CC BY-NC 4.0 |
| [agkphysics/AudioSet](https://huggingface.co/datasets/agkphysics/AudioSet) `bal_train/09.parquet` | background noise for augmentation | CC BY 4.0 |
| [mchl914/fma_xsmall](https://huggingface.co/datasets/mchl914/fma_xsmall) | background music for augmentation | MIT (the dataset); tracks CC |
| [MIT environmental impulse responses](https://huggingface.co/datasets/davidscripka/MIT_environmental_impulse_responses) | room reverberation | unknown |
| [Multilingual LibriSpeech](https://huggingface.co/datasets/facebook/multilingual_librispeech) Dutch `9_hours` | Dutch speech negatives for training | CC BY 4.0 |
| same, Dutch `dev` and `test` | Dutch ambient sets, 12.8 h each (validation, testing) | CC BY 4.0 |

Revisions, URLs, sizes and sha256s are in `recipe::DATASETS`.
The `*_background` feature sets of the same repository are 20 ms features and do not fit the 10 ms recipe.
Upstream's licences make a model trained this way fit for personal, non-commercial use.

### Generators and prosody

| Language | Generator | Speakers used | Speeds | Duration noise |
| --- | --- | --- | --- | --- |
| `en` | `en_US-libritts_r-medium.pt` | 800 of 904 (the last have artefacts) | 0.75, 1.0, 1.25 | 0.8 |
| `nl` | `nl_NL-mls-medium.pt` | 52 | 0.6, 0.7, 0.85 | 0.4 |
| `de` | `de_DE-mls-medium.pt` | 236 | 0.35, 0.45, 0.55 | 0.4 |
| `fr` | `fr_FR-mls-medium.pt` | 125 | 0.45, 0.55, 0.7 | 0.4 |

The MLS voices read slowly: with upstream's speeds and duration noise, `hej roena` lasts a median 1.34 s voiced (up to
3 s) against 0.54 s for English `hey roona`, and German `hey runa` 3.1 s.
At the speeds above, each lasts a median 0.65 to 1.1 s (measured, 2026-10-25).
Each batch mixes a random pair of speakers (slerp 0.5) at a random speed from the list.

### Settings

The defaults are `recipe::default_settings()`; the request's `settings` overrides any of them.

| Setting | Default | Note |
| --- | --- | --- |
| `samples.positivePerLanguage` | 10 000 | per language, the spellings alternating |
| `samples.nearMissPerPhrasePerLanguage` | 1 000 | |
| `features.positiveRepetitions` | 2 | upstream's |
| `training.steps` | 20 000, 10 000 | two phases |
| `training.learningRates` | 0.001, 0.0001 | |
| `training.negativeClassWeight` | 20, 20 | upstream's |
| `training.featureWeights` | positive 2, near miss 2, speech 10, dinner party 10, no speech 5, Dutch speech 3 | sampling weights |
| `training.featureWeights` (evaluation only) | dinner party eval 0, Dutch eval 0 | never sampled; their validation audio picks the checkpoint |
| `deviceMemoryBudgetGib` | 10 | train peaks at 10.6 GiB with the context (measured): upstream validates on 22 h of ambient windows in one tensor |
| `training.model` | upstream's notebook mixednet | |
| `evaluation.slidingWindow` | 5 | the manifest's `sliding_window_size` |
| `seed` | 1 | every random choice follows it |

## Choosing the cutoff

The evaluate stage measures on a 0.01 grid of cutoffs; the worker chooses.

- **Validation** (chooses): the ambient sets CHiME-6 dev and eval and MLS Dutch dev; the validation splits of the
  positives and near misses.
  Upstream's training keeps the checkpoint with the best average viable recall on the same ambient audio, so the
  checkpoint and the cutoff are chosen on one set; with CHiME-6 alone it kept a checkpoint blind to Dutch speech, where
  11 of 13 testing false accepts fell (2026-10-25).
- **Testing** (reports): the ambient sets DiPCo and MLS Dutch test, never seen in training or in the choice; the
  testing splits of the positives and near misses.
- The cutoff is the lowest on the grid at which validation's false accepts per hour, and those of every higher cutoff,
  are at most the target. Recall only falls as the cutoff rises, so the lowest such cutoff keeps the most.
- When no cutoff meets the target, the cutoff with the fewest validation false accepts per hour (the lowest of those)
  is chosen, the training completes with `targetMet: false`, and the log says so (warn).
- A detection is counted as upstream's test counts it: the model's output averaged over `slidingWindow` slices, above
  the cutoff, at least `COOLDOWN_SLICES` slices after the previous one.
- False accepts per hour on testing also carries a 95% upper bound (Poisson): a few hours of audio pin a rate below
  0.5 only loosely.
- Recall on positives is measured on synthetic speech of the same generators: it is optimistic against real voices.

## Output

### Manifest (`<name>.json`)

ESPHome's v2 model manifest:

```json
{
  "type": "micro",
  "wake_word": "Hey Runa",
  "author": "Webber Takken",
  "website": "https://github.com/webbertakken/studio-worker",
  "model": "hey_runa.tflite",
  "trained_languages": ["en", "nl"],
  "version": 2,
  "micro": {
    "probability_cutoff": 0.93,
    "feature_step_size": 10,
    "sliding_window_size": 5,
    "tensor_arena_size": 32768,
    "minimum_esphome_version": "2024.7.0"
  }
}
```

`tensor_arena_size` is the smallest arena TensorFlow Lite Micro runs the model in, plus `ARENA_MARGIN`, rounded up to
1 KiB: the published `hey_jarvis` model runs in 20 320 bytes here and its manifest says 22 860.

### Report (`report.json`, `report.md`)

- the result: cutoff and why, recall (overall and per language), false accepts per hour (pooled and per ambient set,
  with hours, counts and the upper bound), each near miss's accept rate, on validation and testing;
- a table of cutoffs around the chosen one;
- the phonemes of every spelling and near miss per language;
- the recipe: environment pins and the environment's full `pip freeze`, sources, datasets (URL, revision, sha256),
  generators, effective settings, seed;
- the model: size, sha256, tensor arena;
- wall time per stage, device memory peak per stage, the GPU.

## Observability

All under target `studio_worker::wake_word`.

| `op` | When |
| --- | --- |
| `wake_word` | a training accepted, refused (and why), finished (outcome, seconds), cancelled |
| `stage` | a stage started, finished (seconds, device memory) or failed (reason) |
| `stage_event` | a stage's progress (at most every `PROGRESS_LOG_INTERVAL`), phonemes |
| `stage_line` | a stage's stderr line: warnings and errors at warn, upstream's validation lines at info, the rest debug |
| `download` | a download started, skipped (on disk), finished or failed (the shared downloader's own breadcrumbs too) |
| `cutoff` | the cutoff chosen, the rule's numbers, whether the target was met |

## Constants

| Constant | Value | Provenance | Safe range |
| --- | --- | --- | --- |
| `DEFAULT_TARGET_FALSE_ACCEPTS_PER_HOUR` | 0.5 | Webber, 2026-10-25 | above 0, at most 10 |
| `DOWNLOAD_STALL` | 120 s | a CDN that sends nothing for 2 minutes has stalled | 30 to 600 s |
| `ARENA_MARGIN` | 25 % | `hey_jarvis`: 20 320 measured, 22 860 published (+12.5 %), doubled | 10 to 100 % |
| `MIN_COMPUTE_CAPABILITY` | 6.0 | TensorFlow 2.21's GPU wheels | 6.0 and up |
| `COOLDOWN_SLICES` | 25 | upstream's test | 10 to 50 |
| `SPECTROGRAM_BYTES` | 50 KB | 311 frames of 40 float32 features | measured |
| `STAGE_TIMEOUTS` | prepare 2 h, samples 1 h, features 3 h, train 8 h, evaluate 1 h | a hang, not a slow run | |
| `PROGRESS_LOG_INTERVAL` | 30 s | | 5 to 300 s |
| `MINIMUM_ESPHOME_VERSION` | 2024.7.0 | ESPHome's v2 manifests | |
