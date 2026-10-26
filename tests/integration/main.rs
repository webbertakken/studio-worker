//! The integration test suite, linked as one binary.
//!
//! Each `tests/*.rs` file would be its own fully linked test binary, so
//! every feature set and dependency change would relink and store all of
//! them.  One binary keeps link time and disk use to a single copy.  Add
//! new suites as modules here; `layout` fails the build of any other
//! arrangement.

mod layout;

mod auto_register_http;
mod auto_register_log_fields;
mod auto_register_orchestration;
mod auto_register_save_tracing;
mod auto_update;
mod config_tracing;
mod engine_download;
mod engine_tracing;
mod host_probe_tracing;
mod http_contract;
mod http_errors;
mod llama_process;
mod local_api_vision;
mod local_api_voice_samples;
mod local_api_wake_word;
mod multi_modal;
mod real_candle_image;
mod real_llama;
mod real_torch_mesh;
mod real_vision;
mod real_whisper;
mod register_reset;
mod registration_gate;
mod runtime_helpers;
mod runtime_observers;
mod runtime_startup_tracing;
mod runtime_ticks;
mod sd_provision;
mod telemetry;
mod torch_provision;
mod vision_fetch;
mod ws_client_contract;
mod ws_session_full_loop;
mod ws_session_vision;
mod ws_wire;
