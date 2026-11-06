"""The gate set: `drawingsPerWord` raw drawings of every label, drawn by seed from the dataset
(docs/engines/convert.md, "Sampling").

For each label, a stream `SplitMix64.keyed(seed, label)` draws byte offsets uniform over the
label's raw ndjson file (its size from a HEAD request); a draw takes the first whole line that
starts at or after its offset (wrapping to the file's first line past the last one), read by HTTP
range. A drawing already drawn for the label is drawn again, so a label's drawings are distinct.
The draws of a smaller gate set are the first draws of a larger one, so a label's cached draws
serve every `drawingsPerWord` up to their number; a larger one draws the label again.
"""

import json
import os
import random
import sys
import threading
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

from convert.prng import SplitMix64

# The first range read; a longer drawing reads on in chunks of this size.
CHUNK_BYTES = 16 * 1024
ATTEMPTS = 6
BACKOFF_SECONDS = 0.5
TIMEOUT_SECONDS = 30
USER_AGENT = "studio-worker-convert"
# A label whose drawings keep repeating has fewer drawings than asked for.
REDRAWS_PER_DRAWING = 20


class SampleError(Exception):
    pass


def label_url(base_url, label):
    return base_url.rstrip("/") + "/" + urllib.parse.quote(label) + ".ndjson"


class HttpStatus(Exception):
    def __init__(self, status):
        super().__init__(f"HTTP {status}")
        self.status = status


_local = threading.local()


def session():
    """This thread's keep-alive session: a TLS handshake per connection, not per read."""
    import requests

    if not hasattr(_local, "session"):
        _local.session = requests.Session()
        _local.session.headers["User-Agent"] = USER_AGENT
    return _local.session


def with_retries(what, call):
    """`call()`, retried with exponential backoff on network errors, 429 and 5xx."""
    import requests

    for attempt in range(1, ATTEMPTS + 1):
        try:
            return call()
        except HttpStatus as error:
            if not (error.status == 429 or error.status >= 500) or attempt == ATTEMPTS:
                raise SampleError(f"{what}: {error}") from error
            reason = str(error)
        except (requests.RequestException, OSError) as error:
            if attempt == ATTEMPTS:
                raise SampleError(f"{what}: {error}") from error
            reason = f"{type(error).__name__}: {error}"
        pause = BACKOFF_SECONDS * (2 ** (attempt - 1)) * (1 + random.random() / 2)
        print(f"[convert] fetch retry {attempt}/{ATTEMPTS - 1} in {pause:.1f}s: {what}: {reason}",
              file=sys.stderr)
        time.sleep(pause)
    raise AssertionError("unreachable")


def get_text(url):
    def get():
        response = session().get(url, timeout=TIMEOUT_SECONDS)
        if response.status_code != 200:
            raise HttpStatus(response.status_code)
        return response.content.decode("utf-8")

    return with_retries(f"GET {url}", get)


def file_size(url):
    def head():
        response = session().head(url, timeout=TIMEOUT_SECONDS, allow_redirects=True)
        if response.status_code != 200:
            raise HttpStatus(response.status_code)
        length = response.headers.get("Content-Length")
        if length is None:
            raise SampleError(f"HEAD {url}: no Content-Length")
        return int(length)

    return with_retries(f"HEAD {url}", head)


def read_range(url, start, length):
    """`length` bytes from `start` (fewer at the end of the file)."""
    def get():
        response = session().get(url, timeout=TIMEOUT_SECONDS,
                                 headers={"Range": f"bytes={start}-{start + length - 1}"})
        if response.status_code != 206:
            raise HttpStatus(response.status_code)
        return response.content

    return with_retries(f"GET {url} bytes={start}-{start + length - 1}", get)


class Window:
    """Bytes of a file from `start`, read on in chunks as a search needs them."""

    def __init__(self, read, size, start):
        self.read, self.size, self.start = read, size, start
        self.data = b""

    def find_newline(self, at):
        """The file position of the first newline at or after `at`, or `None` before the end."""
        while True:
            index = self.data.find(b"\n", at - self.start)
            if index >= 0:
                return self.start + index
            end = self.start + len(self.data)
            if end >= self.size:
                return None
            chunk = self.read(end, CHUNK_BYTES)
            if not chunk:
                return None
            self.data += chunk

    def slice(self, begin, end):
        return self.data[begin - self.start:end - self.start]


def line_from(read, size, offset):
    """The first whole line starting at or after `offset`, through `read(start, length)`."""
    if offset > 0:
        window = Window(read, size, offset - 1)  # a newline just before the offset starts a line at it
        newline = window.find_newline(offset - 1)
        if newline is not None and newline + 1 < size:
            line_start = newline + 1
            end = window.find_newline(line_start)
            return window.slice(line_start, size if end is None else end)
    window = Window(read, size, 0)  # past the last line: wrap to the first
    end = window.find_newline(0)
    return window.slice(0, size if end is None else end)


def parse_line(url, offset, line):
    try:
        return json.loads(line)
    except ValueError as error:
        raise SampleError(f"{url} at byte {offset}: not a JSON line ({error})") from error


def choose(label, url, stream, size, count, fetched, read):
    """`count` distinct drawings of `label` in draw order. `fetched` holds the drawings of the
    first `count` offsets (callables, in order); a redraw reads its line at once through `read`."""
    drawings, seen, draw, redraws = [], set(), 0, 0
    while len(drawings) < count:
        if draw < len(fetched):
            drawing = fetched[draw]()
        else:
            offset = stream.below(size)
            drawing = parse_line(url, offset, line_from(read, size, offset))
        draw += 1
        key = drawing.get("key_id")
        if key in seen:
            redraws += 1
            if redraws > REDRAWS_PER_DRAWING * count:
                raise SampleError(f"{label}: too few distinct drawings for {count}")
            continue
        if drawing.get("word") != label:
            raise SampleError(f"{url}: a drawing of {drawing.get('word')!r}, not {label!r}")
        seen.add(key)
        drawings.append(drawing)
    return drawings


def read_cache(cache, count):
    if cache and os.path.exists(cache):
        with open(cache, encoding="utf-8") as file:
            cached = [json.loads(line) for line in file if line.strip()]
        if len(cached) >= count:
            return cached[:count]
    return None


def write_cache(cache, drawings):
    os.makedirs(os.path.dirname(cache), exist_ok=True)
    partial = cache + ".partial"
    with open(partial, "w", encoding="utf-8") as file:
        for drawing in drawings:
            file.write(json.dumps(drawing, separators=(",", ":")) + "\n")
    os.replace(partial, cache)


def draw_label(base_url, label, seed, count, read, size):
    """`count` distinct drawings of `label`, read through `read(start, length)` (tests)."""
    url = label_url(base_url, label)
    stream = SplitMix64.keyed(seed, label)
    offsets = [stream.below(size) for _ in range(count)]
    fetched = [lambda o=o: parse_line(url, o, line_from(read, size, o)) for o in offsets]
    return choose(label, url, stream, size, count, fetched, read)


def draw_gate_set(base_url, labels, seed, count, concurrency, cache_dir=None, progress=None):
    """Every label's drawings, by label. Every read runs on one pool of `concurrency` threads:
    a range read deep in a large file takes 0.2 to 0.8 s, so the reads run side by side."""
    caches = {label: os.path.join(cache_dir, f"{label}.ndjson") if cache_dir else None for label in labels}
    gate = {label: read_cache(caches[label], count) for label in labels}
    missing = [label for label in labels if gate[label] is None]
    total, done = len(missing) * count, 0
    with ThreadPoolExecutor(max_workers=max(1, concurrency)) as pool:
        sizes = {label: pool.submit(file_size, label_url(base_url, label)) for label in missing}
        plans = {}
        for label in missing:
            url = label_url(base_url, label)
            size = sizes[label].result()
            stream = SplitMix64.keyed(seed, label)
            futures = []
            for _ in range(count):
                offset = stream.below(size)
                futures.append(pool.submit(
                    lambda o=offset, u=url, s=size: parse_line(u, o, line_from(lambda a, n: read_range(u, a, n), s, o))))
            plans[label] = (url, stream, size, futures)
        try:
            for label in missing:
                url, stream, size, futures = plans[label]
                for future in futures:
                    future.result()
                    done += 1
                    if progress:
                        progress(done, total)
                read = lambda a, n, u=url: read_range(u, a, n)  # noqa: E731
                gate[label] = choose(label, url, stream, size, count, [f.result for f in futures], read)
                if caches[label]:
                    write_cache(caches[label], gate[label])
        except BaseException:
            for _, _, _, futures in plans.values():
                for future in futures:
                    future.cancel()
            raise
    return gate
