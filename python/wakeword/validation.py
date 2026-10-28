"""microWakeWord's `validate_nonstreaming`, with its ambient audio evaluated in chunks
(docs/engines/wake-word.md, "Memory").

Upstream builds every validation ambient window into one array (about 155 000 windows of 203 x 40
float32 for 13 h of audio: 5 GiB), and Keras copies it once more: each validation pass added
10 GiB of anonymous memory on top of training's 10 GiB (measured 2026-10-28), and the system's
out-of-memory killer ended two trainings there. Here the windows are evaluated `AMBIENT_CHUNK`
at a time, and the validation set is freed before them. Upstream already accumulates the metrics
across `evaluate` calls (it swaps `reset_metrics` for a no-op), so the result is the same.

Everything else is upstream's code at the pinned revision, unchanged. `install` refuses a revision
whose function differs from the one mirrored here, so a new revision is reviewed, never silently
diverged from.
"""

import hashlib
import inspect

import numpy as np

from wakeword.common import log

# sha256 of upstream's `validate_nonstreaming` source at microWakeWord 4665173cd35f.
UPSTREAM_SHA256 = "19ff3f4cc838bd00f554867631886b7f88cdae15abf92d7d0549a21f50eca7ef"
# Ambient windows evaluated at once: 16 384 x 203 x 40 float32 is about 0.5 GiB.
AMBIENT_CHUNK = 16384


def ambient_chunks(data_processor, mode, features_length, size):
    """Every ambient window of `mode`, as upstream's `get_data(..., "split")` yields them, `size` at a
    time: (windows, labels shaped (n, 1))."""
    windows, labels = [], []
    for provider in data_processor.feature_providers:
        for window in provider.get_feature_generator(mode, features_length, "split"):
            windows.append(window)
            labels.append(provider.label)
            if len(windows) == size:
                yield np.array(windows), np.array(labels).reshape(-1, 1)
                windows, labels = [], []
    if windows:
        yield np.array(windows), np.array(labels).reshape(-1, 1)


def validate_nonstreaming(config, data_processor, model, test_set):
    from microwakeword.train import swap_attribute

    testing_fingerprints, testing_ground_truth, _ = data_processor.get_data(
        test_set,
        batch_size=config["batch_size"],
        features_length=config["spectrogram_length"],
        truncation_strategy="truncate_start",
    )
    testing_ground_truth = testing_ground_truth.reshape(-1, 1)

    model.reset_metrics()

    result = model.evaluate(
        testing_fingerprints,
        testing_ground_truth,
        batch_size=1024,
        return_dict=True,
        verbose=0,
    )
    del testing_fingerprints

    metrics = {}
    metrics["accuracy"] = result["accuracy"]
    metrics["recall"] = result["recall"]
    metrics["precision"] = result["precision"]

    metrics["auc"] = result["auc"]
    metrics["loss"] = result["loss"]
    metrics["recall_at_no_faph"] = 0
    metrics["cutoff_for_no_faph"] = 0
    metrics["ambient_false_positives"] = 0
    metrics["ambient_false_positives_per_hour"] = 0
    metrics["average_viable_recall"] = 0

    test_set_fp = np.asarray(result["fp"])

    if data_processor.get_mode_size("validation_ambient") > 0:
        # The one change: the ambient windows in chunks, the metrics accumulating across them.
        ambient_predictions = None
        with swap_attribute(model, "reset_metrics", lambda: None):
            for windows, labels in ambient_chunks(
                data_processor, test_set + "_ambient", config["spectrogram_length"], AMBIENT_CHUNK
            ):
                ambient_predictions = model.evaluate(
                    windows,
                    labels,
                    batch_size=1024,
                    return_dict=True,
                    verbose=0,
                )
        if ambient_predictions is None:
            return metrics

        duration_of_ambient_set = (
            data_processor.get_mode_duration("validation_ambient") / 3600.0
        )

        # Other than the false positive rate, all other metrics are accumulated across
        # both test sets
        all_true_positives = np.asarray(ambient_predictions["tp"])
        ambient_false_positives = np.asarray(ambient_predictions["fp"]) - test_set_fp
        all_false_negatives = np.asarray(ambient_predictions["fn"])

        metrics["auc"] = ambient_predictions["auc"]
        metrics["loss"] = ambient_predictions["loss"]

        recall_at_cutoffs = (
            all_true_positives / (all_true_positives + all_false_negatives)
        )
        faph_at_cutoffs = ambient_false_positives / duration_of_ambient_set

        target_faph_cutoff_probability = 1.0
        for index, cutoff in enumerate(np.linspace(0.0, 1.0, 101)):
            if faph_at_cutoffs[index] == 0:
                target_faph_cutoff_probability = cutoff
                recall_at_no_faph = recall_at_cutoffs[index]
                break

        if faph_at_cutoffs[0] > 2:
            # Use linear interpolation to estimate recall at 2 faph

            # Increase index until we find a faph less than 2
            index_of_first_viable = 1
            while faph_at_cutoffs[index_of_first_viable] > 2:
                index_of_first_viable += 1

            x0 = faph_at_cutoffs[index_of_first_viable - 1]
            y0 = recall_at_cutoffs[index_of_first_viable - 1]
            x1 = faph_at_cutoffs[index_of_first_viable]
            y1 = recall_at_cutoffs[index_of_first_viable]

            recall_at_2faph = (y0 * (x1 - 2.0) + y1 * (2.0 - x0)) / (x1 - x0)
        else:
            # Lowest faph is already under 2, assume the recall is constant before this
            index_of_first_viable = 0
            recall_at_2faph = recall_at_cutoffs[0]

        x_coordinates = [2.0]
        y_coordinates = [recall_at_2faph]

        for index in range(index_of_first_viable, len(recall_at_cutoffs)):
            if faph_at_cutoffs[index] != x_coordinates[-1]:
                # Only add a point if it is a new faph
                # This ensures if a faph rate is repeated, we use the highest recall
                x_coordinates.append(faph_at_cutoffs[index])
                y_coordinates.append(recall_at_cutoffs[index])

        # Use trapezoid rule to estimate the area under the curve, then divide by 2.0 to get the average recall
        average_viable_recall = (
            np.trapezoid(np.flip(y_coordinates), np.flip(x_coordinates)) / 2.0
        )

        metrics["recall_at_no_faph"] = recall_at_no_faph
        metrics["cutoff_for_no_faph"] = target_faph_cutoff_probability
        metrics["ambient_false_positives"] = ambient_false_positives[50]
        metrics["ambient_false_positives_per_hour"] = faph_at_cutoffs[50]
        metrics["average_viable_recall"] = average_viable_recall

    return metrics


def install():
    """Use [validate_nonstreaming] in upstream's training loop; refuse a revision it does not mirror."""
    import microwakeword.train as upstream

    source = inspect.getsource(upstream.validate_nonstreaming)
    seen = hashlib.sha256(source.encode()).hexdigest()
    if seen != UPSTREAM_SHA256:
        raise RuntimeError(
            "microWakeWord's validate_nonstreaming is not the revision python/wakeword/validation.py "
            f"mirrors (sha256 {seen}); review the change and mirror it"
        )
    upstream.validate_nonstreaming = validate_nonstreaming
    log(f"validation evaluates ambient audio {AMBIENT_CHUNK} windows at a time")
