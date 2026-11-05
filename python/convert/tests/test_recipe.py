import unittest

import numpy as np

from convert import recipe

# A fixed raw drawing: a "Z" of two strokes, `[xs, ys, ts]` each, 100 x 100 at (10, 20).
DRAWING = [[[10, 60, 110], [20, 20, 120], [0, 1, 2]], [[10, 110], [120, 120], [3, 4]]]


class SimplifyTest(unittest.TestCase):
    def test_aligns_to_zero_and_scales_the_longer_side_to_255(self):
        self.assertEqual(recipe.simplify(DRAWING), [([0, 128, 255], [0, 0, 255]), ([0, 255], [255, 255])])

    def test_scales_uniformly(self):
        wide = [[[0, 200, 200], [0, 0, 50], [0, 1, 2]]]
        self.assertEqual(recipe.simplify(wide), [([0, 255, 255], [0, 0, 64])])

    def test_keeps_a_drawing_of_one_point(self):
        self.assertEqual(recipe.simplify([[[5], [7], [0]]]), [([0], [0])])
        self.assertEqual(recipe.simplify([]), [])

    def test_rdp_drops_points_within_epsilon_and_keeps_corners(self):
        line = [(0.0, 0.0), (5.0, 1.9), (10.0, 0.0)]
        corner = [(0.0, 0.0), (5.0, 2.1), (10.0, 0.0)]
        self.assertEqual(recipe.rdp(line, 2.0), [(0.0, 0.0), (10.0, 0.0)])
        self.assertEqual(recipe.rdp(corner, 2.0), corner)

    def test_resamples_every_pixel_along_the_stroke(self):
        points = recipe.resample([(0.0, 0.0), (2.5, 0.0), (2.5, 1.5)], 1.0)
        self.assertEqual(points, [(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (2.5, 0.5), (2.5, 1.5)])


class RasteriseTest(unittest.TestCase):
    def test_draws_ink_one_on_zero_in_steps_of_a_255th(self):
        raster = recipe.raster_of(DRAWING)
        self.assertEqual(raster.shape, (28, 28))
        self.assertEqual(raster.dtype, np.float32)
        self.assertEqual(float(raster.min()), 0.0)
        self.assertEqual(float(raster.max()), 1.0)
        levels = raster * 255
        self.assertTrue(np.allclose(levels, np.round(levels), atol=1e-4))

    def test_matches_the_golden_raster_of_the_fixed_drawing(self):
        raster = recipe.raster_of(DRAWING)
        # Measured with cairo 1.18; antialiasing may move a few levels between cairo versions.
        self.assertAlmostEqual(float(raster.sum()), 92.1255, delta=0.5)
        self.assertEqual(float(raster[0].sum()), 0.0)  # the padding keeps the edges clear
        self.assertEqual(float(raster[:, 0].sum()), 0.0)

    def test_centres_the_drawing_on_its_bounding_box(self):
        square = [[[0, 100, 100, 0, 0], [0, 0, 100, 100, 0], [0, 1, 2, 3, 4]]]
        raster = recipe.raster_of(square)
        ys, xs = np.nonzero(raster)
        weights = raster[ys, xs]
        self.assertAlmostEqual(float((xs * weights).sum() / weights.sum()), 13.5, delta=0.1)
        self.assertAlmostEqual(float((ys * weights).sum() / weights.sum()), 13.5, delta=0.1)
        self.assertTrue(np.allclose(raster, raster[:, ::-1], atol=1 / 255))

    def test_draws_a_blank_raster_for_no_strokes(self):
        self.assertEqual(float(recipe.rasterise([]).sum()), 0.0)


if __name__ == "__main__":
    unittest.main()
