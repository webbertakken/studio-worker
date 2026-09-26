//! Streaming speech-to-text: the `/transcribe` session protocol, voice
//! activity detection and the LAN listener that serves loaded models.

pub mod server;
pub mod session;
pub mod tokens;
pub mod vad;
