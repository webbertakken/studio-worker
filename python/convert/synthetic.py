"""The synthetic `model_convert` result's model (docs/engines/convert.md, "Synthetic").

    python -m convert.synthetic <out.onnx>

A tiny ONNX model with the converted model's interface, (1, 28, 28, 1) float32 in and (1, 345)
float32 out: the input's mean plus a fixed ramp over the classes. The worker embeds the file this
writes (`src/engine/convert/synthetic.onnx`); a test holds the two equal.
"""

import sys

import numpy as np

CLASSES = 345
OPSET = 13


def model():
    from onnx import TensorProto, helper, numpy_helper

    ramp = (np.arange(CLASSES, dtype=np.float32) / CLASSES).reshape(1, CLASSES)
    graph = helper.make_graph(
        [
            helper.make_node("Reshape", ["image", "flat_shape"], ["flat"], name="flatten"),
            helper.make_node("ReduceMean", ["flat"], ["mean"], name="mean", axes=[1], keepdims=1),
            helper.make_node("Add", ["mean", "ramp"], ["scores"], name="scores"),
        ],
        "synthetic_doodle_classifier",
        [helper.make_tensor_value_info("image", TensorProto.FLOAT, [1, 28, 28, 1])],
        [helper.make_tensor_value_info("scores", TensorProto.FLOAT, [1, CLASSES])],
        [
            numpy_helper.from_array(np.array([1, 28 * 28], dtype=np.int64), "flat_shape"),
            numpy_helper.from_array(ramp, "ramp"),
        ],
    )
    made = helper.make_model(graph, producer_name="studio-worker", opset_imports=[helper.make_opsetid("", OPSET)])
    made.ir_version = 7
    return made


def main(argv):
    import onnx

    made = model()
    onnx.checker.check_model(made)
    with open(argv[1], "wb") as file:
        file.write(made.SerializeToString())
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
