//! The daemon's side of a llama process, against fake processes: `sh` scripts that speak the
//! protocol, one of which aborts itself the way ggml does on a CUDA out-of-memory.

use super::*;
use crate::types::{ChatMessage, LlmParams};
use std::path::Path;

const OOM_LOG: &str = r#"{"type":"log","level":"ERROR","target":"llama-cpp-2","message":"CUDA error: out of memory"}"#;

fn loaded_line() -> String {
    format!(
        r#"{{"type":"loaded","version":"{}"}}"#,
        env!("CARGO_PKG_VERSION")
    )
}

/// A fake llama process: `/bin/sh` running `body` after it read the `load` line and answered
/// `loaded`.  `$dir` is the script's directory.
fn fake(dir: &Path, body: &str) -> ChildProgram {
    fake_raw(
        dir,
        &format!("read -r load\nprintf '%s\\n' '{}'\n{body}", loaded_line()),
    )
}

fn fake_raw(dir: &Path, script: &str) -> ChildProgram {
    let path = dir.join(format!("fake-{}.sh", rand_suffix()));
    std::fs::write(&path, format!("dir='{}'\n{script}\n", dir.display())).unwrap();
    ChildProgram::new("/bin/sh", vec![path.into_os_string()])
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn spec() -> LoadSpec {
    LoadSpec {
        model: "qwen3.5-2b".into(),
        model_path: "/m/q.gguf".into(),
        projector_path: None,
        defaults: Default::default(),
    }
}

fn params() -> LlmParams {
    LlmParams {
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }],
        max_tokens: 8,
        ..Default::default()
    }
}

fn quick() -> Timing {
    Timing {
        cancel_grace: Duration::from_millis(300),
        stop_grace: Duration::from_millis(300),
    }
}

fn start(program: &ChildProgram) -> LlamaProcess {
    LlamaProcess::start_with(program, &spec(), quick()).expect("the fake loads")
}

fn chat(process: &mut LlamaProcess) -> Result<serde_json::Value, LlamaProcessError> {
    process.chat(params(), &|| false, &mut |_| {})
}

#[test]
fn a_chat_streams_its_pieces_and_returns_the_completion() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"while read -r line; do
  printf '%s\n' '{"type":"piece","text":"Hel"}' '{"type":"piece","text":"lo"}'
  printf '%s\n' '{"type":"done","completion":{"object":"chat.completion"}}'
done"#,
    );
    let mut process = start(&program);
    let mut pieces = String::new();
    let completion = process
        .chat(params(), &|| false, &mut |p| pieces.push_str(p))
        .unwrap();
    assert_eq!(pieces, "Hello");
    assert_eq!(completion["object"], "chat.completion");
    // The same process serves the next request.
    assert!(chat(&mut process).is_ok());
}

#[test]
fn an_abort_mid_request_fails_the_request_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        &format!(
            r#"read -r chat
printf '%s\n' '{OOM_LOG}'
echo 'ggml-cuda.cu:108: CUDA error' >&2
kill -ABRT $$"#
        ),
    );
    let mut process = start(&program);
    let err = chat(&mut process).unwrap_err();
    assert!(matches!(err, LlamaProcessError::Ended { .. }), "{err:?}");
    let message = err.to_string();
    assert!(message.starts_with("out of GPU memory: "), "{message}");
    assert!(message.contains("signal 6, SIGABRT"), "{message}");
    assert!(message.contains("CUDA error: out of memory"), "{message}");
    assert!(
        message.contains("ggml-cuda.cu:108: CUDA error"),
        "{message}"
    );
    assert!(message.contains("the daemon is unaffected"), "{message}");
    assert!(process.has_ended());
    // A process that ended serves nothing more, and says so.
    assert!(matches!(
        chat(&mut process).unwrap_err(),
        LlamaProcessError::Ended { .. }
    ));
}

#[test]
fn an_abort_is_logged_as_process_died() {
    let logs = crate::test_support::capture(|| {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "read -r chat\nkill -ABRT $$");
        let mut process = start(&program);
        let _ = chat(&mut process);
    });
    assert!(logs.contains("studio_worker::llama_process"), "{logs}");
    assert!(logs.contains("op=\"process_died\""), "{logs}");
    assert!(logs.contains("ERROR"), "{logs}");
}

#[test]
fn a_handled_failure_carries_llamas_error_lines_and_the_process_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"read -r chat
printf '%s\n' '{"type":"log","level":"ERROR","target":"llama-cpp-2","message":"cudaMalloc failed: out of memory"}'
printf '%s\n' '{"type":"log","level":"INFO","target":"llama-cpp-2","message":"reserving"}'
printf '%s\n' '{"type":"failed","error":"creating llama context"}'
read -r chat
printf '%s\n' '{"type":"done","completion":{}}'
read -r rest"#,
    );
    let mut process = start(&program);
    let err = chat(&mut process).unwrap_err();
    assert_eq!(
        err.to_string(),
        "out of GPU memory: creating llama context (llama.cpp: cudaMalloc failed: out of memory)"
    );
    assert!(!process.has_ended());
    assert!(chat(&mut process).is_ok(), "the next request is served");
}

#[test]
fn a_failure_without_llama_lines_is_the_error_alone() {
    let err = LlamaProcessError::Failed {
        error: "tokenize prompt".into(),
        llama: vec![],
        permanent: false,
    };
    assert_eq!(err.to_string(), "tokenize prompt");
}

#[test]
fn tokenize_answers_the_token_ids() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"read -r tokenize
case "$tokenize" in *'"addSpecial":true'*) printf '%s\n' '{"type":"tokens","tokens":[1,2,3]}' ;; esac
read -r rest"#,
    );
    let mut process = start(&program);
    assert_eq!(process.tokenize("hi", true).unwrap(), vec![1, 2, 3]);
}

#[test]
fn a_load_failure_is_the_start_error() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake_raw(
        dir.path(),
        r#"read -r load
printf '%s\n' '{"type":"failed","error":"loading model qwen3.5-2b from /m/q.gguf"}'
exit 1"#,
    );
    let err = LlamaProcess::start_with(&program, &spec(), quick())
        .err()
        .expect("refused");
    assert!(
        err.to_string().contains("loading model qwen3.5-2b"),
        "{err}"
    );
}

#[test]
fn a_process_of_another_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake_raw(
        dir.path(),
        r#"read -r load
printf '%s\n' '{"type":"loaded","version":"0.0.0-other"}'
read -r rest"#,
    );
    let err = LlamaProcess::start_with(&program, &spec(), quick())
        .err()
        .expect("refused");
    assert!(matches!(err, LlamaProcessError::Version { .. }), "{err:?}");
    assert!(err.to_string().contains("0.0.0-other"), "{err}");
}

#[test]
fn a_program_that_cannot_start_is_a_spawn_error() {
    let program = ChildProgram::new("/nonexistent/studio-worker", vec![]);
    let err = LlamaProcess::start_with(&program, &spec(), quick())
        .err()
        .expect("refused");
    assert!(matches!(err, LlamaProcessError::Spawn(_)), "{err:?}");
    assert!(
        err.to_string().contains("starting the llama process"),
        "{err}"
    );
}

#[test]
fn a_cancelled_request_sends_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"read -r chat
read -r cancel
case "$cancel" in *cancel*) printf '%s\n' '{"type":"failed","error":"cancelled"}' ;; esac
read -r rest"#,
    );
    let mut process = start(&program);
    let err = process.chat(params(), &|| true, &mut |_| {}).unwrap_err();
    assert_eq!(err.to_string(), "cancelled");
}

#[test]
fn a_process_that_ignores_cancel_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(dir.path(), "read -r chat\nwhile true; do sleep 1; done");
    let mut process = start(&program);
    let started = Instant::now();
    let err = process.chat(params(), &|| true, &mut |_| {}).unwrap_err();
    assert!(matches!(err, LlamaProcessError::Unresponsive), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(process.has_ended());
}

#[test]
fn dropping_closes_stdin_and_the_process_exits() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"while read -r line; do :; done
touch "$dir/exited""#,
    );
    let process = start(&program);
    drop(process);
    assert!(
        dir.path().join("exited").exists(),
        "the process saw EOF and exited"
    );
}

#[test]
fn dropping_kills_a_process_that_does_not_exit() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(dir.path(), "while true; do sleep 1; done");
    let process = start(&program);
    let started = Instant::now();
    let logs = crate::test_support::capture(move || drop(process));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(logs.contains("op=\"stop\""), "{logs}");
    assert!(logs.contains("killed"), "{logs}");
}

#[test]
fn stray_output_and_process_logs_reach_the_callers_log() {
    let logs = crate::test_support::capture(|| {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_raw(
            dir.path(),
            &format!(
                r#"read -r load
echo 'printf from C'
printf '%s\n' '{{"type":"log","level":"INFO","target":"llama-cpp-2","message":"offloaded 25/25 layers to GPU"}}'
printf '%s\n' '{}'
read -r rest"#,
                loaded_line()
            ),
        );
        let _process = start(&program);
    });
    assert!(logs.contains("printf from C"), "{logs}");
    assert!(logs.contains("op=\"output\""), "{logs}");
    assert!(logs.contains("offloaded 25/25 layers to GPU"), "{logs}");
    assert!(logs.contains("llama-cpp-2"), "{logs}");
    assert!(logs.contains("op=\"start\""), "{logs}");
}

#[test]
fn reasons_keep_the_first_and_the_last_lines_shortened() {
    let mut reasons = Reasons::default();
    for i in 0..10 {
        reasons.push(&format!("line {i}"));
    }
    reasons.push(&"x".repeat(1000));
    reasons.push(&"x".repeat(1000));
    reasons.push("  ");
    let lines = reasons.take();
    assert_eq!(lines.len(), REASON_LINES);
    assert_eq!(
        &lines[..5],
        ["line 0", "line 1", "line 7", "line 8", "line 9"]
    );
    assert_eq!(
        lines[5].chars().count(),
        REASON_LINE_CHARS + 1,
        "{}",
        lines[5]
    );
    assert!(lines[5].ends_with('…'));
    assert!(reasons.take().is_empty(), "taken");
}

/// A model load that runs out of memory logs the cause first, then what failed because of it.
#[test]
fn the_cause_stays_in_the_error_however_many_lines_follow() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"read -r chat
printf '%s\n' '{"type":"log","level":"ERROR","target":"llama-cpp-2","message":"cudaMalloc failed: out of memory","fields":" module=\"ggml::alloc\""}'
for i in 1 2 3 4 5 6 7 8; do
  printf '{"type":"log","level":"ERROR","target":"llama-cpp-2","message":"then %s"}\n' "$i"
done
printf '%s\n' '{"type":"failed","error":"null result from llama cpp"}'
read -r rest"#,
    );
    let mut process = start(&program);
    let message = chat(&mut process).unwrap_err().to_string();
    assert_eq!(
        message,
        "out of GPU memory: null result from llama cpp \
         (llama.cpp: cudaMalloc failed: out of memory; then 1; then 5; then 6; then 7; then 8)"
    );
}

#[cfg(unix)]
#[test]
fn an_exit_names_its_signal_or_code() {
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(describe_exit(ExitStatus::from_raw(6)), "signal 6, SIGABRT");
    assert_eq!(describe_exit(ExitStatus::from_raw(9)), "signal 9, SIGKILL");
    assert_eq!(
        describe_exit(ExitStatus::from_raw(11)),
        "signal 11, SIGSEGV"
    );
    assert_eq!(describe_exit(ExitStatus::from_raw(31)), "signal 31");
    assert_eq!(describe_exit(ExitStatus::from_raw(3 << 8)), "exit code 3");
}

#[test]
fn the_daemons_own_executable_serves_with_backtraces_off() {
    let program = ChildProgram::this_executable().unwrap();
    assert_eq!(program.args, vec![std::ffi::OsString::from(SERVE_COMMAND)]);
    #[cfg(target_os = "linux")]
    assert_eq!(program.program, std::path::PathBuf::from("/proc/self/exe"));
    assert!(program
        .env
        .iter()
        .any(|(k, v)| k == "GGML_NO_BACKTRACE" && v == "1"));
}

// -- IsolatedLlm: a loaded model over its llama process ---------------------------------------

/// Dies on the first chat (leaving a marker), serves every later one.
fn dies_once(dir: &Path) -> ChildProgram {
    fake(
        dir,
        r#"while read -r line; do
  if [ ! -e "$dir/died" ]; then touch "$dir/died"; kill -ABRT $$; fi
  printf '%s\n' '{"type":"done","completion":{"object":"chat.completion"}}'
done"#,
    )
}

#[test]
fn a_loaded_model_restarts_its_process_after_it_ended() {
    use crate::host::ChatModel as _;
    let logs = crate::test_support::capture(|| {
        let dir = tempfile::tempdir().unwrap();
        let model = IsolatedLlm::start(dies_once(dir.path()), spec()).unwrap();
        let first = model.chat(params(), &|| false, &mut |_| {}).unwrap_err();
        assert!(first.to_string().contains("SIGABRT"), "{first}");
        let second = model.chat(params(), &|| false, &mut |_| {}).unwrap();
        assert_eq!(second["object"], "chat.completion");
    });
    assert!(logs.contains("op=\"restart\""), "{logs}");
}

#[test]
fn a_loaded_model_tokenizes_through_its_process() {
    use crate::host::{ChatModel as _, LoadedModel as _};
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"while read -r line; do printf '%s\n' '{"type":"tokens","tokens":[7]}'; done"#,
    );
    let model = IsolatedLlm::start(program, spec()).unwrap();
    assert_eq!(model.tokenize("x", false).unwrap(), vec![7]);
    assert!(model.as_chat().is_some());
    assert!(model.as_any().downcast_ref::<IsolatedLlm>().is_some());
}

#[test]
fn a_restart_that_fails_fails_the_request() {
    use crate::host::ChatModel as _;
    let dir = tempfile::tempdir().unwrap();
    // Loads once; every later start finds the marker and fails its load.
    let program = fake_raw(
        dir.path(),
        &format!(
            r#"read -r load
if [ -e "$dir/started" ]; then printf '%s\n' '{{"type":"failed","error":"out of memory loading"}}'; exit 1; fi
touch "$dir/started"
printf '%s\n' '{}'
read -r chat
kill -ABRT $$"#,
            loaded_line()
        ),
    );
    let model = IsolatedLlm::start(program, spec()).unwrap();
    assert!(model.chat(params(), &|| false, &mut |_| {}).is_err());
    let err = model.chat(params(), &|| false, &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("out of memory loading"), "{err}");
}

#[test]
fn a_permanent_failure_stays_permanent_across_the_process() {
    let permanent: anyhow::Error = LlamaProcessError::Failed {
        error: "the task carries 99 images".into(),
        llama: vec![],
        permanent: true,
    }
    .into();
    assert!(crate::runtime::is_permanent_failure(&permanent));
    let ended: anyhow::Error = LlamaProcessError::Ended {
        exit: "signal 6, SIGABRT".into(),
        llama: vec!["CUDA error: out of memory".into()],
    }
    .into();
    assert!(!crate::runtime::is_permanent_failure(&ended));
}
