"""studio-worker's torch runner: one pipeline, one job (docs/engines/torch.md, "Runner protocol").

    python -u runner.py <request.json>

stdout carries events only, one JSON object per line; everything else anything prints goes to
stderr. Exit codes: 0 done, 2 out of device memory, 3 bad input, 1 anything else.
"""

import importlib
import json
import os
import sys
import threading
import time
import traceback

EXIT_DONE = 0
EXIT_INTERNAL = 1
EXIT_OUT_OF_MEMORY = 2
EXIT_BAD_INPUT = 3
# How often the runner checks that the worker that started it still runs.
PARENT_WATCH_SECONDS = 2.0
GIB = 1024 ** 3


def take_stdout_for_events():
    """Keep the real stdout for events and send everything printed to stderr."""
    events = os.fdopen(os.dup(1), "w", buffering=1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return events


def die_with_parent():
    """End this process when the worker that started it ends, however it ends."""
    parent = os.getppid()
    try:
        import ctypes
        import signal

        PR_SET_PDEATHSIG = 1
        ctypes.CDLL("libc.so.6", use_errno=True).prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
    except Exception as error:  # not Linux, or no libc: the watch below still holds
        print(f"[runner] no parent-death signal ({error}); watching the parent", file=sys.stderr)

    def watch():
        while True:
            time.sleep(PARENT_WATCH_SECONDS)
            if os.getppid() != parent:
                os._exit(EXIT_INTERNAL)

    threading.Thread(target=watch, daemon=True, name="parent-watch").start()


def cap_device_memory(torch, budget_gib):
    """Cap this process's CUDA allocator at the budget, so it never takes another model's memory."""
    total = torch.cuda.get_device_properties(0).total_memory
    fraction = min(1.0, budget_gib * GIB / total)
    torch.cuda.set_per_process_memory_fraction(fraction, 0)
    print(f"[runner] device memory capped at {budget_gib} GiB ({fraction:.3f} of the card)",
          file=sys.stderr)


def main(argv):
    events = take_stdout_for_events()

    def emit(event, **fields):
        events.write(json.dumps({"event": event, **fields}) + "\n")

    die_with_parent()
    started = time.monotonic()
    with open(argv[1], encoding="utf-8") as file:
        request = json.load(file)

    from pipelines.common import BadInput

    try:
        import torch
    except Exception as error:
        emit("failed", **{"class": "internal", "message": f"torch does not import: {error}"})
        return EXIT_INTERNAL

    try:
        if not torch.cuda.is_available():
            emit("failed", **{"class": "internal", "message": "CUDA is not available to torch"})
            return EXIT_INTERNAL
        cap_device_memory(torch, request["budgetGib"])
        pipeline = importlib.import_module(f"pipelines.{request['pipeline']}")
        emit("stage", name="prepare")
        pipeline.prepare(request)
        triangles = pipeline.run(request, lambda name: emit("stage", name=name))
        size = os.path.getsize(request["output"])
        emit(
            "done",
            triangles=int(triangles),
            bytes=size,
            vramPeakGib=round(torch.cuda.max_memory_allocated() / GIB, 2),
            seconds=round(time.monotonic() - started, 1),
        )
        return EXIT_DONE
    except BadInput as error:
        emit("failed", **{"class": "bad_input", "message": str(error)})
        return EXIT_BAD_INPUT
    except torch.cuda.OutOfMemoryError as error:
        traceback.print_exc()
        first_line = str(error).splitlines()[0] if str(error) else "CUDA out of memory"
        emit("failed", **{"class": "out_of_memory", "message": first_line})
        return EXIT_OUT_OF_MEMORY
    except Exception as error:
        traceback.print_exc()
        emit("failed", **{"class": "internal", "message": f"{type(error).__name__}: {error}"})
        return EXIT_INTERNAL


if __name__ == "__main__":
    sys.exit(main(sys.argv))
