"""Stage `features`: augment the synthetic samples and turn them into spectrogram features.

Upstream's recipe: each clip is placed at the end of a 3.2 s window over background noise and music,
reverberated with a room's impulse response, and featurised every 10 ms.  Training and validation
features slide over 10 frames (the streaming offsets a device sees); testing features do not, since
testing streams through the quantised model itself.
"""

import os
import shutil

from wakeword.common import log, write_json

SPLITS = (
    # (features split, the clips' split, slide frames)
    ("training", "train", 10),
    ("validation", "validation", 10),
    ("testing", "test", 1),
)


def _augmenter(request):
    from microwakeword.audio.augmentation import Augmentation

    settings = request["settings"]["augmentation"]
    root = request["datasets"]["root"]
    return Augmentation(
        augmentation_duration_s=settings["durationSeconds"],
        augmentation_probabilities=settings["probabilities"],
        impulse_paths=[os.path.join(root, f"{request['datasets']['impulseResponses']['name']}-16k")],
        background_paths=[os.path.join(root, f"{audio['name']}-16k") for audio in request["datasets"]["backgroundAudio"]],
        background_min_snr_db=settings["backgroundMinSnrDb"],
        background_max_snr_db=settings["backgroundMaxSnrDb"],
        min_jitter_s=settings["minJitterSeconds"],
        max_jitter_s=settings["maxJitterSeconds"],
    )


def _featurise(source, out_root, name, repetitions, seed, augmenter, events, label):
    from mmap_ninja.ragged import RaggedMmap
    from microwakeword.audio.clips import Clips
    from microwakeword.audio.spectrograms import SpectrogramGeneration

    clips = Clips(
        input_directory=source,
        file_pattern="*.wav",
        max_clip_duration_s=None,
        remove_silence=False,
        random_split_seed=seed,
        split_count=0.1,
    )
    sizes = {}
    for split, clip_split, slide in SPLITS:
        generation = SpectrogramGeneration(clips=clips, augmenter=augmenter, slide_frames=slide, step_ms=10)
        repeat = repetitions if split == "training" else 1
        out = os.path.join(out_root, split, f"{name}_mmap")
        os.makedirs(os.path.dirname(out), exist_ok=True)
        RaggedMmap.from_generator(
            out_dir=out,
            sample_generator=generation.spectrogram_generator(split=clip_split, repeat=repeat),
            batch_size=100,
            verbose=False,
        )
        sizes[split] = len(RaggedMmap(out))
        log(f"{label} {split}: {sizes[split]} spectrograms")
    events.progress(label, 1, 1)
    return sizes


def run(request, events):
    work = request["workDir"]
    samples = os.path.join(work, "samples")
    out = os.path.join(work, "features")
    if os.path.exists(out):
        shutil.rmtree(out)
    settings = request["settings"]["features"]
    augmenter = _augmenter(request)
    seed = request["seed"]

    sizes = {"positive": {}, "nearMiss": {}}
    for language in request["languages"]:
        code = language["code"]
        sizes["positive"][code] = _featurise(
            os.path.join(samples, "positive", code),
            os.path.join(out, "positive"),
            code,
            settings["positiveRepetitions"],
            seed,
            augmenter,
            events,
            f"features positive {code}",
        )
    near_miss = os.path.join(samples, "near_miss")
    for phrase in sorted(os.listdir(near_miss)) if os.path.isdir(near_miss) else []:
        sizes["nearMiss"][phrase] = _featurise(
            os.path.join(near_miss, phrase),
            os.path.join(out, "near_miss"),
            phrase,
            settings["nearMissRepetitions"],
            seed,
            augmenter,
            events,
            f"features near miss {phrase}",
        )
    own_voice = request.get("ownVoice")
    if own_voice:
        sizes["ownVoice"] = _own_voice(request, own_voice, augmenter, out, events)
    write_json(os.path.join(work, "features.json"), sizes)
    return sizes


# The held-out takes are streamed with this much silence before them, so the stream's first
# slices (which never accept) fall before the phrase.
HELD_OUT_LEAD_SECONDS = 1.0
# The operator's talk is cut into windows this long.
SPEECH_WINDOW_SECONDS = 3.0


def _linked(work, group, paths):
    """A directory of links to `paths` (never copies: the takes live only in the worker's store)."""
    into = os.path.join(work, "own_voice", group)
    os.makedirs(into, exist_ok=True)
    for index, path in enumerate(paths):
        os.symlink(path, os.path.join(into, f"{index:04d}.wav"))
    return into


def _own_voice(request, own_voice, augmenter, out, events):
    """The operator's takes: training positives and negatives augmented, held-out takes as recorded."""
    import numpy as np
    from mmap_ninja.ragged import RaggedMmap
    from microwakeword.audio.audio_utils import generate_features_for_clip
    from microwakeword.audio.clips import Clips
    from microwakeword.audio.spectrograms import SpectrogramGeneration

    work = request["workDir"]
    repetitions = request["settings"]["features"]["ownVoiceRepetitions"]
    sizes = {}

    def write(name, generator):
        target = os.path.join(out, name)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        RaggedMmap.from_generator(out_dir=target, sample_generator=generator, batch_size=100, verbose=False)
        sizes[name] = len(RaggedMmap(target))
        log(f"own voice {name}: {sizes[name]} spectrograms")

    def augmented(group, paths):
        clips = Clips(
            input_directory=_linked(work, group, paths),
            file_pattern="*.wav",
            max_clip_duration_s=None,
            remove_silence=True,
            random_split_seed=None,
        )
        generation = SpectrogramGeneration(clips=clips, augmenter=augmenter, slide_frames=10, step_ms=10)
        return generation.spectrogram_generator(repeat=repetitions)

    if own_voice["trainPositive"]:
        write("own_positive/training/own_positive_mmap", augmented("positive", own_voice["trainPositive"]))
    if own_voice["nearMiss"]:
        write("own_near_miss/training/own_near_miss_mmap", augmented("near_miss", own_voice["nearMiss"]))
    if own_voice["speech"]:
        clips = Clips(
            input_directory=_linked(work, "speech", own_voice["speech"]),
            file_pattern="*.wav",
            max_clip_duration_s=None,
            remove_silence=False,
            random_split_seed=None,
        )
        generation = SpectrogramGeneration(
            clips=clips, augmenter=None, split_spectrogram_duration_s=SPEECH_WINDOW_SECONDS, step_ms=10
        )
        write("own_speech/training/own_speech_mmap", generation.spectrogram_generator())

    def held_out():
        import soundfile

        lead = np.zeros(int(HELD_OUT_LEAD_SECONDS * 16000), dtype=np.float32)
        for path in own_voice["heldOut"]:
            audio, rate = soundfile.read(path, dtype="float32")
            if rate != 16000:
                raise ValueError(f"{path} is {rate} Hz, not 16 kHz")
            yield generate_features_for_clip(np.concatenate([lead, audio]), 10)

    write("own_held_out/testing/own_held_out_mmap", held_out())
    events.progress("features own voice", 1, 1)
    return sizes
