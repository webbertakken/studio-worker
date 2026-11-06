"""Top-1 and top-5 accuracy of a classifier on the gate set (docs/engines/convert.md, "Gate").

A model (one per thread) is a function from one (1, 28, 28, 1) float32 input to its scores.
Ranks are taken with a stable sort on the negated scores, so tied scores (a quantised output has many) rank by
class index, as `argmax` does.
"""

import threading
from concurrent.futures import ThreadPoolExecutor

import numpy as np

POLARITIES = ("inkOnDark", "darkOnLight")


def with_polarity(raster, polarity):
    """The model input for a raster (ink 1.0 on 0.0) in a polarity: NHWC (1, 28, 28, 1) float32."""
    if polarity == "inkOnDark":
        image = raster
    elif polarity == "darkOnLight":
        image = np.float32(1.0) - raster
    else:
        raise ValueError(f"unknown polarity {polarity!r}")
    return np.ascontiguousarray(image, dtype=np.float32).reshape(1, 28, 28, 1)


def ranks(scores, k=5):
    """The `k` best classes, best first, ties by class index."""
    flat = np.asarray(scores).reshape(-1)
    return np.argsort(-flat.astype(np.float64), kind="stable")[:k]


class Tally:
    def __init__(self):
        self.total = self.top1 = self.top5 = 0

    def add(self, truth, best):
        self.total += 1
        self.top1 += int(best[0] == truth)
        self.top5 += int(truth in best[:5])

    def accuracy(self):
        if self.total == 0:
            return {"top1": 0.0, "top5": 0.0}
        return {"top1": round(self.top1 / self.total, 4), "top5": round(self.top5 / self.total, 4)}


def predict_all(make_model, inputs, threads, progress=None):
    """The best five classes of every input, `threads` inputs at a time, a model per thread
    (`make_model()`; a LiteRT interpreter is not thread-safe)."""
    local = threading.local()

    def predict(x):
        if not hasattr(local, "model"):
            local.model = make_model()
        return ranks(local.model(x))

    results = []
    with ThreadPoolExecutor(max_workers=max(1, threads)) as pool:
        for best in pool.map(predict, inputs):
            results.append(best)
            if progress:
                progress(len(results), len(inputs))
    return results


def score(samples, polarity, make_onnx, make_source, threads=1, progress=None):
    """`samples`: (class index, raster, recognised). The polarity's three accuracies."""
    inputs = [with_polarity(raster, polarity) for _, raster, _ in samples]
    onnx_best = predict_all(make_onnx, inputs, threads, progress)
    source_best = predict_all(make_source, inputs, threads)
    onnx_all, source_all, onnx_recognised = Tally(), Tally(), Tally()
    for (truth, _, recognised), best_onnx, best_source in zip(samples, onnx_best, source_best):
        onnx_all.add(truth, best_onnx)
        source_all.add(truth, best_source)
        if recognised:
            onnx_recognised.add(truth, best_onnx)
    return {
        "onnx": onnx_all.accuracy(),
        "source": source_all.accuracy(),
        "onnxRecognised": onnx_recognised.accuracy(),
    }
