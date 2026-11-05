import unittest

from convert.prng import SplitMix64, fnv1a64


class SplitMix64Test(unittest.TestCase):
    def test_matches_the_reference_sequence(self):
        # SplitMix64 from state 0 (Vigna's reference implementation).
        stream = SplitMix64(0)
        self.assertEqual(stream.next(), 0xE220A8397B1DCDAF)
        self.assertEqual(stream.next(), 0x6E789E6AA1B965F4)

    def test_fnv1a64_matches_its_reference_values(self):
        self.assertEqual(fnv1a64(b""), 0xCBF29CE484222325)
        self.assertEqual(fnv1a64(b"a"), 0xAF63DC4C8601EC8C)

    def test_keyed_streams_repeat_by_seed_and_key(self):
        first = [SplitMix64.keyed(3577, "cat").below(1000) for _ in range(1)]
        again = [SplitMix64.keyed(3577, "cat").below(1000) for _ in range(1)]
        other = SplitMix64.keyed(3577, "dog").next()
        self.assertEqual(first, again)
        self.assertNotEqual(SplitMix64.keyed(3577, "cat").next(), other)
        self.assertEqual(SplitMix64.keyed(3577, "cat").state, 3577 ^ fnv1a64(b"cat"))

    def test_below_stays_in_range(self):
        stream = SplitMix64.keyed(1, "range")
        draws = [stream.below(7) for _ in range(2000)]
        self.assertEqual(set(draws), set(range(7)))
        with self.assertRaises(ValueError):
            stream.below(0)


if __name__ == "__main__":
    unittest.main()
