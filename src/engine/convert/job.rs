//! A `model_convert` task, checked, and the request the converter reads (`docs/engines/convert.md`,
//! "The task").

use std::path::PathBuf;

use serde::Serialize;

use super::error::ConvertError;
use crate::types::ModelConvertParams;

/// What the engine converts with.
pub const CONVERTER: &str = "tf2onnx";
/// How a raw drawing becomes the model input.
pub const RECIPE: &str = "quickdraw-issue-19";
/// Where the gate set comes from.
pub const DATASET: &str = "quickdraw-raw";
pub const POLARITIES: [&str; 2] = ["inkOnDark", "darkOnLight"];
/// tf2onnx 1.17 writes opsets up to 18; an int8 model needs per-axis `QuantizeLinear` (13).
pub const OPSETS: std::ops::RangeInclusive<u32> = 13..=18;
/// At most this many drawings of each label: 1000 labels' worth is days of range reads.
pub const MAX_DRAWINGS_PER_WORD: u32 = 1000;
/// The studio's `ModelConversionLimits`: at most 100 references.
pub const MAX_REFERENCES: u32 = 100;

/// Refuse a task the engine cannot run as asked, naming the field.
pub fn check(task: &ModelConvertParams) -> Result<(), ConvertError> {
    let refuse = |message: String| Err(ConvertError::BadTask(message));
    check_source(&task.source)?;
    for (field, name) in [("file", &task.file), ("labels", &task.labels)] {
        if name.is_empty() || name.starts_with('/') || name.split('/').any(|part| part == "..") {
            return refuse(format!(
                "`{field}` must be a file in the source repository, not {name:?}"
            ));
        }
    }
    if !task.file.ends_with(".tflite") {
        return refuse(format!(
            "`file` {:?} is not a TFLite model (.tflite)",
            task.file
        ));
    }
    if task.converter != CONVERTER {
        return refuse(format!(
            "converter {:?}: this worker converts with {CONVERTER}",
            task.converter
        ));
    }
    if !OPSETS.contains(&task.opset) {
        return refuse(format!(
            "opset {} is outside {}..={}",
            task.opset,
            OPSETS.start(),
            OPSETS.end()
        ));
    }
    if task.recipe != RECIPE {
        return refuse(format!(
            "recipe {:?}: this worker knows {RECIPE}",
            task.recipe
        ));
    }
    if task.gate_set.dataset != DATASET {
        return refuse(format!(
            "gate set dataset {:?}: this worker reads {DATASET}",
            task.gate_set.dataset
        ));
    }
    if !(1..=MAX_DRAWINGS_PER_WORD).contains(&task.gate_set.drawings_per_word) {
        return refuse(format!(
            "drawingsPerWord {} is outside 1..={MAX_DRAWINGS_PER_WORD}",
            task.gate_set.drawings_per_word
        ));
    }
    if u32::try_from(task.gate_set.seed).is_err() {
        return refuse(format!("seed {} is not a u32", task.gate_set.seed));
    }
    if task.reference_count > MAX_REFERENCES {
        return refuse(format!(
            "referenceCount {} is above {MAX_REFERENCES}",
            task.reference_count
        ));
    }
    if task.polarities.is_empty() {
        return refuse("no polarity to score".into());
    }
    for (i, polarity) in task.polarities.iter().enumerate() {
        if !POLARITIES.contains(&polarity.as_str()) {
            return refuse(format!(
                "polarity {polarity:?} is neither inkOnDark nor darkOnLight"
            ));
        }
        if task.polarities[..i].contains(polarity) {
            return refuse(format!("polarity {polarity:?} is asked for twice"));
        }
    }
    Ok(())
}

/// `hf:<repo>@<revision>`, the revision a full commit sha.
fn check_source(source: &str) -> Result<(), ConvertError> {
    let refuse = || {
        Err(ConvertError::BadTask(format!(
            "source {source:?} is not hf:<owner>/<repo>@<40-hex commit sha>"
        )))
    };
    let Some(rest) = source.strip_prefix("hf:") else {
        return refuse();
    };
    let Some((repo, revision)) = rest.rsplit_once('@') else {
        return refuse();
    };
    let repo_ok = repo.split('/').count() == 2 && repo.split('/').all(|part| !part.is_empty());
    let revision_ok = revision.len() == 40
        && revision
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if repo_ok && revision_ok {
        Ok(())
    } else {
        refuse()
    }
}

/// `request.json`: what `python -m convert` reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub task: ModelConvertParams,
    pub categories_url: String,
    pub dataset_url: String,
    /// Where the converter writes the `.onnx`.
    pub output: PathBuf,
    /// Where it writes the conversion report.
    pub report: PathBuf,
    /// Drawn gate sets, by dataset and seed: the same seed draws the same drawings.
    pub gate_cache: PathBuf,
    /// Range reads in flight at once.
    pub concurrency: usize,
    /// Threads scoring the gate set.
    pub threads: usize,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::GateSetParams;

    pub(crate) fn task() -> ModelConvertParams {
        ModelConvertParams {
            source: "hf:zarqankhn/quickdraw-345-tflite@f978d83cfc41fa3c76435a4c96417e055220c735"
                .into(),
            file: "quickdraw_model_int8.tflite".into(),
            labels: "labels.txt".into(),
            converter: "tf2onnx".into(),
            opset: 13,
            gate_set: GateSetParams {
                dataset: "quickdraw-raw".into(),
                drawings_per_word: 100,
                seed: 3577,
            },
            recipe: "quickdraw-issue-19".into(),
            polarities: vec!["inkOnDark".into(), "darkOnLight".into()],
            reference_count: 20,
        }
    }

    fn refusal(change: impl FnOnce(&mut ModelConvertParams)) -> String {
        let mut task = task();
        change(&mut task);
        match check(&task) {
            Err(ConvertError::BadTask(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn accepts_the_specs_task() {
        check(&task()).unwrap();
    }

    #[test]
    fn refuses_a_source_that_is_not_pinned() {
        for source in [
            "hf:org/repo@main",
            "hf:org/repo",
            "hf:repo@f978d83cfc41fa3c76435a4c96417e055220c735",
            "gh:org/repo@f978d83cfc41fa3c76435a4c96417e055220c735",
            "hf:org/repo@F978D83CFC41FA3C76435A4C96417E055220C735",
        ] {
            assert!(
                refusal(|t| t.source = source.into()).contains("hf:<owner>/<repo>@"),
                "{source}"
            );
        }
    }

    #[test]
    fn refuses_files_outside_the_repository_and_other_formats() {
        assert!(refusal(|t| t.file = "../x.tflite".into()).contains("`file`"));
        assert!(refusal(|t| t.labels = "/etc/passwd".into()).contains("`labels`"));
        assert!(refusal(|t| t.file = "model.onnx".into()).contains("not a TFLite"));
    }

    #[test]
    fn refuses_what_the_engine_does_not_know() {
        assert!(refusal(|t| t.converter = "onnxmltools".into()).contains("tf2onnx"));
        assert!(refusal(|t| t.opset = 12).contains("opset 12"));
        assert!(refusal(|t| t.opset = 19).contains("opset 19"));
        assert!(refusal(|t| t.recipe = "bigger".into()).contains("quickdraw-issue-19"));
        assert!(refusal(|t| t.gate_set.dataset = "qd".into()).contains("quickdraw-raw"));
        assert!(refusal(|t| t.gate_set.drawings_per_word = 0).contains("drawingsPerWord"));
        assert!(refusal(|t| t.reference_count = 101).contains("referenceCount"));
        assert!(refusal(|t| t.gate_set.seed = 1 << 32).contains("seed"));
        assert!(refusal(|t| t.polarities.clear()).contains("no polarity"));
        assert!(refusal(|t| t.polarities = vec!["inverted".into()]).contains("inverted"));
        assert!(refusal(|t| t.polarities = vec!["inkOnDark".into(); 2]).contains("twice"));
    }

    #[test]
    fn the_request_reads_as_the_converter_expects() {
        let request = RunRequest {
            task: task(),
            categories_url: "https://c".into(),
            dataset_url: "https://d/".into(),
            output: "/w/model.onnx".into(),
            report: "/w/report.json".into(),
            gate_cache: "/m/convert/gate-sets".into(),
            concurrency: 64,
            threads: 4,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["task"]["gateSet"]["drawingsPerWord"], 100);
        assert_eq!(json["task"]["referenceCount"], 20);
        assert_eq!(json["categoriesUrl"], "https://c");
        assert_eq!(json["gateCache"], "/m/convert/gate-sets");
        assert_eq!(json["concurrency"], 64);
        assert_eq!(json["threads"], 4);
    }
}
