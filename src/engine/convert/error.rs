//! Why a conversion failed, and whether any worker could do better.

use std::time::Duration;

use crate::engine::torch::TorchError;

/// A `model_convert` job that failed, naming its step.  [`ConvertError::is_permanent`] says
/// whether the studio may retry it.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// The task asks for something this engine does not do.
    #[error("the conversion task cannot be run: {0}")]
    BadTask(String),
    /// Building the environment failed.
    #[error("{0}")]
    Provision(#[from] TorchError),
    /// A step of the conversion failed; `tail` holds the last lines the converter printed.
    #[error("the conversion failed at `{step}`: {message}{}", tail_block(tail))]
    Step {
        step: String,
        message: String,
        /// The converter called the input bad (an unusable source), not itself.
        bad_input: bool,
        tail: String,
    },
    /// The conversion passed its time limit and was stopped.
    #[error("the conversion passed {} minutes and was stopped", .0.as_secs() / 60)]
    Timeout(Duration),
    /// The converter said it was done, but what it wrote does not hold together.
    #[error("the conversion's output is invalid: {0}")]
    InvalidOutput(String),
}

fn tail_block(tail: &str) -> String {
    if tail.is_empty() {
        String::new()
    } else {
        format!("\n--- last output ---\n{tail}")
    }
}

impl ConvertError {
    /// Whether the same job fails the same way on any worker: a task the engine refuses, or a
    /// source the converter calls unusable.  Network, provisioning, timeouts and broken output
    /// may be the machine's, so those are retried.
    pub fn is_permanent(&self) -> bool {
        match self {
            ConvertError::BadTask(_) => true,
            ConvertError::Step { bad_input, .. } => *bad_input,
            ConvertError::Provision(e) => e.is_permanent(),
            ConvertError::Timeout(_) | ConvertError::InvalidOutput(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refused_task_or_an_unusable_source_is_permanent() {
        assert!(ConvertError::BadTask("x".into()).is_permanent());
        let step = |bad_input| ConvertError::Step {
            step: "fetch".into(),
            message: "m".into(),
            bad_input,
            tail: String::new(),
        };
        assert!(step(true).is_permanent());
        assert!(!step(false).is_permanent());
        assert!(!ConvertError::Timeout(Duration::from_secs(60)).is_permanent());
        assert!(!ConvertError::InvalidOutput("x".into()).is_permanent());
        assert!(!ConvertError::Provision(TorchError::Provision {
            step: "uv".into(),
            message: "offline".into(),
            tail: String::new(),
        })
        .is_permanent());
    }

    #[test]
    fn messages_name_the_step_and_the_last_output() {
        let error = ConvertError::Step {
            step: "gate".into(),
            message: "sampling: HTTP 503".into(),
            bad_input: false,
            tail: "[convert] fetch retry 5/5".into(),
        };
        let text = error.to_string();
        assert!(text.contains("at `gate`: sampling: HTTP 503"), "{text}");
        assert!(
            text.contains("--- last output ---\n[convert] fetch retry 5/5"),
            "{text}"
        );
    }
}
