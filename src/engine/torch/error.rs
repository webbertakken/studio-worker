//! Why a torch job failed, and whether any worker could do better.

use std::time::Duration;

/// A torch job that failed.  [`TorchError::is_permanent`] says whether the studio may retry it.
#[derive(Debug, thiserror::Error)]
pub enum TorchError {
    /// The model's `cliDefaults` lack a field the engine needs.
    #[error("the torch engine needs `{field}` in the model's cliDefaults")]
    MissingDefault { field: &'static str },
    /// The model names a pipeline this worker does not have.
    #[error("unknown torch pipeline `{0}` (this worker has: pixal3d, trellis2, hunyuan3d)")]
    UnknownPipeline(String),
    /// A tool provisioning needs is not on the `PATH`.
    #[error("provisioning needs `{tool}` on the PATH: {remedy}")]
    MissingTool {
        tool: &'static str,
        remedy: &'static str,
    },
    /// A provisioning step failed; `tail` holds the last lines of its output.
    #[error("provisioning failed at `{step}`: {message}{}", tail_block(tail))]
    Provision {
        step: String,
        message: String,
        tail: String,
    },
    /// The picture cannot be used: not a picture, or the pipeline refuses it.
    #[error("the picture cannot be used: {0}")]
    BadInput(String),
    /// The run needed more device memory than its budget.
    #[error("out of device memory within the {budget_gib} GiB budget: {message}")]
    OutOfMemory { budget_gib: f32, message: String },
    /// The runner failed otherwise; `tail` holds the last lines it printed.
    #[error("the runner failed ({status}): {message}{}", tail_block(tail))]
    Runner {
        status: String,
        message: String,
        tail: String,
    },
    /// The run passed its time limit and was stopped.
    #[error("the run passed {} minutes and was stopped", .0.as_secs() / 60)]
    Timeout(Duration),
    /// The runner said it was done, but its output is not a GLB.
    #[error("the runner's output is not a GLB: {0}")]
    InvalidGlb(String),
}

fn tail_block(tail: &str) -> String {
    if tail.is_empty() {
        String::new()
    } else {
        format!("\n--- last output ---\n{tail}")
    }
}

impl TorchError {
    /// Whether the same job would fail the same way on any worker: a model configured wrongly, a
    /// picture that cannot be used, or a run that does not fit its budget.  Provisioning,
    /// crashes, timeouts and broken output may be the machine's, so those are retried.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            TorchError::MissingDefault { .. }
                | TorchError::UnknownPipeline(_)
                | TorchError::BadInput(_)
                | TorchError::OutOfMemory { .. }
        )
    }

    pub(super) fn provision(step: impl Into<String>, message: impl std::fmt::Display) -> Self {
        TorchError::Provision {
            step: step.into(),
            message: message.to_string(),
            tail: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_input_and_budget_failures_are_permanent() {
        assert!(TorchError::MissingDefault { field: "pipeline" }.is_permanent());
        assert!(TorchError::UnknownPipeline("x".into()).is_permanent());
        assert!(TorchError::BadInput("x".into()).is_permanent());
        assert!(TorchError::OutOfMemory {
            budget_gib: 16.0,
            message: "x".into()
        }
        .is_permanent());
    }

    #[test]
    fn machine_failures_are_retried() {
        assert!(!TorchError::provision("uv", "offline").is_permanent());
        assert!(!TorchError::Timeout(Duration::from_secs(60)).is_permanent());
        assert!(!TorchError::InvalidGlb("x".into()).is_permanent());
        assert!(!TorchError::MissingTool {
            tool: "git",
            remedy: "install git"
        }
        .is_permanent());
        assert!(!TorchError::Runner {
            status: "exit 1".into(),
            message: "x".into(),
            tail: String::new()
        }
        .is_permanent());
    }

    #[test]
    fn the_message_carries_the_tail_when_there_is_one() {
        let with = TorchError::Provision {
            step: "natten".into(),
            message: "exit 1".into(),
            tail: "nvcc fatal".into(),
        };
        assert_eq!(
            with.to_string(),
            "provisioning failed at `natten`: exit 1\n--- last output ---\nnvcc fatal"
        );
        assert_eq!(
            TorchError::provision("uv", "offline").to_string(),
            "provisioning failed at `uv`: offline"
        );
        assert_eq!(
            TorchError::Timeout(Duration::from_secs(1800)).to_string(),
            "the run passed 30 minutes and was stopped"
        );
    }
}
