#!/usr/bin/env bash
# The embedded Python programs' unit tests (docs/engines/convert.md, "Tests"), in a throwaway uv
# environment with the pins they need; locally and in CI.  Needs `uv` and the system's libcairo.
set -euo pipefail
cd "$(dirname "$0")/.."
uv run --no-project --python 3.12 \
  --with numpy==2.5.4 \
  --with cairocffi==1.7.1 \
  --with onnx==1.23.2 \
  --with onnxruntime==1.31.1 \
  --with protobuf==7.36.2 \
  -- python -m unittest discover -s python/convert/tests -t python -v
