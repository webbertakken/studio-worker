//! The built binary's `llama-serve`: a llama process speaks its protocol on stdout and nothing
//! else, and a model it cannot load is a clear error, not a hang (`docs/engines/llama.md`).

use studio_worker::llama_process::{ChildProgram, LlamaProcess, LlamaProcessError, LoadSpec};

fn program() -> ChildProgram {
    let mut program = ChildProgram::this_executable().expect("this executable");
    program.program = env!("CARGO_BIN_EXE_studio-worker").into();
    program
}

#[test]
fn a_model_that_cannot_load_is_a_clear_error_on_a_clean_protocol() {
    let logs = studio_worker::test_support::capture(|| {
        let spec = LoadSpec {
            model: "missing".into(),
            model_path: "/nonexistent/missing.gguf".into(),
            projector_path: None,
            defaults: Default::default(),
        };
        let err = LlamaProcess::start(&program(), &spec)
            .err()
            .expect("nothing to load");
        assert!(matches!(err, LlamaProcessError::Failed { .. }), "{err:?}");
        let message = err.to_string();
        assert!(
            message.contains("/nonexistent/missing.gguf")
                || message.contains("no in-process llama engine"),
            "{message}"
        );
    });
    // Every line the process wrote was a protocol line: its logging never reached stdout raw.
    assert!(!logs.contains("pipe=\"stdout\""), "{logs}");
    assert!(logs.contains("op=\"start\""), "{logs}");
}
