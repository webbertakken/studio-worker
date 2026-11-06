"""studio-worker's model conversion: one `model_convert` job (docs/engines/convert.md).

    python -m convert <request.json>

Steps, in order: fetch, convert, verify, labels, gate, references. stdout carries events only,
one JSON object per line; everything else anything prints goes to stderr, each line of ours
starting `[convert]`. Exit codes: 0 done, 1 anything else, 3 bad input.
"""

import base64
import hashlib
import json
import os
import sys
import threading
import time
import traceback

EXIT_DONE = 0
EXIT_INTERNAL = 1
EXIT_BAD_INPUT = 3
PARENT_WATCH_SECONDS = 2.0
PROGRESS_SECONDS = 10.0


class Events:
    """The real stdout, for events; everything printed goes to stderr."""

    def __init__(self):
        self.out = os.fdopen(os.dup(1), "w", buffering=1)
        os.dup2(2, 1)
        sys.stdout = sys.stderr
        self.lock = threading.Lock()

    def emit(self, event, **fields):
        with self.lock:
            self.out.write(json.dumps({"event": event, **fields}) + "\n")


STARTED = time.monotonic()


def log(message):
    print(f"[convert] +{time.monotonic() - STARTED:.1f}s {message}", file=sys.stderr, flush=True)


def die_with_parent():
    """End this process when the worker that started it ends, however it ends."""
    parent = os.getppid()
    try:
        import ctypes
        import signal

        ctypes.CDLL("libc.so.6", use_errno=True).prctl(1, signal.SIGKILL)  # PR_SET_PDEATHSIG
    except Exception as error:
        log(f"no parent-death signal ({error}); watching the parent")

    def watch():
        while True:
            time.sleep(PARENT_WATCH_SECONDS)
            if os.getppid() != parent:
                os._exit(EXIT_INTERNAL)

    threading.Thread(target=watch, daemon=True, name="parent-watch").start()


class StepFailed(Exception):
    def __init__(self, step, message, bad_input=False):
        super().__init__(message)
        self.step, self.bad_input = step, bad_input


class Progress:
    """A progress event and log line at most every `PROGRESS_SECONDS`, and at the end."""

    def __init__(self, events, step):
        self.events, self.step, self.last = events, step, 0.0

    def __call__(self, done, total):
        now = time.monotonic()
        if done == total or now - self.last >= PROGRESS_SECONDS:
            self.last = now
            self.events.emit("progress", step=self.step, done=done, total=total)
            log(f"{self.step}: {done}/{total}")


def read_lines(path):
    with open(path, encoding="utf-8") as file:
        lines = [line.rstrip("\r\n") for line in file]
    while lines and not lines[-1]:
        lines.pop()
    return lines


def by_byte_value(words):
    return sorted(words, key=lambda word: word.encode("utf-8"))


def pick_references(labels, seed, count):
    """`count` distinct label indices, by seed: a partial Fisher-Yates shuffle of the labels."""
    from convert.prng import SplitMix64

    stream = SplitMix64.keyed(seed, "references")
    order = list(range(len(labels)))
    picked = []
    for i in range(min(count, len(order))):
        j = i + stream.below(len(order) - i)
        order[i], order[j] = order[j], order[i]
        picked.append(order[i])
    return picked


def raster_base64(raster):
    import numpy as np

    return base64.b64encode(np.ascontiguousarray(raster, dtype="<f4").tobytes()).decode("ascii")


def run(request, events):
    task = request["task"]
    gate_set = task["gateSet"]
    durations = {}

    def step(name):
        events.emit("step", name=name)
        log(f"step {name}")

    # fetch: the source model, its labels, the dataset's categories.
    step("fetch")
    started = time.monotonic()
    from convert import converter, sampler
    from convert.converter import ConvertError

    try:
        files = converter.fetch(task["source"], [task["file"], task["labels"]])
        categories = [line for line in sampler.get_text(request["categoriesUrl"]).split("\n") if line]
    except ConvertError as error:
        raise StepFailed("fetch", str(error), bad_input=True) from error
    except Exception as error:
        raise StepFailed("fetch", f"{type(error).__name__}: {error}") from error
    durations["fetch"] = int((time.monotonic() - started) * 1000)
    log(f"fetched {task['source']} {task['file']} ({os.path.getsize(files[task['file']])} bytes)")

    # convert
    step("convert")
    started = time.monotonic()
    try:
        wrapped = converter.convert(files[task["file"]], int(task["opset"]), request["output"])
    except Exception as error:
        raise StepFailed("convert", f"{type(error).__name__}: {error}") from error
    durations["convert"] = int((time.monotonic() - started) * 1000)
    log(f"converted at opset {task['opset']}; float32 wrapping {wrapped}")

    # verify: the interface, against the labels.
    step("verify")
    labels = read_lines(files[task["labels"]])
    make_onnx = lambda: converter.OnnxModel(request["output"])  # noqa: E731
    make_source = lambda: converter.TfliteModel(files[task["file"]])  # noqa: E731
    input_spec, output_spec = make_onnx().interface()
    try:
        converter.verify(input_spec, output_spec, len(labels))
    except ConvertError as error:
        raise StepFailed("verify", str(error)) from error
    log(f"input {input_spec}, output {output_spec}")

    step("labels")
    matches = labels == by_byte_value(categories)
    log(f"{len(labels)} labels; {len(categories)} categories; labelsMatchCategories={matches}")

    # gate: draw, rasterise, score.
    step("gate")
    started = time.monotonic()
    from convert import recipe, scoring

    seed, per_word = int(gate_set["seed"]), int(gate_set["drawingsPerWord"])
    cache_dir = os.path.join(request["gateCache"], gate_set["dataset"], str(seed)) if request.get("gateCache") else None
    try:
        gate = sampler.draw_gate_set(request["datasetUrl"], labels, seed, per_word, int(request["concurrency"]),
                                     cache_dir, Progress(events, "sample"))
    except Exception as error:
        raise StepFailed("gate", f"sampling: {type(error).__name__}: {error}") from error
    samples, recognised = [], 0
    rasters = {}
    for index, label in enumerate(labels):
        for n, drawing in enumerate(gate[label]):
            raster = recipe.raster_of(drawing["drawing"])
            if n == 0:
                rasters[label] = raster
            ok = bool(drawing.get("recognized"))
            recognised += int(ok)
            samples.append((index, raster, ok))
    log(f"gate set: {len(samples)} drawings, {recognised} recognised")
    accuracy = {}
    for polarity in task["polarities"]:
        accuracy[polarity] = scoring.score(samples, polarity, make_onnx, make_source, int(request["threads"]),
                                           Progress(events, f"score {polarity}"))
        log(f"{polarity}: {json.dumps(accuracy[polarity])}")
    durations["gate"] = int((time.monotonic() - started) * 1000)

    step("references")
    references = []
    for index in pick_references(labels, seed, int(task["referenceCount"])):
        label = labels[index]
        drawing = gate[label][0]
        references.append({
            "label": label,
            "keyId": drawing["key_id"],
            "strokes": drawing["drawing"],
            "raster": raster_base64(rasters[label]),
        })

    import onnxruntime
    import tf2onnx

    with open(request["output"], "rb") as file:
        onnx_bytes = file.read()
    report = {
        "kind": "model_convert",
        "sha256": hashlib.sha256(onnx_bytes).hexdigest(),
        "bytes": len(onnx_bytes),
        "source": task["source"],
        "file": task["file"],
        "converter": {"name": "tf2onnx", "version": tf2onnx.__version__, "opset": int(task["opset"])},
        "runtime": {"name": "onnxruntime", "version": onnxruntime.__version__},
        "input": input_spec,
        "output": output_spec,
        "labels": labels,
        "labelsMatchCategories": matches,
        "gate": {
            "dataset": gate_set["dataset"],
            "seed": seed,
            "drawingsPerWord": per_word,
            "drawings": len(samples),
            "recognised": recognised,
            "recipe": task["recipe"],
            "accuracy": accuracy,
        },
        "references": references,
        "durationsMs": durations,
    }
    with open(request["report"], "w", encoding="utf-8") as file:
        json.dump(report, file)
    return report


def main(argv):
    events = Events()
    die_with_parent()
    started = time.monotonic()
    if len(argv) != 2:
        events.emit("failed", step="request", **{"class": "bad_input", "message": "usage: python -m convert <request.json>"})
        return EXIT_BAD_INPUT
    try:
        with open(argv[1], encoding="utf-8") as file:
            request = json.load(file)
        report = run(request, events)
    except StepFailed as error:
        log(f"step {error.step} failed: {error}")
        events.emit("failed", step=error.step, **{"class": "bad_input" if error.bad_input else "internal",
                                                  "message": str(error)})
        return EXIT_BAD_INPUT if error.bad_input else EXIT_INTERNAL
    except Exception as error:
        traceback.print_exc()
        events.emit("failed", step="internal", **{"class": "internal", "message": f"{type(error).__name__}: {error}"})
        return EXIT_INTERNAL
    gate = report["gate"]
    events.emit("done", seconds=round(time.monotonic() - started, 1), bytes=report["bytes"],
                sha256=report["sha256"], drawings=gate["drawings"], accuracy=gate["accuracy"])
    return EXIT_DONE


if __name__ == "__main__":
    sys.exit(main(sys.argv))
