"""studio-worker's wake word trainer: one stage of one training job (docs/engines/wake-word.md).

    python -m wakeword <stage> <request.json>

Stages, in order: prepare, samples, features, train, evaluate. stdout carries events only, one JSON
object per line; everything else anything prints goes to stderr. Exit codes: 0 done, 1 anything
else, 2 out of memory, 3 bad input, 4 disk full.
"""

import importlib
import resource
import sys
import time
import traceback

from wakeword.common import (
    EXIT_BAD_INPUT,
    EXIT_DISK_FULL,
    EXIT_DONE,
    EXIT_INTERNAL,
    EXIT_OUT_OF_MEMORY,
    BadInput,
    DeviceMemory,
    Events,
    die_with_parent,
    first_sentences,
    is_disk_full,
    is_out_of_memory,
    read_json,
)

STAGES = ("prepare", "samples", "features", "train", "evaluate")


def main(argv):
    events = Events()
    die_with_parent()
    if len(argv) != 3 or argv[1] not in STAGES:
        events.emit(
            "failed",
            **{"class": "bad_input", "message": f"usage: python -m wakeword <{'|'.join(STAGES)}> <request.json>"},
        )
        return EXIT_BAD_INPUT
    stage = argv[1]
    started = time.monotonic()
    memory = DeviceMemory()
    try:
        request = read_json(argv[2])
        events.emit("stage", name=stage)
        module = importlib.import_module(f"wakeword.{stage}")
        summary = module.run(request, events) or {}
        events.emit(
            "done",
            stage=stage,
            seconds=round(time.monotonic() - started, 1),
            deviceMemoryPeakMib=memory.stop(),
            # ru_maxrss is in KiB on Linux.
            hostMemoryPeakMib=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss // 1024,
            summary=summary,
        )
        return EXIT_DONE
    except BadInput as error:
        events.emit("failed", **{"class": "bad_input", "message": str(error)})
        return EXIT_BAD_INPUT
    except BaseException as error:  # noqa: BLE001 - every failure is named on the way out
        traceback.print_exc()
        if is_disk_full(error):
            events.emit("failed", **{"class": "disk_full", "message": f"{error.strerror}: {error.filename}"})
            return EXIT_DISK_FULL
        if is_out_of_memory(error):
            events.emit("failed", **{"class": "out_of_memory", "message": first_sentences(str(error))})
            return EXIT_OUT_OF_MEMORY
        events.emit("failed", **{"class": "internal", "message": f"{type(error).__name__}: {error}"})
        return EXIT_INTERNAL


if __name__ == "__main__":
    sys.exit(main(sys.argv))
