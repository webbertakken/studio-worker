//! WebSocket worker-channel module.
//!
//! `types` mirrors the TypeScript wire-format types from
//! `apps/studio/src/shared/types/workerWs.ts`.  `client` wraps the
//! `tokio-tungstenite` connection and exposes a session API to the
//! runtime.
pub mod client;
pub mod session;
pub mod types;
