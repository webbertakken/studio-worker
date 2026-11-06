//! The synthetic `model_convert` result: a tiny ONNX model and a report of the real one's shape,
//! with no network and no Python, so CI runs a conversion job end to end
//! (`docs/engines/convert.md`, "Synthetic").

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::types::{ModelConvertParams, TaskResult};

/// `python -m convert.synthetic` writes it; a Python test holds the two equal.
pub const MODEL: &[u8] = include_bytes!("synthetic.onnx");
/// The synthetic model's classes.
pub const CLASSES: usize = 345;

/// The synthetic model and a report that names it, its interface, the task's gate set (every
/// accuracy 0) and the task's references (a diagonal stroke of each synthetic label).
pub fn result(task: &ModelConvertParams) -> TaskResult {
    let labels: Vec<String> = (0..CLASSES).map(|i| format!("synthetic {i:03}")).collect();
    let per_word = u64::from(task.gate_set.drawings_per_word);
    let zero = serde_json::json!({ "top1": 0.0, "top5": 0.0 });
    let accuracy: serde_json::Map<String, serde_json::Value> = task
        .polarities
        .iter()
        .map(|polarity| {
            (
                polarity.clone(),
                serde_json::json!({ "onnx": zero, "source": zero, "onnxRecognised": zero }),
            )
        })
        .collect();
    let references: Vec<serde_json::Value> = labels
        .iter()
        .take(task.reference_count as usize)
        .enumerate()
        .map(|(i, label)| {
            serde_json::json!({
                "label": label,
                "keyId": format!("synthetic-{i}"),
                "strokes": [[[0, 255], [0, 255], [0, 100]]],
                "raster": diagonal_raster(),
            })
        })
        .collect();
    let report = serde_json::json!({
        "kind": "model_convert",
        "sha256": hex::encode(Sha256::digest(MODEL)),
        "bytes": MODEL.len(),
        "source": task.source,
        "file": task.file,
        "converter": { "name": "synthetic", "version": env!("CARGO_PKG_VERSION"), "opset": task.opset },
        "runtime": { "name": "synthetic", "version": env!("CARGO_PKG_VERSION") },
        "input": { "name": "image", "shape": [1, 28, 28, 1], "type": "float32" },
        "output": { "name": "scores", "shape": [1, CLASSES], "type": "float32" },
        "labels": labels,
        "labelsMatchCategories": true,
        "gate": {
            "dataset": task.gate_set.dataset,
            "seed": task.gate_set.seed,
            "drawingsPerWord": per_word,
            "drawings": per_word * CLASSES as u64,
            "recognised": 0,
            "recipe": task.recipe,
            "accuracy": accuracy,
        },
        "references": references,
        "durationsMs": { "fetch": 0, "convert": 0, "gate": 0 },
    });
    TaskResult::ModelConvert {
        bytes: MODEL.to_vec(),
        ext: "onnx".into(),
        report,
    }
}

/// 784 little-endian float32, ink 1.0 on the diagonal, base64.
fn diagonal_raster() -> String {
    let mut bytes = Vec::with_capacity(28 * 28 * 4);
    for row in 0..28 {
        for column in 0..28 {
            let value: f32 = if row == column { 1.0 } else { 0.0 };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_model_is_onnx_with_the_converted_interface() {
        // An ONNX ModelProto starts with its ir_version field (1, varint).
        assert_eq!(MODEL[0], 0x08);
        let text = String::from_utf8_lossy(MODEL);
        assert!(text.contains("image") && text.contains("scores"));
    }

    #[test]
    fn the_report_has_the_specs_shape() {
        let task = super::super::job::tests::task();
        let TaskResult::ModelConvert { bytes, ext, report } = result(&task) else {
            panic!("expected a model");
        };
        assert_eq!(ext, "onnx");
        assert_eq!(report["kind"], "model_convert");
        assert_eq!(report["sha256"], hex::encode(Sha256::digest(&bytes)));
        assert_eq!(report["bytes"], bytes.len());
        assert_eq!(report["source"], task.source);
        assert_eq!(report["labels"].as_array().unwrap().len(), 345);
        assert_eq!(report["gate"]["drawings"], 34_500);
        assert_eq!(report["gate"]["seed"], 3577);
        for polarity in ["inkOnDark", "darkOnLight"] {
            assert_eq!(report["gate"]["accuracy"][polarity]["onnx"]["top1"], 0.0);
        }
        let references = report["references"].as_array().unwrap();
        assert_eq!(references.len(), 20);
        let raster = base64::engine::general_purpose::STANDARD
            .decode(references[0]["raster"].as_str().unwrap())
            .unwrap();
        assert_eq!(raster.len(), 784 * 4);
        assert_eq!(f32::from_le_bytes(raster[0..4].try_into().unwrap()), 1.0);
        assert_eq!(report["durationsMs"]["gate"], 0);
    }
}
