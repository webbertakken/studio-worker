//! A completed training's output files, as `GET /wake-words/:id/files/:file` serves them
//! (`docs/engines/wake-word.md`, "Output files"): the model, its manifest and the report, found on
//! disk by the training's id, so they outlive a daemon restart.  Nothing else is ever served.

use std::path::{Path, PathBuf};

use super::trainer::Paths;

/// A file to serve and its content type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub path: PathBuf,
    pub content_type: &'static str,
}

/// Why a file is not served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRefusal {
    /// No completed training with that id has output on disk.
    UnknownTraining,
    /// Not one of the training's output files.
    UnknownFile,
}

/// A training id as the worker mints them: `local-<millis>-<n>`.
fn is_training_id(id: &str) -> bool {
    let mut parts = id.split('-');
    parts.next() == Some("local")
        && parts.clone().count() == 2
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The output directory of training `id`: `<models>/<name>/<id>`, holding its report.
fn output_dir(paths: &Paths, id: &str) -> Option<PathBuf> {
    let models = paths.root().join("models");
    std::fs::read_dir(models)
        .ok()?
        .flatten()
        .map(|name| name.path().join(id))
        .find(|dir| dir.join("report.json").is_file())
}

/// The file `file` of training `id`, when it is one of its outputs.
pub fn output_file(paths: &Paths, id: &str, file: &str) -> Result<OutputFile, FileRefusal> {
    if !is_training_id(id) {
        return Err(FileRefusal::UnknownTraining);
    }
    let dir = output_dir(paths, id).ok_or(FileRefusal::UnknownTraining)?;
    let name = dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .ok_or(FileRefusal::UnknownTraining)?;
    let content_type = match file {
        "report.json" => "application/json",
        "report.md" => "text/markdown; charset=utf-8",
        f if f == format!("{name}.json") => "application/json",
        f if f == format!("{name}.tflite") => "application/octet-stream",
        _ => return Err(FileRefusal::UnknownFile),
    };
    let path = dir.join(file);
    if !path.is_file() {
        return Err(FileRefusal::UnknownFile);
    }
    Ok(OutputFile { path, content_type })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(paths: &Paths, name: &str, id: &str) -> PathBuf {
        let dir = paths.output(name, id);
        std::fs::create_dir_all(&dir).unwrap();
        for file in [
            format!("{name}.tflite"),
            format!("{name}.json"),
            "report.json".into(),
            "report.md".into(),
        ] {
            std::fs::write(dir.join(file), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn the_four_outputs_of_a_completed_training_are_served() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path());
        let dir = completed(&paths, "hey_runa", "local-1792954242730-1");
        let served = |file: &str| output_file(&paths, "local-1792954242730-1", file);
        assert_eq!(
            served("hey_runa.tflite").unwrap(),
            OutputFile {
                path: dir.join("hey_runa.tflite"),
                content_type: "application/octet-stream"
            }
        );
        assert_eq!(
            served("hey_runa.json").unwrap().content_type,
            "application/json"
        );
        assert_eq!(
            served("report.json").unwrap().content_type,
            "application/json"
        );
        assert_eq!(
            served("report.md").unwrap().content_type,
            "text/markdown; charset=utf-8"
        );
    }

    #[test]
    fn nothing_else_is_ever_served() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path());
        let dir = completed(&paths, "hey_runa", "local-1-1");
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        completed(&paths, "other", "local-2-2");
        let refused = |id: &str, file: &str| output_file(&paths, id, file).unwrap_err();
        for file in [
            "notes.txt",
            "other.tflite",
            "../other/local-2-2/other.tflite",
            "hey_runa.TFLITE",
            "",
            ".",
        ] {
            assert_eq!(
                refused("local-1-1", file),
                FileRefusal::UnknownFile,
                "{file}"
            );
        }
        for id in [
            "local-9-9",
            "../hey_runa",
            "local-1",
            "local-1-1-1",
            "x-1-1",
            "local-a-1",
        ] {
            assert_eq!(
                refused(id, "report.json"),
                FileRefusal::UnknownTraining,
                "{id}"
            );
        }
        std::fs::remove_file(dir.join("hey_runa.json")).unwrap();
        assert_eq!(
            refused("local-1-1", "hey_runa.json"),
            FileRefusal::UnknownFile
        );
    }

    #[test]
    fn a_training_without_a_report_has_no_files() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path());
        let dir = completed(&paths, "hey_runa", "local-3-3");
        std::fs::remove_file(dir.join("report.json")).unwrap();
        assert_eq!(
            output_file(&paths, "local-3-3", "hey_runa.tflite").unwrap_err(),
            FileRefusal::UnknownTraining
        );
        assert_eq!(
            output_file(
                &Paths::new(&root.path().join("none")),
                "local-3-3",
                "report.md"
            )
            .unwrap_err(),
            FileRefusal::UnknownTraining
        );
    }
}
