"""Stage `samples`: synthetic speech of the phrase and its near misses, in every language asked.

Speech comes from piper-sample-generator's multi-speaker generators.  Each batch mixes random pairs
of the generator's speakers at a random speed and noise from the language's prosody: upstream's CLI
walks the speakers in order, which voices thousands of samples with the same first speaker.
"""

import os
import random
import logging
import shutil
import wave

import numpy as np

from wakeword.common import BadInput, GIB, log, read_json, write_json


def _cap_device_memory(torch, budget_gib):
    if not torch.cuda.is_available():
        log("no CUDA device: generating on the CPU")
        return
    total = torch.cuda.get_device_properties(0).total_memory
    fraction = min(1.0, budget_gib * GIB / total)
    torch.cuda.set_per_process_memory_fraction(fraction, 0)
    log(f"device memory capped at {budget_gib} GiB ({fraction:.3f} of the card)")


class Generator:
    """One language's multi-speaker generator."""

    def __init__(self, torch, language):
        from piper_sample_generator import __main__ as piper

        # Importing it sets the root logger to DEBUG; the job's log wants INFO.
        logging.getLogger().setLevel(logging.INFO)

        self.piper = piper
        self.torch = torch
        self.config = read_json(language["config"])
        self.voice = self.config["espeak"]["voice"]
        self.sample_rate = self.config["audio"]["sample_rate"]
        self.speakers = min(self.config["num_speakers"], language["maxSpeakers"])
        self.prosody = language["prosody"]
        self.model = torch.load(language["generator"], weights_only=False)
        self.model.eval()
        if torch.cuda.is_available():
            self.model.cuda()

    def phonemes(self, text):
        return "".join(p for sentence in self.piper._PHONEMIZER.phonemize(self.voice, text) for p in sentence)

    def phoneme_ids(self, text):
        ids = self.piper.get_phonemes(self.voice, self.config, text)
        if len(ids) <= 4:
            raise BadInput(f"'{text}' has no phonemes the {self.voice} voice can speak")
        return ids

    def batch(self, ids, count, rng, prosody):
        torch = self.torch
        with torch.no_grad():
            first = torch.LongTensor([rng.randrange(self.speakers) for _ in range(count)])
            second = torch.LongTensor([rng.randrange(self.speakers) for _ in range(count)])
            audio, phoneme_samples = self.piper.generate_audio(
                self.model,
                first,
                second,
                [ids] * count,
                rng.choice(prosody["slerpWeights"]),
                rng.choice(prosody["noiseScales"]),
                rng.choice(prosody["noiseScaleWs"]),
                rng.choice(prosody["lengthScales"]),
                None,
            )
            for i in range(count):
                last = int(phoneme_samples[i].flatten().sum().item())
                audio[i, 0, last + 1 :] = 0
            clips = audio.cpu().numpy()
        return [np.trim_zeros(self.piper.audio_float_to_int16(clip).flatten()) for clip in clips]


def _write(path, samples, rate):
    with wave.open(path, "wb") as out:
        out.setframerate(rate)
        out.setsampwidth(2)
        out.setnchannels(1)
        out.writeframes(samples.tobytes())


def _generate(generator, texts, count, into, prefix, rng, settings, events, label):
    """`count` clips of `texts` (cycled per batch) into `into`, in the generator's prosody."""
    os.makedirs(into, exist_ok=True)
    ids = [generator.phoneme_ids(text) for text in texts]
    written = 0
    batch_size = settings["batchSize"]
    while written < count:
        size = min(batch_size, count - written)
        clips = generator.batch(ids[(written // batch_size) % len(ids)], size, rng, generator.prosody)
        for clip in clips:
            if clip.size == 0:
                continue
            _write(os.path.join(into, f"{prefix}-{written:06d}.wav"), clip, generator.sample_rate)
            written += 1
        events.progress(label, written, count)
    return written


def run(request, events):
    import torch

    settings = request["settings"]["samples"]
    work = request["workDir"]
    root = os.path.join(work, "samples")
    if os.path.exists(root):
        shutil.rmtree(root)
    _cap_device_memory(torch, request["settings"]["deviceMemoryBudgetGib"])
    rng = random.Random(request["seed"])
    torch.manual_seed(request["seed"])

    counts = {"positive": {}, "nearMiss": {}}
    phonemes = []
    negatives = request["negatives"]
    for language in request["languages"]:
        code = language["code"]
        generator = Generator(torch, language)
        log(f"{code}: {generator.speakers} speakers, voice {generator.voice}")
        for text in language["spellings"]:
            spoken = generator.phonemes(text)
            phonemes.append({"language": code, "text": text, "role": "positive", "phonemes": spoken})
            events.emit("phonemes", language=code, text=text, role="positive", phonemes=spoken)
        for near_miss in negatives:
            text = near_miss["text"]
            spoken = generator.phonemes(text)
            phonemes.append({"language": code, "text": text, "role": "near_miss", "phonemes": spoken})
            events.emit("phonemes", language=code, text=text, role="near_miss", phonemes=spoken)
        counts["positive"][code] = _generate(
            generator,
            language["spellings"],
            settings["positivePerLanguage"],
            os.path.join(root, "positive", code),
            code,
            rng,
            settings,
            events,
            f"samples positive {code}",
        )
        for near_miss in negatives:
            # The worker names each near miss's samples (its slug) and maps them back in the report.
            name = near_miss["slug"]
            counts["nearMiss"].setdefault(name, {})[code] = _generate(
                generator,
                [near_miss["text"]],
                settings["nearMissPerPhrasePerLanguage"],
                os.path.join(root, "near_miss", name),
                code,
                rng,
                settings,
                events,
                f"samples near miss {name} {code}",
            )
        del generator
        if torch.cuda.is_available():
            torch.cuda.empty_cache()

    peak = torch.cuda.max_memory_allocated() / GIB if torch.cuda.is_available() else 0.0
    summary = {"counts": counts, "phonemes": phonemes, "torchPeakGib": round(peak, 2)}
    write_json(os.path.join(work, "samples.json"), summary)
    return summary
