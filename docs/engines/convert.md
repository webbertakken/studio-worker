# convert engine: model conversion to ONNX with an accuracy gate

Implemented in [`src/engine/convert/`](../../src/engine/convert/) and
[`python/convert/`](../../python/convert/). The `convert` engine (`ModelEngine::Convert`, wire
`convert`) serves the `model_convert` kind: a published classifier converted to ONNX with tf2onnx
and scored, beside its source model, on a held-out **gate set**, on the CPU, in a child process per
job. The contract is the studio's spec, `docs/specs/015-studio/model-conversion.md` (minis.gg).
Its first user is Doodle Games' guesser, `zarqankhn/quickdraw-345-tflite`.

## Terms

| Term | Meaning |
| --- | --- |
| source | the model to convert: `hf:<owner>/<repo>@<40-hex commit>` and a `file` in it |
| gate set | `drawingsPerWord` raw Quick, Draw! drawings of every label, drawn by `seed` |
| recipe | how a raw drawing becomes the 28 x 28 input: `quickdraw-issue-19` |
| polarity | `inkOnDark` (the raster as is) or `darkOnLight` (`1 - raster`) |
| reference | one gate drawing with its raw strokes and the recipe's raster, for golden tests |
| report | the conversion report, sent as `generationMeta` beside the `.onnx` |

## The task

```jsonc
{
  "kind": "model_convert",
  "source": "hf:zarqankhn/quickdraw-345-tflite@f978d83cfc41fa3c76435a4c96417e055220c735",
  "file": "quickdraw_model_int8.tflite",
  "labels": "labels.txt",                 // default labels.txt
  "converter": "tf2onnx",
  "opset": 13,                            // default 13; 13..=18
  "gateSet": { "dataset": "quickdraw-raw", "drawingsPerWord": 100, "seed": 3577 },
  "recipe": "quickdraw-issue-19",
  "polarities": ["inkOnDark", "darkOnLight"],  // default both
  "referenceCount": 20                    // default 20; 0..=100
}
```

The offer's `modelSource` is `{ "engine": "convert", "files": [] }` (registry model `tf2onnx`,
`vramGbEstimate: 0`). The engine refuses, permanently (`ConvertError::BadTask`, never retried), a
source that is not pinned to a commit, a file outside the repository or not `.tflite`, another
converter, recipe or dataset, an opset outside 13..=18 (tf2onnx 1.17 writes up to 18; an int8 model
needs per-axis `QuantizeLinear`), `drawingsPerWord` outside 1..=1000, `referenceCount` above 100, a
seed above `u32::MAX`, and a polarity that is unknown or asked twice.

## The result

- `TaskResult::ModelConvert { bytes, ext: "onnx", report }`. The session uploads it through the
  multipart `/complete` route: `image` is the `.onnx`, `ext=onnx`, `generationMeta` the report JSON.
- Before upload the engine checks the report against the file (`kind`, `sha256`, `bytes`, every
  part the spec names) and its size (at most 1 MiB, the studio's `maxReportBytes`). A mismatch fails
  the job (`InvalidOutput`, retried).
- The report is the spec's: `sha256`, `bytes`, `source`, `file`, `converter {name, version,
  opset}`, `runtime {name, version}`, `input` and `output {name, shape, type}`, `labels`,
  `labelsMatchCategories`, `gate {dataset, seed, drawingsPerWord, drawings, recognised, recipe,
  accuracy}`, `references` and `durationsMs {fetch, convert, gate}`. Each polarity in `accuracy`
  has `onnx`, `source` and `onnxRecognised`, each `{top1, top5}` to 4 decimals.
- `durationsMs.fetch` covers the source model, its labels and the dataset's categories;
  `convert` the conversion; `gate` the sampling, rasterising and scoring.
- A job that cannot fetch, convert or score fails with the step and its error; it never completes
  with a partial report.

## Platform

- Registers on every build; advertises `model_convert` (model `convert:*`, and the kind in the
  heartbeat's `taskKinds`) only on Linux x86_64, the target of the `uv` the provisioner downloads.
  It logs `op="platform"` either way.
- CPU only: the converter runs with `CUDA_VISIBLE_DEVICES=` and scores on half the machine's
  threads, at most `SCORE_THREADS_MAX`.
- The recipe draws with the system's cairo: `libcairo.so.2` must be installed (`libcairo2` on
  Debian and Ubuntu; every desktop has it). Without it the environment's import check fails and
  names `cairocffi`.

## Environment

Built once by the torch engine's provisioner ([torch engine](torch.md#provisioning)) for the CPU
(`capability: None`: no torch step, no CUDA toolkit, nothing compiled), under
`<models_root>/torch/envs/convert`, with Python 3.12.15:

| Package | Pin | Why |
| --- | --- | --- |
| tensorflow-cpu | 2.21.0 | tf2onnx reads TFLite through TensorFlow |
| tf2onnx | 1.17.0 | the converter |
| onnx | 1.23.2 | wrapping and checking the graph |
| onnxruntime | 1.31.1 | scoring the converted model |
| ai-edge-litert | 2.3.0 | scoring the source model (`tf.lite.Interpreter` is deprecated) |
| cairocffi | 1.7.1 | the recipe's cairo; ABI mode, so no compiler or headers (pycairo has no Linux wheels) |
| numpy, protobuf, huggingface-hub, requests | 2.5.4, 7.36.2, 2.4.0, 2.34.2 | |

Measured on the dev box: the environment is about 1.6 GB and builds in about a minute.

| Path | What |
| --- | --- |
| `<models_root>/torch/envs/convert/` | the environment |
| `<models_root>/convert/program/` | the embedded converter (`python/convert/`), rewritten when it differs |
| `<models_root>/convert/gate-sets/<dataset>/<seed>/<label>.ndjson` | drawn gate sets, reused by every job with the same seed |
| `<models_root>/hf-cache/` | `HF_HOME`: the source repositories |

## A job

1. **check** the task (above).
2. **provision** the environment when its marker is missing or stale.
3. **run** `python -m convert <request.json>` in a temporary directory, until it exits or
   `RUN_TIMEOUT` (45 minutes) passes. Its steps:
   1. `fetch`: `file` and `labels` from Hugging Face at the pinned revision (`HF_HOME`), and the
      dataset's `categories.txt` at a pinned commit of `googlecreativelab/quickdraw-dataset`.
   2. `convert`: `tf2onnx.convert.from_tflite` at the opset, then the I/O wrap (below).
   3. `verify`: input `[1, 28, 28, 1]` float32, output `[1, <labels>]` float32.
   4. `labels`: `labelsMatchCategories` is `labels == categories sorted by UTF-8 bytes`.
   5. `gate`: sample, rasterise, score.
   6. `references`, then the report.
4. **read** the `.onnx` and the report back and check them.

### Conversion

The int8 file takes `uint8` (scale 1/255, zero point 0) and gives `uint8` (scale 1/256), although
its model card says float32. tf2onnx keeps that: int8 weights in a QDQ graph, uint8 at the edges.
The engine puts a `QuantizeLinear` in front and a `DequantizeLinear` behind, with the TFLite file's
own scales and zero points, and fixes the batch at 1. The ONNX model then takes and gives float32,
as the spec's interface says; the int8 graph inside is not touched and nothing is re-quantised.
TFLite does the same arithmetic when a caller quantises its input, which is how the source model is
scored.

tf2onnx numbers its nodes from a process-wide counter and orders some of them by set iteration, so
two conversions of the same file give different bytes (same graph, same scores): the sha256 names
one conversion, not the source.

### Sampling

- For each label, `SplitMix64.keyed(seed, label)`: the state starts at
  `seed XOR fnv1a64(utf-8 label)` (mod 2^64); each draw adds `0x9E3779B97F4A7C15` and mixes
  (`z ^= z >> 30; z *= 0xBF58476D1CE4E5B9; z ^= z >> 27; z *= 0x94D049BB133111EB; z ^= z >> 31`).
  A uniform integer below `n` is `(draw * n) >> 64`.
- A draw's byte offset is uniform below the file's size (a `HEAD`); the drawing is the first whole
  line starting at or after it, read by HTTP range (16 KiB, then more while the line goes on). Past
  the last line it wraps to the first.
- A drawing already drawn for its label is drawn again, so a label's drawings are distinct; a
  smaller gate set is the start of a larger one.
- Labels are URL-encoded (`The%20Eiffel%20Tower.ndjson`).
- Reads run `FETCH_CONCURRENCY` (64) at a time on keep-alive connections: a range read deep in a
  large file takes 0.2 to 0.8 s. Network errors, `429` and `5xx` are retried 5 times with
  exponential backoff from 0.5 s; any other status fails the step.
- References: `referenceCount` labels by a partial Fisher-Yates shuffle driven by
  `SplitMix64.keyed(seed, "references")`, each with its label's first gate drawing.

### Recipe `quickdraw-issue-19`

1. The dataset README's simplification: align the drawing to 0, scale uniformly so its longer side
   is 255, resample every stroke at 1 px along its length, Ramer-Douglas-Peucker at epsilon 2.0,
   round half up.
2. `vector_to_raster` from the dataset's GitHub issue #19: cairo, `ANTIALIAS_BEST`, round caps and
   joins, line width 16 and padding 16 in the 256 space, centred on the bounding box (`(256 - max) /
   2`), scaled by `28 / (256 + 2 * 16 + 16)` and drawn straight onto the 28 x 28 surface. Ink 1.0 on
   0.0, `/ 255`.

Checked against the dataset's own `simplified` files (same `key_id`, the first lines of `cat`,
`The Eiffel Tower`, `zigzag` and `hot air balloon`): 421 of 466 drawings simplify identically; the
rest differ by one unit at a few points (the dataset's exact resampling is unpublished). Their
rasters differ by at most 0.106 on one pixel, 0.004 on average.

### Scoring

- Input NHWC `[1, 28, 28, 1]` float32: `inkOnDark` the raster, `darkOnLight` `1 - raster`.
- Top-1 and top-5 by a stable sort of the scores: ties (a quantised output has many) rank by class
  index, as `argmax` does. `onnxRecognised` counts the drawings the dataset marks
  `recognized: true`.

## Converter protocol

- stdout carries events only, one JSON object per line: `step {name}`, `progress {step, done,
  total}`, `done {seconds, bytes, sha256, drawings, accuracy}`, `failed {step, class, message}`
  (`class`: `bad_input` or `internal`). Everything else goes to stderr; the converter's own lines
  start `[convert] +<seconds>s`.
- Exit codes: 0 done, 3 bad input (an unusable source: permanent), 1 anything else (retried).
- The converter dies with the worker: `PR_SET_PDEATHSIG` and a parent watch.

### Errors

| Error | When | Retried by the studio |
| --- | --- | --- |
| `UnsupportedTask` | not Linux x86_64 | no |
| `ConvertError::BadTask` | the task is outside what the engine does | no |
| `ConvertError::Provision` | building the environment failed | yes |
| `ConvertError::Step` | a step failed: `bad_input` (e.g. a bad source) or not | `bad_input` no, else yes |
| `ConvertError::Timeout` | `RUN_TIMEOUT` passed | yes |
| `ConvertError::InvalidOutput` | the `.onnx` and report do not match, or the report is too large | yes |

## Synthetic

The synthetic engine serves `synthetic-convert`: the embedded `synthetic.onnx` (a 1.7 KB model
with the converted interface, written by `python -m convert.synthetic`; a Python test holds the
two equal) and a report of the real one's shape (345 synthetic labels, every accuracy 0, diagonal
references), so CI runs a `model_convert` job end to end with no network and no Python.

## Tests

- Rust: task checks, events and outcomes, report checks, the synthetic result, CPU provisioning
  (`tests/integration/torch_provision.rs`), the multipart `generationMeta`
  (`tests/integration/http_contract.rs`).
- Python, in the environment:
  `PYTHONPATH=python <env>/bin/python -m unittest discover -s python/convert/tests -t python`:
  SplitMix64 and FNV against their reference values, sampler determinism and line reading, RDP,
  resampling, scaling, the rasteriser on a fixed drawing, scoring, the I/O wrap.
- Real: `RUN_REAL_ENGINE_TESTS=1 CONVERT_PER_WORD=100 cargo test --test integration real_convert
  -- --nocapture` runs the spec's task through the engine on this machine; nothing is uploaded.

## Measured (dev box, 2026-11-05)

The spec's task (`quickdraw_model_int8.tflite`, opset 13, seed 3577, 100 per word), through
`real_convert`, on a 28-thread box:

| | top-1 | top-5 |
| --- | --- | --- |
| `inkOnDark` ONNX | 0.7585 | 0.9182 |
| `inkOnDark` source (LiteRT) | 0.7585 | 0.9182 |
| `inkOnDark` ONNX, recognised only | 0.8030 | 0.9561 |
| `darkOnLight` ONNX | 0.0123 | 0.0548 |
| `darkOnLight` source | 0.0121 | 0.0548 |
| `darkOnLight` ONNX, recognised only | 0.0128 | 0.0569 |

- The model reads ink 1.0 on 0.0, as the dataset's numpy bitmaps draw it; the card's
  `darkOnLight` is wrong for it.
- 34 500 drawings, 31 594 recognised; labels match the categories.
- 291 s in all: fetch 1.8 s, convert 2.0 s, gate 281 s (sampling 168 s, rasterising 22 s, scoring
  91 s). A second job with the same seed reads its gate set from `gate-sets/` (128 MB).
- `.onnx` 4.5 MB; the report 383 KB with 20 references (raw strokes carry their timings, about
  17 KB a reference), so `referenceCount` above about 45 passes the studio's 1 MiB.

## Observability

All under target `studio_worker::engine::convert`.

| `op` | When |
| --- | --- |
| `platform` | at start: `model_convert` advertised or not, and why |
| `convert` | started (source, file, opset, seed), each `step`, `progress`, the converter's `done`, finished (seconds, bytes, sha256, accuracy) or failed (permanent) |
| `runner` | each line the converter prints |
| `provision` | (target `studio_worker::engine::torch`) each provisioning step of the `convert` environment |

## Constants

| Constant | Value | Provenance | Safe range |
| --- | --- | --- | --- |
| `RUN_TIMEOUT` | 45 min | a full gate is a few minutes; a slow dataset host takes longer | 10 to 55 min |
| `FETCH_CONCURRENCY` | 64 | range reads take 0.2 to 0.8 s each | 8 to 128 |
| `SCORE_THREADS_MAX` | 8 | half the machine's threads, so a GPU job beside it keeps its CPU | 1 to 32 |
| `MAX_REPORT_BYTES` | 1 MiB | the studio's `maxReportBytes` | |
| `MAX_REFERENCES` | 100 | the studio's `ModelConversionLimits` | |
| `MAX_DRAWINGS_PER_WORD` | 1000 | the studio's `ModelConversionLimits` | |
| `CHUNK_BYTES` (Python) | 16 KiB | a raw line is about 3 KB (median), 8 KB (90th percentile) | 4 to 64 KiB |
