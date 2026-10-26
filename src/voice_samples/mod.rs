//! The operator's own recordings, for wake word training (`docs/engines/wake-word.md`, "The
//! operator's voice"): what a take is ([`take`]), where takes live ([`store`]), and how they arrive
//! over the LAN listener ([`upload`]).

pub mod store;
pub mod take;
pub mod upload;

pub use store::{Held, SetSummary, StoreError, TakeFile, VoiceSamples};
pub use take::{Kind, Take};

/// Tracing target for recordings.  Stable so operators can filter with
/// `RUST_LOG=studio_worker::voice_samples=debug`.
pub const TRACE_TARGET: &str = "studio_worker::voice_samples";
/// The path recording grants name; takes are `PUT` beneath it.
pub const PATH: &str = "/voice-samples";
/// A recording token lives this long unless the request says otherwise.  Safe range 30 s to 1 h.
pub const DEFAULT_TOKEN_TTL_SECS: i64 = 600;
