# Wake word training

Spec: [`docs/engines/wake-word.md`](../docs/engines/wake-word.md).

## Prototype (outside the worker, throwaway)

- [x] Read microWakeWord's notebook and code; piper-sample-generator's code; their latest revisions.
- [x] Download the datasets and generators; check every source decodes.
- [x] One environment holds torch and TensorFlow, both on the GPU.
- [x] espeak reads `hey runa` as /ɹʌnə/: spellings per language, phonemes logged.
- [x] Dutch MLS voices read slowly: prosody per language.
- [x] The five stages run end to end on a small request; TensorFlow Lite Micro loads the model.

## Worker

- [ ] `TaskKind::WakeWord` (local only) and its glyph.
- [ ] Torch provisioner: a recipe without a CUDA toolkit, the C++ compiler only for recipes that build.
- [ ] Shared stream runner: stop on request (`Ended::Stopped`).
- [ ] Downloader: a stall bound per read, tested against a server that stalls mid-body.
- [ ] `wake_word::request`: parse and validate the request; merge settings.
- [ ] `wake_word::recipe`: the environment, sources, datasets, generators, default settings.
- [ ] `wake_word::cutoff`: choose the cutoff; Poisson upper bound.
- [ ] `wake_word::manifest` and `wake_word::report`.
- [ ] `wake_word::stage`: run a stage, parse its events, classify its failure.
- [ ] `wake_word::training`: the job: gate, job run, stages, cancel, registry.
- [ ] Local API routes and their refusals.
- [ ] Python trainer embedded and installed from the binary.
- [ ] Docs: local API, index, AGENTS.md layout, daemon-and-tray kinds.
- [ ] fmt, clippy (default, headless, all), tests, coverage gate.

## Ship

- [ ] PR, CI green, merged; release; the installed worker updates.
- [ ] Train "Hey Runa" through the installed worker under a GPU slot.
- [ ] Report: paths, recall, false accepts per hour, wall time, device memory.
