//! Wake word training: a local job that trains a microWakeWord model for a phrase
//! (`docs/engines/wake-word.md`).
//!
//! - [`request`] reads and checks `POST /wake-words`;
//! - [`recipe`] and [`settings`] pin every input;
//! - [`training`] runs the job: provision, download, the trainer's stages ([`stage`], [`trainer`]),
//!   then the worker's own choice of cutoff ([`cutoff`]), manifest ([`manifest`]) and report
//!   ([`report`]).

pub mod cutoff;
mod error;
pub mod files;
pub mod manifest;
pub mod memory;
pub mod own_voice;
pub mod recipe;
pub mod report;
pub mod request;
pub mod settings;
pub mod stage;
pub mod trainer;
pub mod training;

pub use error::TrainingError;
pub use training::{Backend, CancelRefusal, Refusal, Training, WakeWords};

/// Tracing target for wake word training.  Stable so operators can filter with
/// `RUST_LOG=studio_worker::wake_word=debug`.
pub const TRACE_TARGET: &str = "studio_worker::wake_word";

#[cfg(all(test, unix))]
mod tests;
