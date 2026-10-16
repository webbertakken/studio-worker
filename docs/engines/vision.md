# Vision engine: animal detection and species classification

The worker answers two questions about a picture, on its own GPU when it has one:

- **Is there an animal, and where?** A detector (MegaDetector, or any YOLO export) returns boxes
  with a class and a confidence; a picture without an animal is blank, so camera trap blanks
  can be filtered out.
- **Which species?** A classifier (SpeciesNet over 2 498 camera trap species, the 525-species bird
  EfficientNet, or any ImageNet-style export) ranks its classes with confidences, and their
  taxonomy when its labels carry one (SpeciesNet's do).

Together (`POST /vision/species`) the detector finds the animals and the classifier ranks each
one on its own crop, so a small bird in a wide camera trap frame is classified on the bird, not
the frame.  Without a named model the first enabled one doing the task answers: in the seeded
catalogue MegaDetector and SpeciesNet, which names a fox a fox where a bird-only classifier can
only pick its nearest bird.

Implemented in [`src/engine/vision/`](../../src/engine/vision) (inference),
[`src/local_vision.rs`](../../src/local_vision.rs) (serving and jobs) and
[`src/local_api/vision.rs`](../../src/local_api/vision.rs) (HTTP).  Cargo feature `vision`, on
by default.  Local only: the studio never offers vision jobs, and the worker does not advertise
the `vision` kind to it.

## Terms

| Term | Meaning |
| --- | --- |
| vision model | a catalogue model of kind `vision` on engine `onnx-vision`, with `cliDefaults.vision` |
| detector | a vision model with `task: detect`: YOLO-style boxes |
| classifier | a vision model with `task: classify`: one score per class |
| labels | the class names by output index: inline, or the model's `labels` file |
| animal class | a detector class listed in `animalLabels` (every class when absent) |
| blank | a picture in which the detector finds no animal class at or above the threshold |

## Models

Seeded in the catalogue; their files download on first use, pinned by revision and sha256.

| id | task | input | classes | size | licence |
| --- | --- | --- | --- | --- | --- |
| `megadetector-v5a` | detect | 1280² letterbox | `animal`, `person`, `vehicle` | 562 MB | MIT ([CameraTraps v5.0](https://github.com/microsoft/CameraTraps/releases/tag/v5.0), ONNX by [malyvsen](https://huggingface.co/malyvsen/megadetector-v5a-onnx)) |
| `speciesnet-v4a` | classify | 480² channels last, an animal's crop | 2 498 camera trap species, with taxonomy | 224 MB | Apache-2.0 ([SpeciesNet](https://github.com/google/cameratrapai) v4.0.1a always-crop, ONNX by [daslearning](https://huggingface.co/daslearning/Google-SpeciesNet-ONNX), labels from [the 4.0.1a release](https://huggingface.co/earthtoolsmaker/speciesnet)) |
| `bird-species-efficientnet-b2` | classify | 260² | 525 bird species | 34 MB | MIT ([chriamue/bird-species-classifier](https://huggingface.co/chriamue/bird-species-classifier)) |

Any other model is added like every catalogue model (`POST /models` or `models.json`):

```json
{
  "id": "yolov8n-coco",
  "displayName": "YOLOv8n (COCO)",
  "kind": "vision",
  "vramGbEstimate": 0.5,
  "source": {
    "engine": "onnx-vision",
    "files": [
      { "role": "model", "url": "https://…/yolov8n.onnx", "filename": "yolov8n.onnx" },
      { "role": "labels", "url": "https://…/coco.txt", "filename": "coco.txt" }
    ],
    "cliDefaults": {
      "cfgScale": 0, "steps": 0, "width": 640, "height": 640,
      "vision": { "task": "detect", "animalLabels": ["bird", "cat", "dog"], "minConfidence": 0.25 }
    }
  }
}
```

### `cliDefaults.vision`

| Key | Meaning | Default |
| --- | --- | --- |
| `task` | `detect` or `classify` | required |
| `labels` | class names by output index | the `labels` file |
| `animalLabels` | detector classes that are animals | every class |
| `mean`, `std` | per-channel RGB normalisation after scaling to 0..1 | 0 and 1 |
| `probabilities` | the classifier already outputs probabilities | false: logits, softmaxed |
| `minConfidence` | lowest confidence answered | 0.25 (detect), 0 (classify) |
| `iou` | overlap above which a weaker box of the same class is dropped | 0.45 |
| `topK` | classes a classifier answers | 5 (at most 100) |
| `channelsLast` | the network takes `1 x height x width x 3` (Keras-style) | false: `1 x 3 x height x width` |
| `cropMargin` | fraction of an animal's box per side its crop adds before this classifier ranks it | 0.1 |

`cliDefaults.width` and `height` are the network's input size.  A load refuses a model whose
input is not one f32 tensor of that shape (a dynamic dimension fits any size), a classifier whose
output does not hold one score per label, a detector whose animal labels are not among its
labels, and labels that skip an index.

### Labels files

- a Hugging Face `config.json`: its `id2label`;
- a JSON array of names;
- text, one class per line; a blank line between classes is refused, since every class after
  it would answer for the wrong index.  A line of seven `;`-separated fields is a
  [SpeciesNet](https://github.com/google/cameratrapai) label,
  `id;class;order;family;genus;species;common name`: its common name is the label and the rest
  its `taxonomy` (`class`, `order`, `family`, `genus`, `species`, `scientificName`).

A labels file at the top of `models_root` with the same name is reused only at its declared
size, as is the weights file: names like `config.json` are common to many models.

## Pipeline

**Detect** (YOLOv5 and YOLOv8+ exports):

1. Letterbox: the picture scaled to fit the input with its aspect kept, centred on grey 114,
   scaled to 0..1.
2. The output layout is read from its shape and the class count: `[1, N, 5 + C]` (YOLOv5,
   objectness times the best class), `[1, 4 + C, N]` (YOLOv8+), or `[1, N, 4 + C]`.  Any other
   shape fails the job, naming both expectations.
3. Boxes below `minConfidence` are dropped, then non-maximum suppression per class (`iou`),
   strongest first, at most 300.
4. Boxes are mapped back onto the picture, clamped to it, in pixels and as fractions.

**Classify:** the picture stretched to the input, normalised with `mean` and `std`, the output
softmaxed (unless `probabilities`), the `topK` classes at or above `minConfidence`.

**Species:** detect; each animal box (at most 20, strongest first) widened by the classifier's
`cropMargin` and cropped; each crop classified.  SpeciesNet's always-crop model is trained on the
detector's exact boxes, so its margin is 0.  A blank picture never runs the classifier.

Pictures are PNG, JPEG or WebP, at most 25 MiB, turned upright by their EXIF orientation.

## Serving, memory and the GPU

- **Not loaded** (the default): a request is a transient job.  It takes the one-job gate
  (`503 busy` while a studio or local job holds it), is admitted next to the loaded models
  (`409 insufficient_memory`), loads the model, answers, and frees it.  Nothing stays in memory
  between requests: a worker without vision traffic holds no vision weights.
- **Loaded** (`POST /models/:id/load`, the tray UI's Load): the model answers on its own lane,
  warm, beside other jobs, without the gate.  Keep the detector loaded for a camera trap batch;
  MegaDetector's load (about a second on a warm disk, minutes on its first download) is then
  paid once.
- **GPU:** vision shares the process's ONNX Runtime with the LaMa and streaming speech engines
  ([ONNX engine](onnx.md#flavour-cpu-or-cuda)).  With a complete CUDA runtime the CUDA build is
  provisioned and every vision session registers the CUDA provider strictly: a CUDA session that
  cannot load fails the load instead of running on the CPU unnoticed.  Otherwise the model runs
  on the CPU.  Each answer says where in `device` (`cuda` or `cpu`).  macOS and Windows without
  CUDA run on the CPU.
- **What stays in memory:** unloading a model (or ending a transient job) frees its weights.
  The CUDA context the first CUDA session creates (about 0.4 GB on an RTX 4090) stays for the
  daemon's lifetime, shared by every ONNX engine; only stopping the daemon returns it.

Every request is recorded as a job of kind `vision` (source `local`, or `lane` when loaded),
with a thumbnail of the picture; `species` records the detector and the classifier as two jobs.

## Local API

All three routes take the picture the same three ways; see [local API](../local-api.md#vision).

| Route | Options | Answers |
| --- | --- | --- |
| `POST /vision/detect` | `model`, `minConfidence`, `iou` | `{model, device, picture, animalPresent, detections, elapsedMs}` |
| `POST /vision/classify` | `model`, `minConfidence`, `topK` | `{model, device, picture, predictions, elapsedMs}` |
| `POST /vision/species` | `detector`, `classifier`, `minConfidence` (detector), `iou`, `topK` | `{detector, classifier, picture, animalPresent, detections (each animal with species), elapsedMs}` |

## Observability

| `op` | Target | When |
| --- | --- | --- |
| `load` | `studio_worker::engine::vision` | a vision model loaded: task, device, flavour, input and output names, label count, time |
| `transient` | `studio_worker::lifecycle` | a model loaded for one job, or failed to |
| `admit` | `studio_worker::lifecycle` | a transient job admitted or refused |
| `detect` / `classify` / `species` | `studio_worker::vision` | a request answered: model, device, counts, time |
| `vision` | `studio_worker::local_api` | a request refused, with its status, code and reason |
| `job` | `studio_worker::job` | each vision job started and finished |

## Parity

Checked against SpeciesNet's own test pictures (`google/cameratrapai` `test_data`, whose tests pin
the expected class and MegaDetector box of each): every MegaDetector box lands within about 0.002
of the frame of the reference box, and SpeciesNet on the reference crops names every expected
class (elephants, also greyscale, CMYK, truncated and EXIF-rotated; black bear, cattle, dog,
ocelot, human, three blanks) at 98.7 % to 99.9 %.
