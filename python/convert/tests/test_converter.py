import os
import tempfile
import unittest

import numpy as np

from convert import converter, synthetic

HERE = os.path.dirname(os.path.abspath(__file__))
EMBEDDED_SYNTHETIC = os.path.join(HERE, "..", "..", "..", "src", "engine", "convert", "synthetic.onnx")


def quantised_identity():
    """uint8 in, uint8 out, a dynamic batch: the I/O of an int8 TFLite conversion."""
    from onnx import TensorProto, helper

    graph = helper.make_graph(
        [helper.make_node("Identity", ["image"], ["scores"])],
        "quantised",
        [helper.make_tensor_value_info("image", TensorProto.UINT8, ["batch", 4])],
        [helper.make_tensor_value_info("scores", TensorProto.UINT8, ["batch", 4])],
    )
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 13)])
    model.ir_version = 7
    return model


class SourceTest(unittest.TestCase):
    def test_parses_a_pinned_hugging_face_source(self):
        sha = "f978d83cfc41fa3c76435a4c96417e055220c735"
        self.assertEqual(converter.parse_source(f"hf:org/repo@{sha}"), ("org/repo", sha))

    def test_refuses_a_branch_or_another_hub(self):
        for source in ("hf:org/repo@main", "hf:org/repo", "s3:org/repo@" + "a" * 40):
            with self.assertRaises(converter.ConvertError):
                converter.parse_source(source)


class WrapTest(unittest.TestCase):
    def test_wraps_quantised_io_in_float32_with_the_sources_parameters(self):
        import onnx

        model = quantised_identity()
        wrapped = converter.wrap_io(model, (1 / 255, 0), (1 / 256, 0))
        onnx.checker.check_model(model)
        self.assertEqual(wrapped["output"], {"scale": 1 / 256, "zeroPoint": 0})
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "m.onnx")
            onnx.save(model, path)
            onnx_model = converter.OnnxModel(path)
            spec_in, spec_out = onnx_model.interface()
            self.assertEqual(spec_in, {"name": "image", "shape": [1, 4], "type": "float32"})
            self.assertEqual(spec_out["type"], "float32")
            x = np.array([[0.0, 1 / 255, 128 / 255, 1.0]], np.float32)
            y = onnx_model(x)
        # quantised by 1/255, dequantised by 1/256
        self.assertTrue(np.allclose(y, np.array([[0, 1, 128, 255]]) / 256))

    def test_leaves_float_io_alone(self):
        from onnx import TensorProto

        model = quantised_identity()
        model.graph.input[0].type.tensor_type.elem_type = TensorProto.FLOAT
        model.graph.output[0].type.tensor_type.elem_type = TensorProto.FLOAT
        self.assertEqual(converter.wrap_io(model, None, None), {"input": None, "output": None})
        self.assertEqual(len(model.graph.node), 1)


class VerifyTest(unittest.TestCase):
    def test_holds_the_interface(self):
        good_in = {"name": "i", "shape": [1, 28, 28, 1], "type": "float32"}
        good_out = {"name": "o", "shape": [1, 345], "type": "float32"}
        converter.verify(good_in, good_out, 345)
        with self.assertRaises(converter.ConvertError):
            converter.verify(dict(good_in, type="uint8"), good_out, 345)
        with self.assertRaises(converter.ConvertError):
            converter.verify(good_in, good_out, 340)


class SyntheticTest(unittest.TestCase):
    def test_the_embedded_synthetic_model_is_the_generators(self):
        with open(EMBEDDED_SYNTHETIC, "rb") as file:
            self.assertEqual(file.read(), synthetic.model().SerializeToString())


if __name__ == "__main__":
    unittest.main()
