"""Stage `train`: microWakeWord's own training, conversion and streaming test, as its notebook runs them.

The request's settings become upstream's `training_parameters.yaml` and command line; upstream's
`model_train_eval` runs unchanged in this process, so its log lines land in the job's log.
"""

import os
import runpy
import shutil
import sys

import yaml

from wakeword.common import GIB, log, write_json

# Where upstream writes the quantised streaming model, under its train_dir.
STREAMING_MODEL = os.path.join("tflite_stream_state_internal_quant", "stream_state_internal_quant.tflite")


def _feature_sets(request):
    work = request["workDir"]
    root = request["datasets"]["root"]
    weights = request["settings"]["training"]["featureWeights"]

    def feature_set(features_dir, name, truth, truncation):
        weight = weights[name]
        if truncation is None:
            # Ambient-only sets are split into windows; the rest are read at random offsets.
            truncation = "split" if weight["samplingWeight"] == 0 else "random"
        return {
            "features_dir": features_dir,
            "truth": truth,
            "truncation_strategy": truncation,
            "sampling_weight": float(weight["samplingWeight"]),
            "penalty_weight": float(weight["penaltyWeight"]),
            "type": "mmap",
        }

    sets = [feature_set(os.path.join(work, "features", "positive"), "positive", True, "truncate_start")]
    if request["negatives"]:
        sets.append(feature_set(os.path.join(work, "features", "near_miss"), "nearMiss", False, "truncate_start"))
    for negative in request["datasets"]["negativeFeatures"]:
        name = negative["name"]
        sets.append(feature_set(os.path.join(root, "negative-features", name), name, False, None))
    # Every speech group: one weighed 0 is never sampled, and its validation_ambient audio joins
    # the audio upstream counts false accepts on when it picks the checkpoint to keep.
    for group in sorted({s["group"] for s in request["datasets"]["speech"]}):
        sets.append(feature_set(os.path.join(root, f"{group}-features"), group, False, None))
    return sets


def config_of(request, train_dir):
    """upstream's training configuration for this request."""
    training = request["settings"]["training"]
    return {
        "window_step_ms": 10,
        "train_dir": train_dir,
        "features": _feature_sets(request),
        "training_steps": training["steps"],
        "positive_class_weight": training["positiveClassWeight"],
        "negative_class_weight": training["negativeClassWeight"],
        "learning_rates": training["learningRates"],
        "batch_size": training["batchSize"],
        "time_mask_max_size": training["timeMaskMaxSize"],
        "time_mask_count": training["timeMaskCount"],
        "freq_mask_max_size": training["freqMaskMaxSize"],
        "freq_mask_count": training["freqMaskCount"],
        "eval_step_interval": training["evalStepInterval"],
        "clip_duration_ms": training["clipDurationMs"],
        "target_minimization": training["targetMinimization"],
        "minimization_metric": training["minimizationMetric"],
        "maximization_metric": training["maximizationMetric"],
    }


def arguments(config_path, model):
    return [
        "model_train_eval",
        f"--training_config={config_path}",
        "--train", "1",
        "--restore_checkpoint", "1",
        "--test_tf_nonstreaming", "0",
        "--test_tflite_nonstreaming", "0",
        "--test_tflite_nonstreaming_quantized", "0",
        "--test_tflite_streaming", "0",
        "--test_tflite_streaming_quantized", "1",
        "--use_weights", "best_weights",
        "mixednet",
        "--pointwise_filters", model["pointwiseFilters"],
        "--repeat_in_block", model["repeatInBlock"],
        "--mixconv_kernel_sizes", model["mixconvKernelSizes"],
        "--residual_connection", model["residualConnection"],
        "--first_conv_filters", str(model["firstConvFilters"]),
        "--first_conv_kernel_size", str(model["firstConvKernelSize"]),
        "--stride", str(model["stride"]),
    ]


def _cap_device_memory(tf, budget_gib):
    gpus = tf.config.list_physical_devices("GPU")
    if not gpus:
        log("no GPU visible to TensorFlow: training on the CPU")
        return False
    tf.config.set_logical_device_configuration(
        gpus[0], [tf.config.LogicalDeviceConfiguration(memory_limit=int(budget_gib * 1024))]
    )
    log(f"TensorFlow device memory capped at {budget_gib} GiB")
    return True


def run(request, events):
    import tensorflow as tf

    work = request["workDir"]
    train_dir = os.path.join(work, "train")
    if os.path.exists(train_dir):
        shutil.rmtree(train_dir)
    os.makedirs(train_dir)
    on_gpu = _cap_device_memory(tf, request["settings"]["deviceMemoryBudgetGib"])
    tf.keras.utils.set_random_seed(request["seed"])

    config = config_of(request, train_dir)
    config_path = os.path.join(work, "training_parameters.yaml")
    with open(config_path, "w", encoding="utf-8") as file:
        yaml.safe_dump(config, file)

    argv = sys.argv
    sys.argv = arguments(config_path, request["settings"]["training"]["model"])
    try:
        runpy.run_module("microwakeword.model_train_eval", run_name="__main__")
    finally:
        sys.argv = argv

    model = os.path.join(train_dir, STREAMING_MODEL)
    if not os.path.isfile(model):
        raise RuntimeError(f"training ended without a streaming model at {model}")
    peak = tf.config.experimental.get_memory_info("GPU:0")["peak"] / GIB if on_gpu else 0.0
    summary = {"model": model, "config": config_path, "tensorflowPeakGib": round(peak, 2)}
    write_json(os.path.join(work, "train.json"), summary)
    return summary
