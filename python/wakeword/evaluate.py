"""Stage `evaluate`: stream held-out audio through the quantised streaming model, as a device would.

The stage measures; it decides nothing.  For every cutoff on a grid it counts false accepts on each
ambient set, and it keeps every held-out positive's and near miss's best score, so the worker can
choose the cutoff and write the report (docs/engines/wake-word.md, "Choosing the cutoff").  It also
loads the model in TensorFlow Lite Micro, the runtime ESPHome and the phone run, and measures the
smallest tensor arena it runs in.
"""

import glob
import hashlib
import os

import numpy as np
from numpy.lib.stride_tricks import sliding_window_view

from wakeword.common import log, write_json

FRAME_SECONDS = 0.01
# TensorFlow Lite Micro's smallest arena is searched between these sizes, in bytes.
ARENA_SEARCH_MIN = 1024
ARENA_SEARCH_MAX = 4 * 1024 * 1024
ARENA_SEARCH_PRECISION = 16


def cutoff_grid(step):
    count = int(round(1 / step))
    return [round(i * step, 4) for i in range(count + 1)]


def count_false_accepts(probabilities, cutoffs, cooldown):
    """False accepts per cutoff on one track, counted as microWakeWord's test counts them.

    A detection is an accept when no accept happened in the `cooldown` slices before it; the first
    `cooldown - 1` slices of a track never accept.
    """
    counts = []
    for cutoff in cutoffs:
        above = np.flatnonzero(probabilities > cutoff)
        count = 0
        allowed_from = cooldown - 1
        index = int(np.searchsorted(above, allowed_from))
        while index < above.size:
            count += 1
            allowed_from = int(above[index]) + cooldown
            index = int(np.searchsorted(above, allowed_from))
        counts.append(count)
    return counts


def windowed(probabilities, window):
    if probabilities.size < window:
        return np.zeros(0, dtype=np.float32)
    return sliding_window_view(probabilities, window).mean(axis=-1)


def _mmaps(directory):
    from mmap_ninja.ragged import RaggedMmap

    paths = sorted(glob.glob(os.path.join(directory, "**", "*_mmap"), recursive=True))
    if not paths:
        raise FileNotFoundError(f"no features under {directory}")
    return [RaggedMmap(path) for path in paths]


def _as_features(spectrogram):
    if np.issubdtype(spectrogram.dtype, np.uint16):
        return spectrogram.astype(np.float32) * 0.0390625
    return spectrogram


def _ambient(model, directory, cutoffs, window, cooldown):
    totals = np.zeros(len(cutoffs), dtype=np.int64)
    frames = 0
    for mmap in _mmaps(directory):
        for index in range(len(mmap)):
            track = _as_features(mmap[index])
            frames += track.shape[0]
            probabilities = windowed(np.asarray(model.predict_spectrogram(track)), window)
            totals += np.asarray(count_false_accepts(probabilities, cutoffs, cooldown))
    return totals.tolist(), frames * FRAME_SECONDS / 3600


def _scores(model, mmap_path, window, cooldown):
    from mmap_ninja.ragged import RaggedMmap

    mmap = RaggedMmap(mmap_path)
    scores = []
    for index in range(len(mmap)):
        probabilities = np.asarray(model.predict_spectrogram(_as_features(mmap[index])))
        smoothed = windowed(probabilities[cooldown:], window)
        scores.append(float(smoothed.max()) if smoothed.size else 0.0)
    return sorted(round(s, 5) for s in scores)


def _micro_arena(path):
    """The smallest tensor arena TensorFlow Lite Micro runs the model in, or why it cannot."""
    from tflite_micro.python.tflite_micro import runtime

    def runs(size):
        try:
            interpreter = runtime.Interpreter.from_file(path, arena_size=size)
            details = interpreter.get_input_details(0)
            interpreter.set_input(np.zeros(details["shape"], dtype=details["dtype"]), 0)
            interpreter.invoke()
            interpreter.get_output(0)
            return True
        except Exception as error:  # every refusal is a size that does not fit, but the largest
            if size == ARENA_SEARCH_MAX:
                raise RuntimeError(f"TensorFlow Lite Micro cannot run the model: {error}") from error
            return False

    runs(ARENA_SEARCH_MAX)
    low, high = ARENA_SEARCH_MIN, ARENA_SEARCH_MAX
    while high - low > ARENA_SEARCH_PRECISION:
        middle = (low + high) // 2
        if runs(middle):
            high = middle
        else:
            low = middle
    return high


def run(request, events):
    from microwakeword.inference import Model

    work = request["workDir"]
    settings = request["settings"]["evaluation"]
    stride = request["settings"]["training"]["model"]["stride"]
    window = settings["slidingWindow"]
    cooldown = settings["cooldownSlices"]
    cutoffs = cutoff_grid(settings["cutoffStep"])
    path = os.path.join(work, "train", "tflite_stream_state_internal_quant", "stream_state_internal_quant.tflite")
    model = Model(path, stride=stride)

    ambient = []
    for index, ambient_set in enumerate(request["evaluation"]["ambient"]):
        counts, hours = _ambient(model, ambient_set["dir"], cutoffs, window, cooldown)
        ambient.append({"name": ambient_set["name"], "split": ambient_set["split"], "hours": hours, "falseAccepts": counts})
        log(f"ambient {ambient_set['name']} ({ambient_set['split']}): {hours:.2f} h")
        events.progress("evaluate ambient", index + 1, len(request["evaluation"]["ambient"]))

    features = os.path.join(work, "features")
    positive, near_miss = [], []
    for split in ("validation", "testing"):
        for language in request["languages"]:
            code = language["code"]
            scores = _scores(model, os.path.join(features, "positive", split, f"{code}_mmap"), window, cooldown)
            positive.append({"language": code, "split": split, "scores": scores})
        near_miss_dir = os.path.join(features, "near_miss", split)
        for mmap_path in sorted(glob.glob(os.path.join(near_miss_dir, "*_mmap"))):
            phrase = os.path.basename(mmap_path)[: -len("_mmap")]
            near_miss.append({"phrase": phrase, "split": split, "scores": _scores(model, mmap_path, window, cooldown)})
        events.progress("evaluate samples", 1 if split == "validation" else 2, 2)

    with open(path, "rb") as file:
        model_bytes = file.read()
    arena = _micro_arena(path)
    log(f"TensorFlow Lite Micro runs the model in a {arena} byte arena")
    evaluation = {
        "cutoffs": cutoffs,
        "slidingWindow": window,
        "cooldownSlices": cooldown,
        "stride": stride,
        "ambient": ambient,
        "positive": positive,
        "nearMiss": near_miss,
        "model": {
            "path": path,
            "bytes": len(model_bytes),
            "sha256": hashlib.sha256(model_bytes).hexdigest(),
            "tensorArenaMinBytes": arena,
        },
    }
    held_out = os.path.join(features, "own_held_out", "testing", "own_held_out_mmap")
    if request.get("ownVoice"):
        scores = _scores(model, held_out, window, cooldown)
        evaluation["ownVoice"] = {"heldOut": scores}
        log(f"own voice: {len(scores)} held-out takes measured")
    out = os.path.join(work, "evaluation.json")
    write_json(out, evaluation)
    return {"evaluation": out, "modelBytes": len(model_bytes), "tensorArenaMinBytes": arena}
