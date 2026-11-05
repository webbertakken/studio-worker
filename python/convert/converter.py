"""The source model and its conversion (docs/engines/convert.md, "Conversion").

`tf2onnx` converts a TFLite file at an opset as it is: an int8 model stays int8. When the TFLite
file takes or gives quantised tensors (uint8 or int8), the ONNX graph gets a `QuantizeLinear` in
front and a `DequantizeLinear` behind, with the TFLite file's own scale and zero point, so it
takes and gives float32 like the model card's interface. TFLite does the same arithmetic when a
caller quantises its input, so the scores do not change; nothing is re-quantised. The batch
dimension is fixed at 1.
"""

import numpy as np

FLOAT = "float32"
# onnxruntime spells element types as ONNX does; the report spells them as numpy does.
ORT_TYPES = {"float": "float32", "double": "float64", "float16": "float16"}


class ConvertError(Exception):
    pass


def parse_source(source):
    """`hf:<repo>@<revision>` to (repo, revision)."""
    if not source.startswith("hf:") or "@" not in source:
        raise ConvertError(f"source {source!r} is not hf:<repo>@<revision>")
    repo, revision = source[3:].rsplit("@", 1)
    if not repo or len(revision) != 40 or any(c not in "0123456789abcdef" for c in revision):
        raise ConvertError(f"source {source!r}: the revision must be a full commit sha")
    return repo, revision


def fetch(source, names):
    """Each of `names` from the source repository at its revision, through `HF_HOME`."""
    from huggingface_hub import hf_hub_download

    repo, revision = parse_source(source)
    return {name: hf_hub_download(repo, name, revision=revision) for name in names}


class TfliteModel:
    """The source model in LiteRT: float32 in, float32 scores out, (de)quantised as TFLite's
    own quantisation parameters say."""

    def __init__(self, path):
        from ai_edge_litert.interpreter import Interpreter

        self.interpreter = Interpreter(model_path=path, num_threads=1)
        self.interpreter.allocate_tensors()
        self.input = self.interpreter.get_input_details()[0]
        self.output = self.interpreter.get_output_details()[0]

    def quantisation(self, detail):
        scale, zero_point = detail["quantization"]
        return (float(scale), int(zero_point)) if detail["dtype"] != np.float32 else None

    def __call__(self, x):
        detail = self.input
        q = self.quantisation(detail)
        if q:
            info = np.iinfo(detail["dtype"])
            x = np.clip(np.round(x / np.float32(q[0])) + q[1], info.min, info.max).astype(detail["dtype"])
        self.interpreter.set_tensor(detail["index"], x)
        self.interpreter.invoke()
        y = self.interpreter.get_tensor(self.output["index"])
        q = self.quantisation(self.output)
        if q:
            y = (y.astype(np.float32) - q[1]) * np.float32(q[0])
        return y


def convert(tflite_path, opset, output_path):
    """Convert, wrap quantised I/O in float32, fix the batch at 1, save. Returns the wrapping."""
    import onnx
    import tf2onnx

    source = TfliteModel(tflite_path)
    model, _ = tf2onnx.convert.from_tflite(tflite_path, opset=opset)
    wrapped = wrap_io(model, source.quantisation(source.input), source.quantisation(source.output))
    onnx.checker.check_model(model)
    onnx.save(model, output_path)
    return wrapped


def wrap_io(model, input_quantisation, output_quantisation):
    """Float32 in and out around a quantised graph; batch 1."""
    from onnx import TensorProto, helper, numpy_helper

    graph = model.graph
    if len(graph.input) != 1 or len(graph.output) != 1:
        raise ConvertError(f"expected one input and one output, found {len(graph.input)} and {len(graph.output)}")
    wrapped = {"input": None, "output": None}
    graph_input, graph_output = graph.input[0], graph.output[0]
    if input_quantisation:
        scale, zero_point = input_quantisation
        name, inner = graph_input.name, graph_input.name + "_quantised"
        elem = graph_input.type.tensor_type.elem_type
        for node in graph.node:
            node.input[:] = [inner if i == name else i for i in node.input]
        graph.initializer.extend([
            numpy_helper.from_array(np.array(scale, dtype=np.float32), name + "_scale"),
            numpy_helper.from_array(np.array(zero_point, dtype=helper.tensor_dtype_to_np_dtype(elem)), name + "_zero_point"),
        ])
        graph.node.insert(0, helper.make_node("QuantizeLinear", [name, name + "_scale", name + "_zero_point"], [inner],
                                              name=name + "_quantise"))
        graph_input.type.tensor_type.elem_type = TensorProto.FLOAT
        wrapped["input"] = {"scale": scale, "zeroPoint": zero_point}
    if output_quantisation:
        scale, zero_point = output_quantisation
        name, inner = graph_output.name, graph_output.name + "_quantised"
        elem = graph_output.type.tensor_type.elem_type
        for node in graph.node:
            node.output[:] = [inner if o == name else o for o in node.output]
        graph.initializer.extend([
            numpy_helper.from_array(np.array(scale, dtype=np.float32), name + "_scale"),
            numpy_helper.from_array(np.array(zero_point, dtype=helper.tensor_dtype_to_np_dtype(elem)), name + "_zero_point"),
        ])
        graph.node.append(helper.make_node("DequantizeLinear", [inner, name + "_scale", name + "_zero_point"], [name],
                                           name=name + "_dequantise"))
        graph_output.type.tensor_type.elem_type = TensorProto.FLOAT
        wrapped["output"] = {"scale": scale, "zeroPoint": zero_point}
    for value in (graph_input, graph_output):
        dims = value.type.tensor_type.shape.dim
        if dims:
            dims[0].ClearField("dim_param")
            dims[0].dim_value = 1
    return wrapped


class OnnxModel:
    def __init__(self, path):
        import onnxruntime as ort

        options = ort.SessionOptions()
        options.intra_op_num_threads = 1
        self.session = ort.InferenceSession(path, options, providers=["CPUExecutionProvider"])
        self.input = self.session.get_inputs()[0]
        self.output = self.session.get_outputs()[0]

    def __call__(self, x):
        return self.session.run([self.output.name], {self.input.name: x})[0]

    def interface(self):
        """The report's `input` and `output`."""
        def of(value):
            kind = value.type
            kind = kind[len("tensor("):-1] if kind.startswith("tensor(") else kind
            return {"name": value.name, "shape": list(value.shape), "type": ORT_TYPES.get(kind, kind)}
        return of(self.input), of(self.output)


def verify(input_spec, output_spec, classes):
    """The converted model takes (1, 28, 28, 1) float32 and gives (1, classes) float32."""
    want_in, want_out = [1, 28, 28, 1], [1, classes]
    problems = []
    if input_spec["shape"] != want_in or input_spec["type"] != FLOAT:
        problems.append(f"input {input_spec['shape']} {input_spec['type']}, expected {want_in} {FLOAT}")
    if output_spec["shape"] != want_out or output_spec["type"] != FLOAT:
        problems.append(f"output {output_spec['shape']} {output_spec['type']}, expected {want_out} {FLOAT}")
    if problems:
        raise ConvertError("; ".join(problems))
