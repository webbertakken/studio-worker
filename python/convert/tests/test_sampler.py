import json
import unittest

from convert import sampler
from convert.prng import SplitMix64


def ndjson(word, count):
    lines = [json.dumps({"word": word, "key_id": str(1000 + i), "recognized": i % 2 == 0,
                         "drawing": [[[0, i], [0, i], [0, 1]]], "pad": "x" * (i * 37 % 300)})
             for i in range(count)]
    return ("\n".join(lines) + "\n").encode()


class Reader:
    def __init__(self, data, chunk=None):
        self.data, self.reads = data, []

    def __call__(self, start, length):
        self.reads.append((start, length))
        return self.data[start:start + length]


class LineFromTest(unittest.TestCase):
    data = b"first\nsecond line\nthird\n"

    def line(self, offset):
        return sampler.line_from(Reader(self.data), len(self.data), offset)

    def test_takes_the_first_whole_line_at_or_after_the_offset(self):
        self.assertEqual(self.line(0), b"first")
        self.assertEqual(self.line(1), b"second line")
        self.assertEqual(self.line(6), b"second line")  # a line that starts at the offset is whole
        self.assertEqual(self.line(7), b"third")

    def test_wraps_to_the_first_line_past_the_last(self):
        self.assertEqual(self.line(19), b"first")
        self.assertEqual(self.line(len(self.data) - 1), b"first")

    def test_reads_on_for_a_line_longer_than_a_chunk(self):
        long_line = b"x" * (sampler.CHUNK_BYTES * 2 + 5)
        data = b"a\n" + long_line + b"\nb\n"
        reader = Reader(data)
        self.assertEqual(sampler.line_from(reader, len(data), 1), long_line)
        self.assertGreaterEqual(len(reader.reads), 3)

    def test_url_encodes_labels(self):
        self.assertEqual(sampler.label_url("https://h/raw/", "The Eiffel Tower"),
                         "https://h/raw/The%20Eiffel%20Tower.ndjson")


class DrawLabelTest(unittest.TestCase):
    def draw(self, data, seed, count):
        return sampler.draw_label("https://h/raw", "cat", seed, count, Reader(data), len(data))

    def test_repeats_by_seed_and_differs_by_seed(self):
        data = ndjson("cat", 200)
        first = [d["key_id"] for d in self.draw(data, 3577, 20)]
        self.assertEqual(first, [d["key_id"] for d in self.draw(data, 3577, 20)])
        self.assertNotEqual(first, [d["key_id"] for d in self.draw(data, 3578, 20)])

    def test_draws_distinct_drawings(self):
        drawings = self.draw(ndjson("cat", 12), 7, 12)
        self.assertEqual(len({d["key_id"] for d in drawings}), 12)

    def test_a_smaller_set_is_the_start_of_a_larger_one(self):
        data = ndjson("cat", 500)
        small = [d["key_id"] for d in self.draw(data, 1, 10)]
        large = [d["key_id"] for d in self.draw(data, 1, 30)]
        self.assertEqual(small, large[:10])

    def test_offsets_come_from_the_labels_stream(self):
        data = ndjson("cat", 300)
        stream = SplitMix64.keyed(42, "cat")
        offset = stream.below(len(data))
        expected = json.loads(sampler.line_from(Reader(data), len(data), offset))
        self.assertEqual(self.draw(data, 42, 1)[0]["key_id"], expected["key_id"])

    def test_refuses_too_few_drawings_and_another_word(self):
        with self.assertRaises(sampler.SampleError):
            self.draw(ndjson("cat", 3), 1, 5)
        with self.assertRaises(sampler.SampleError):
            self.draw(ndjson("dog", 50), 1, 2)


class CacheTest(unittest.TestCase):
    def test_a_cached_label_is_not_read_again(self):
        import tempfile, os

        with tempfile.TemporaryDirectory() as cache:
            drawings = [{"word": "cat", "key_id": str(i)} for i in range(5)]
            sampler.write_cache(os.path.join(cache, "cat.ndjson"), drawings)
            gate = sampler.draw_gate_set("http://127.0.0.1:9/never", ["cat"], 1, 3, 2, cache)
            self.assertEqual(gate["cat"], drawings[:3])


if __name__ == "__main__":
    unittest.main()
