//! Every pinned input of a conversion (`docs/engines/convert.md`, "Environment").  Latest on
//! 2026-11-05 unless a pin says why not.

use crate::engine::torch::recipe::Recipe;

/// The `convert` environment: CPU only, nothing built, every package a wheel.
pub const ENVIRONMENT: Recipe = Recipe {
    name: "convert",
    // onnxruntime 1.31 needs 3.11 and numpy 2.5 needs 3.12; TensorFlow 2.21 runs 3.10 to 3.13.
    // The 3.12 the `wakeword` environment uses, so `uv` installs one interpreter for both.
    python: "3.12.15",
    cuda: None,
    torch: &[],
    torch_index: "",
    packages: &[
        // TFLite parsing in tf2onnx and the TFLite reference interpreter it converts against.
        "tensorflow-cpu==2.21.0",
        "tf2onnx==1.17.0",
        "onnx==1.23.2",
        "onnxruntime==1.31.1",
        // Scores the source model: TensorFlow's own `tf.lite.Interpreter` is deprecated.
        "ai-edge-litert==2.3.0",
        // The recipe's cairo, through the system's libcairo (no compiler, unlike pycairo).
        "cairocffi==1.7.1",
        "numpy==2.5.4",
        "protobuf==7.36.2",
        "huggingface-hub==2.4.0",
        "requests==2.34.2",
    ],
    no_deps: &[],
    sources: &[],
    builds: &[],
    imports: &[
        "tensorflow",
        "tf2onnx",
        "onnx",
        "onnxruntime",
        "ai_edge_litert.interpreter",
        "cairocffi",
        "numpy",
        "huggingface_hub",
        "requests",
    ],
};

/// The Quick, Draw! dataset's category list, at its latest commit (2025-03-11).
pub const CATEGORIES_URL: &str = "https://raw.githubusercontent.com/googlecreativelab/quickdraw-dataset/5fe6c0a910b3732bc3db7639d3c9e7c287617f2f/categories.txt";

/// Where `quickdraw-raw` reads `<label>.ndjson` (the spec's dataset).
pub const QUICKDRAW_RAW_URL: &str = "https://storage.googleapis.com/quickdraw_dataset/full/raw/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_pins_every_package_and_builds_nothing() {
        assert!(!ENVIRONMENT.has_torch());
        assert!(ENVIRONMENT.cuda.is_none());
        assert!(ENVIRONMENT.builds.is_empty() && ENVIRONMENT.sources.is_empty());
        for package in ENVIRONMENT.packages {
            assert!(package.contains("=="), "{package} is not pinned");
        }
    }

    #[test]
    fn the_categories_are_pinned_to_a_commit() {
        assert!(CATEGORIES_URL.contains("/5fe6c0a910b3732bc3db7639d3c9e7c287617f2f/"));
    }
}
