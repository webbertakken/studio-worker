import unittest

import numpy as np

from convert import scoring


class ScoringTest(unittest.TestCase):
    def test_polarities_map_ink(self):
        raster = np.zeros((28, 28), np.float32)
        raster[3, 4] = 1.0
        ink_on_dark = scoring.with_polarity(raster, "inkOnDark")
        dark_on_light = scoring.with_polarity(raster, "darkOnLight")
        self.assertEqual(ink_on_dark.shape, (1, 28, 28, 1))
        self.assertEqual(ink_on_dark.dtype, np.float32)
        self.assertEqual(float(ink_on_dark[0, 3, 4, 0]), 1.0)
        self.assertEqual(float(dark_on_light[0, 3, 4, 0]), 0.0)
        self.assertEqual(float(dark_on_light[0, 0, 0, 0]), 1.0)
        with self.assertRaises(ValueError):
            scoring.with_polarity(raster, "sideways")

    def test_ranks_break_ties_by_class_index(self):
        self.assertEqual(list(scoring.ranks(np.array([[0.1, 0.5, 0.5, 0.2, 0.0, 0.5]]))), [1, 2, 5, 3, 0])

    def test_scores_top1_top5_and_recognised(self):
        raster = np.zeros((28, 28), np.float32)
        samples = [(0, raster, True), (1, raster, False), (6, raster, True)]

        def model():
            return lambda x: np.array([[0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3]])

        result = scoring.score(samples, "inkOnDark", model, model, threads=2)
        self.assertEqual(result["onnx"], {"top1": 0.3333, "top5": 0.6667})
        self.assertEqual(result["source"], result["onnx"])
        self.assertEqual(result["onnxRecognised"], {"top1": 0.5, "top5": 0.5})


if __name__ == "__main__":
    unittest.main()
