"""What every stage shares: events, failures, the parent watch, device memory sampling."""

import errno
import json
import os
import subprocess
import sys
import threading
import time

EXIT_DONE = 0
EXIT_INTERNAL = 1
EXIT_OUT_OF_MEMORY = 2
EXIT_BAD_INPUT = 3
EXIT_DISK_FULL = 4
# How often the stage checks that the worker that started it still runs.
PARENT_WATCH_SECONDS = 2.0
# How often the stage samples its own device memory through nvidia-smi.
MEMORY_SAMPLE_SECONDS = 5.0
GIB = 1024**3
MIB = 1024**2


class BadInput(Exception):
    """The request cannot be trained as asked."""


class Events:
    """The stage's events: one JSON object per line on the real stdout."""

    def __init__(self):
        self._out = os.fdopen(os.dup(1), "w", buffering=1)
        os.dup2(2, 1)
        sys.stdout = sys.stderr
        self._lock = threading.Lock()

    def emit(self, event, **fields):
        with self._lock:
            self._out.write(json.dumps({"event": event, **fields}) + "\n")

    def progress(self, stage, done, total):
        self.emit("progress", stage=stage, done=int(done), total=int(total))


def log(message):
    print(f"[wakeword] {message}", file=sys.stderr, flush=True)


def die_with_parent():
    """End this process when the worker that started it ends, however it ends."""
    parent = os.getppid()
    try:
        import ctypes
        import signal

        pr_set_pdeathsig = 1
        ctypes.CDLL("libc.so.6", use_errno=True).prctl(pr_set_pdeathsig, signal.SIGKILL)
    except Exception as error:  # not Linux, or no libc: the watch below still holds
        log(f"no parent-death signal ({error}); watching the parent")

    def watch():
        while True:
            time.sleep(PARENT_WATCH_SECONDS)
            if os.getppid() != parent:
                os._exit(EXIT_INTERNAL)

    threading.Thread(target=watch, daemon=True, name="parent-watch").start()


class DeviceMemory:
    """The most device memory this process held, sampled from nvidia-smi (context included)."""

    def __init__(self):
        self.peak_mib = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True, name="device-memory")
        self._thread.start()

    def _sample(self):
        try:
            out = subprocess.run(
                [
                    "nvidia-smi",
                    "--query-compute-apps=pid,used_memory",
                    "--format=csv,noheader,nounits",
                ],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            return
        pid = str(os.getpid())
        for line in out.splitlines():
            parts = [p.strip() for p in line.split(",")]
            if len(parts) == 2 and parts[0] == pid and parts[1].isdigit():
                self.peak_mib = max(self.peak_mib, int(parts[1]))

    def _run(self):
        while not self._stop.is_set():
            self._sample()
            self._stop.wait(MEMORY_SAMPLE_SECONDS)

    def stop(self):
        self._stop.set()
        self._sample()
        return self.peak_mib


def is_out_of_memory(error):
    """Whether `error` says the device (or the host) ran out of memory."""
    name = type(error).__name__
    text = str(error)
    return (
        name in ("OutOfMemoryError", "ResourceExhaustedError", "MemoryError")
        or "CUDA out of memory" in text
        or "OOM when allocating" in text
        or "CUDA_ERROR_OUT_OF_MEMORY" in text
    )


def is_disk_full(error):
    return isinstance(error, OSError) and error.errno in (errno.ENOSPC, errno.EDQUOT)


def first_sentences(text, count=2):
    line = text.splitlines()[0] if text else ""
    return ". ".join(line.split(". ")[:count])


def write_json(path, value):
    partial = f"{path}.partial"
    with open(partial, "w", encoding="utf-8") as file:
        json.dump(value, file, indent=2)
    os.replace(partial, path)


def read_json(path):
    with open(path, encoding="utf-8") as file:
        return json.load(file)
