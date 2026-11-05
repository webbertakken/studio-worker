"""The gate set's deterministic randomness (docs/engines/convert.md, "Sampling").

SplitMix64, keyed per purpose: a stream starts at `seed XOR fnv1a64(utf-8 key)` (mod 2^64), and
each draw advances it by the golden gamma and mixes the state. A uniform integer below `n` is
`(draw * n) >> 64`. The same seed and key give the same draws in any language with 64-bit integers.
"""

MASK = (1 << 64) - 1
GOLDEN_GAMMA = 0x9E3779B97F4A7C15
FNV_OFFSET = 0xCBF29CE484222325
FNV_PRIME = 0x100000001B3


def fnv1a64(data: bytes) -> int:
    value = FNV_OFFSET
    for byte in data:
        value ^= byte
        value = (value * FNV_PRIME) & MASK
    return value


class SplitMix64:
    def __init__(self, state: int):
        self.state = state & MASK

    @classmethod
    def keyed(cls, seed: int, key: str) -> "SplitMix64":
        return cls(seed ^ fnv1a64(key.encode("utf-8")))

    def next(self) -> int:
        self.state = (self.state + GOLDEN_GAMMA) & MASK
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        return z ^ (z >> 31)

    def below(self, n: int) -> int:
        """A uniform integer in [0, n)."""
        if n <= 0:
            raise ValueError(f"no integer below {n}")
        return (self.next() * n) >> 64
