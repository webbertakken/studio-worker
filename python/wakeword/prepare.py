"""Stage `prepare`: turn the downloaded datasets into what training reads, once per download.

Every prepared dataset lives under the datasets root in its own directory, built in `<dir>.partial`
and renamed when complete; `<dir>.ready` names the sha256 of the download it was built from, so a
dataset is rebuilt only when its download changes.
"""

import io
import os
import shutil
import zipfile

import numpy as np

from wakeword.common import log, read_json, write_json

# Bumped when the way a dataset is prepared changes, so every dataset is prepared once more.
PREPARE_FORMAT = 1
SAMPLE_RATE = 16000


def _ready(target, key):
    marker = f"{target}.ready"
    if not os.path.isdir(target) or not os.path.isfile(marker):
        return False
    try:
        return read_json(marker) == {"format": PREPARE_FORMAT, "key": key}
    except (OSError, ValueError):
        return False


def _build(target, key, build):
    """Build `target` with `build(partial_dir)` unless it is ready for `key`."""
    if _ready(target, key):
        log(f"{os.path.basename(target)}: ready")
        return False
    partial = f"{target}.partial"
    for stale in (partial, target):
        if os.path.exists(stale):
            shutil.rmtree(stale)
    os.makedirs(partial)
    log(f"{os.path.basename(target)}: preparing")
    build(partial)
    os.replace(partial, target)
    write_json(f"{target}.ready", {"format": PREPARE_FORMAT, "key": key})
    return True


def _extract_features(archive):
    def build(into):
        with zipfile.ZipFile(archive) as zip_file:
            for member in zip_file.infolist():
                parts = member.filename.split("/")
                if parts[0] == "__MACOSX" or parts[-1] == ".DS_Store" or member.is_dir():
                    continue
                # The archive holds `<name>/<split>/<set>_mmap/...`: drop the top folder.
                relative = os.path.join(*parts[1:]) if len(parts) > 1 else parts[0]
                destination = os.path.normpath(os.path.join(into, relative))
                if not destination.startswith(os.path.abspath(into) + os.sep):
                    raise ValueError(f"{archive}: {member.filename} escapes the dataset")
                os.makedirs(os.path.dirname(destination), exist_ok=True)
                with zip_file.open(member) as source, open(destination, "wb") as target:
                    shutil.copyfileobj(source, target, 1024 * 1024)

    return build


def to_mono_16k(data, rate):
    """Mono float audio at 16 kHz."""
    from scipy.signal import resample_poly

    if data.ndim > 1:
        data = data.mean(axis=1)
    if rate != SAMPLE_RATE:
        divisor = np.gcd(int(rate), SAMPLE_RATE)
        data = resample_poly(data, SAMPLE_RATE // divisor, int(rate) // divisor)
    return data.astype(np.float32)


def _write_wav(path, data):
    import soundfile

    soundfile.write(path, np.clip(data, -1.0, 1.0), SAMPLE_RATE, subtype="PCM_16")


def _parquet_audio(path):
    """Every (name, bytes) of a Hugging Face audio parquet, one row group at a time."""
    import pyarrow.parquet as pq

    parquet = pq.ParquetFile(path)
    for group in range(parquet.num_row_groups):
        column = parquet.read_row_group(group, columns=["audio"]).column("audio")
        for row in column:
            audio = row.as_py()
            yield os.path.basename(audio["path"] or ""), audio["bytes"]


def _decode(name, payload):
    import soundfile

    data, rate = soundfile.read(io.BytesIO(payload), always_2d=False)
    return to_mono_16k(np.asarray(data, dtype=np.float32), rate)


def _audio_to_wavs(source, kind):
    def build(into):
        if kind == "parquet":
            items = _parquet_audio(source)
        else:
            def zipped():
                with zipfile.ZipFile(source) as zip_file:
                    for member in zip_file.namelist():
                        if member.lower().endswith((".mp3", ".wav", ".flac", ".ogg")):
                            yield os.path.basename(member), zip_file.read(member)

            items = zipped()
        written = skipped = 0
        for index, (name, payload) in enumerate(items):
            try:
                audio = _decode(name, payload)
            except Exception as error:  # a damaged clip is skipped, and said so
                skipped += 1
                log(f"skipped {name or index}: {error}")
                continue
            if audio.size == 0:
                skipped += 1
                continue
            _write_wav(os.path.join(into, f"{index:06d}.wav"), audio)
            written += 1
        log(f"{os.path.basename(into)}: {written} clips written, {skipped} skipped")
        if written == 0:
            raise ValueError(f"{source}: no audio could be decoded")

    return build


def _speech_features(source, track_seconds):
    """Spectrogram features of a speech parquet, the clips joined into tracks of `track_seconds`.

    Tracks long enough to stream through make the false-accept count of an ambient set match how a
    device listens; training reads random windows of them all the same.
    """

    def build(into):
        from mmap_ninja.ragged import RaggedMmap
        from microwakeword.audio.audio_utils import generate_features_for_clip

        name = os.path.basename(into).split(".")[0]
        out = os.path.join(into, f"{name.replace('-', '_')}_mmap")
        track_samples = int(track_seconds * SAMPLE_RATE)

        def tracks():
            pending = []
            pending_samples = 0
            for clip_name, payload in _parquet_audio(source):
                pending.append(_decode(clip_name, payload))
                pending_samples += pending[-1].size
                if pending_samples >= track_samples:
                    yield generate_features_for_clip(np.concatenate(pending))
                    pending, pending_samples = [], 0
            if pending:
                yield generate_features_for_clip(np.concatenate(pending))

        RaggedMmap.from_generator(out_dir=out, sample_generator=tracks(), batch_size=16, verbose=False)

    return build


def run(request, events):
    datasets = request["datasets"]
    root = datasets["root"]
    os.makedirs(root, exist_ok=True)
    built = []

    items = []
    for feature_set in datasets["negativeFeatures"]:
        items.append((os.path.join(root, "negative-features", feature_set["name"]), feature_set["sha256"],
                      _extract_features(feature_set["path"])))
    for audio in datasets["backgroundAudio"] + [datasets["impulseResponses"]]:
        items.append((os.path.join(root, f"{audio['name']}-16k"), audio["sha256"],
                      _audio_to_wavs(audio["path"], audio["kind"])))
    for speech in datasets["speech"]:
        # A group's sets go where their split says: training, validation_ambient or testing_ambient.
        target = os.path.join(root, f"{speech['group']}-features", speech["split"], speech["name"])
        items.append((target, speech["sha256"], _speech_features(speech["path"], speech["trackSeconds"])))

    for index, (target, key, build) in enumerate(items):
        if _build(target, key, build):
            built.append(os.path.relpath(target, root))
        events.progress("prepare", index + 1, len(items))
    return {"built": built}
