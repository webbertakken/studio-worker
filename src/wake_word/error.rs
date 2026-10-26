//! Why a training failed (`docs/engines/wake-word.md`, "Failures"): every failure has a stable
//! reason and a message that names what failed.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TrainingError {
    #[error("ran out of disk space during {stage}: {message}")]
    DiskFull { stage: String, message: String },
    #[error("download of {what} stalled: nothing arrived for {} s ({url})", stall.as_secs())]
    DownloadStalled {
        what: String,
        url: String,
        stall: Duration,
    },
    #[error("download of {what} failed: {message}")]
    DownloadFailed { what: String, message: String },
    #[error("ran out of memory during {stage}: {message}")]
    OutOfMemory { stage: String, message: String },
    #[error("building the Python environment failed: {0}")]
    ProvisionFailed(String),
    #[error("the {stage} stage refused the request: {message}")]
    BadInput { stage: String, message: String },
    #[error("the {stage} stage passed its {} minute limit and was stopped", limit.as_secs() / 60)]
    StageTimedOut { stage: String, limit: Duration },
    #[error("the {stage} stage failed ({status}): {message}{}", tail_block(tail))]
    StageFailed {
        stage: String,
        status: String,
        message: String,
        tail: String,
    },
    #[error(
        "the {stage} stage was killed (SIGKILL) without a word: usually the system's out-of-memory \
         killer, when host memory ran out; `journalctl -u earlyoom` and `journalctl -k` say which"
    )]
    Killed { stage: String },
    #[error("the {stage} stage did not leave what comes next: {message}")]
    InvalidOutput { stage: String, message: String },
    #[error("cancelled")]
    Cancelled,
}

fn tail_block(tail: &str) -> String {
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!("\n--- last lines ---\n{tail}")
    }
}

impl TrainingError {
    /// The stable reason the API answers (`failure.reason`).
    pub fn reason(&self) -> &'static str {
        match self {
            TrainingError::DiskFull { .. } => "disk_full",
            TrainingError::DownloadStalled { .. } => "download_stalled",
            TrainingError::DownloadFailed { .. } => "download_failed",
            TrainingError::OutOfMemory { .. } => "out_of_memory",
            TrainingError::ProvisionFailed(_) => "provision_failed",
            TrainingError::BadInput { .. } => "bad_input",
            TrainingError::StageTimedOut { .. } => "stage_timed_out",
            TrainingError::StageFailed { .. } => "stage_failed",
            TrainingError::Killed { .. } => "killed",
            TrainingError::InvalidOutput { .. } => "invalid_output",
            TrainingError::Cancelled => "cancelled",
        }
    }

    /// A failed download, named a stall when nothing arrived for its bound.
    pub fn download(what: &str, url: &str, stall: Duration, error: &anyhow::Error) -> Self {
        if crate::engine::download::is_stall(error) {
            return TrainingError::DownloadStalled {
                what: what.into(),
                url: url.into(),
                stall,
            };
        }
        let disk_full = error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull)
        });
        if disk_full {
            return TrainingError::DiskFull {
                stage: "download".into(),
                message: format!("{what}: {error:#}"),
            };
        }
        TrainingError::DownloadFailed {
            what: what.into(),
            message: format!("{error:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_has_its_reason_and_names_what_failed() {
        let cases = [
            (
                TrainingError::DiskFull {
                    stage: "features".into(),
                    message: "No space left on device: /m".into(),
                },
                "disk_full",
                "ran out of disk space during features: No space left on device: /m",
            ),
            (
                TrainingError::DownloadStalled {
                    what: "speech".into(),
                    url: "https://x".into(),
                    stall: Duration::from_secs(120),
                },
                "download_stalled",
                "download of speech stalled: nothing arrived for 120 s (https://x)",
            ),
            (
                TrainingError::OutOfMemory {
                    stage: "samples".into(),
                    message: "CUDA out of memory".into(),
                },
                "out_of_memory",
                "ran out of memory during samples: CUDA out of memory",
            ),
            (
                TrainingError::StageTimedOut {
                    stage: "train".into(),
                    limit: Duration::from_secs(8 * 3600),
                },
                "stage_timed_out",
                "the train stage passed its 480 minute limit and was stopped",
            ),
            (TrainingError::Cancelled, "cancelled", "cancelled"),
            (
                TrainingError::ProvisionFailed("packages: exit 1".into()),
                "provision_failed",
                "building the Python environment failed: packages: exit 1",
            ),
            (
                TrainingError::BadInput {
                    stage: "samples".into(),
                    message: "no phonemes".into(),
                },
                "bad_input",
                "the samples stage refused the request: no phonemes",
            ),
            (
                TrainingError::InvalidOutput {
                    stage: "train".into(),
                    message: "no model".into(),
                },
                "invalid_output",
                "the train stage did not leave what comes next: no model",
            ),
            (
                TrainingError::DownloadFailed {
                    what: "fma".into(),
                    message: "404".into(),
                },
                "download_failed",
                "download of fma failed: 404",
            ),
        ];
        for (error, reason, message) in cases {
            assert_eq!(error.reason(), reason);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn a_failed_stage_carries_its_last_lines() {
        let error = TrainingError::StageFailed {
            stage: "train".into(),
            status: "exit 1".into(),
            message: "KeyError: 'x'".into(),
            tail: "Traceback\nKeyError: 'x'".into(),
        };
        assert_eq!(error.reason(), "stage_failed");
        assert_eq!(
            error.to_string(),
            "the train stage failed (exit 1): KeyError: 'x'\n--- last lines ---\nTraceback\nKeyError: 'x'"
        );
        let bare = TrainingError::StageFailed {
            stage: "a".into(),
            status: "exit 1".into(),
            message: "m".into(),
            tail: " ".into(),
        };
        assert_eq!(bare.to_string(), "the a stage failed (exit 1): m");
    }

    #[test]
    fn a_download_error_is_named_by_its_cause() {
        let full = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull))
            .context("writing speech.zip");
        assert_eq!(
            TrainingError::download("speech", "https://x", Duration::from_secs(1), &full).reason(),
            "disk_full"
        );
        let other = anyhow::anyhow!("GET https://x -> 404 Not Found");
        let error = TrainingError::download("speech", "https://x", Duration::from_secs(1), &other);
        assert_eq!(error.reason(), "download_failed");
        assert!(error.to_string().contains("404"));
    }
}
