//! The llama process's side (`studio-worker llama-serve`): load the model the first line names,
//! then answer one request at a time until stdin closes.  Every tracing event goes back to the
//! daemon as a `log` line; stdout carries nothing else.
//! Spec: docs/engines/llama.md, "Llama process".

use super::protocol::{encode, FromProcess, LoadSpec, ToProcess};
use crate::host::ChatModel;
use parking_lot::Mutex;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

/// The process's stdout: one protocol line per write.
#[derive(Clone)]
pub struct Output(Arc<Mutex<Box<dyn Write + Send>>>);

impl Output {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }

    pub fn send(&self, message: &FromProcess) -> std::io::Result<()> {
        let mut writer = self.0.lock();
        writer.write_all(encode(message).as_bytes())?;
        writer.flush()
    }
}

/// Whether the request in progress should stop: the daemon cancelled it, or closed stdin.
#[derive(Default)]
struct Stop {
    cancelled: AtomicBool,
    closed: AtomicBool,
}

impl Stop {
    fn requested(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.closed.load(Ordering::Relaxed)
    }
}

/// Read requests on a thread of their own, so a `cancel` reaches the request in progress.  A
/// new request clears a cancel; stdin closing stops everything.
fn read_requests(
    input: impl BufRead + Send + 'static,
    stop: Arc<Stop>,
) -> mpsc::Receiver<ToProcess> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in input.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ToProcess>(&line) {
                Ok(ToProcess::Cancel) => stop.cancelled.store(true, Ordering::Relaxed),
                Ok(request) => {
                    stop.cancelled.store(false, Ordering::Relaxed);
                    if tx.send(request).is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!(
                    target: super::TRACE_TARGET,
                    error = %e,
                    "llama process ignored a line that is not a request"
                ),
            }
        }
        stop.closed.store(true, Ordering::Relaxed);
    });
    rx
}

fn failed(error: &anyhow::Error) -> FromProcess {
    FromProcess::Failed {
        error: format!("{error:#}"),
        permanent: crate::runtime::is_permanent_failure(error),
    }
}

/// Serve requests from `input` on `out`: the first is `load`, handled by `load`.
pub fn serve(
    input: impl BufRead + Send + 'static,
    out: &Output,
    load: impl FnOnce(&LoadSpec) -> anyhow::Result<Box<dyn ChatModel>>,
) -> anyhow::Result<()> {
    let stop = Arc::new(Stop::default());
    let requests = read_requests(input, stop.clone());
    let spec = match requests.recv() {
        Ok(ToProcess::Load(spec)) => spec,
        Ok(_) => {
            let error = anyhow::anyhow!("the first request must be `load`");
            out.send(&failed(&error))?;
            return Err(error);
        }
        // Stdin closed before anything was asked.
        Err(_) => return Ok(()),
    };
    let model = match load(&spec) {
        Ok(model) => model,
        Err(error) => {
            out.send(&failed(&error))?;
            return Err(error);
        }
    };
    out.send(&FromProcess::Loaded {
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    let cancelled = || stop.requested();
    for request in requests {
        let reply = match request {
            ToProcess::Chat { params } => {
                let mut on_piece = |text: &str| {
                    let _ = out.send(&FromProcess::Piece { text: text.into() });
                };
                match model.chat(params, &cancelled, &mut on_piece) {
                    Ok(completion) => FromProcess::Done { completion },
                    Err(e) => failed(&e),
                }
            }
            ToProcess::Tokenize { text, add_special } => match model.tokenize(&text, add_special) {
                Ok(tokens) => FromProcess::Tokens { tokens },
                Err(e) => failed(&e),
            },
            ToProcess::Load(_) => failed(&anyhow::anyhow!("the model is already loaded")),
            ToProcess::Cancel => continue,
        };
        out.send(&reply)?;
    }
    Ok(())
}

/// Sends every tracing event to the daemon as a `log` line.
pub struct ProtocolLogLayer {
    out: Output,
}

impl ProtocolLogLayer {
    pub fn new(out: Output) -> Self {
        Self { out }
    }
}

/// An event's message, then its other fields as `name=value`.
#[derive(Default)]
struct Message {
    message: String,
    fields: String,
}

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for ProtocolLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut message = Message::default();
        event.record(&mut message);
        let meta = event.metadata();
        let _ = self.out.send(&FromProcess::Log {
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            message: message.message.trim_end().to_string(),
            fields: message.fields,
        });
    }
}

/// `studio-worker llama-serve`: serve on stdin and stdout.  `main` runs it before any other
/// logging starts, so stdout carries protocol lines only.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn run_stdio() -> anyhow::Result<()> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let out = Output::new(Box::new(std::io::stdout()));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(crate::DEFAULT_LOG_FILTER));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(ProtocolLogLayer::new(out.clone()))
        .try_init();
    serve(std::io::BufReader::new(std::io::stdin()), &out, load_model)
}

#[cfg(all(feature = "llama", not(target_os = "windows")))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn load_model(spec: &LoadSpec) -> anyhow::Result<Box<dyn ChatModel>> {
    Ok(Box::new(crate::engine::llama::load_local(spec)?))
}

#[cfg(not(all(feature = "llama", not(target_os = "windows"))))]
fn load_model(spec: &LoadSpec) -> anyhow::Result<Box<dyn ChatModel>> {
    anyhow::bail!(
        "this build has no in-process llama engine to load {}",
        spec.model
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LlmParams;
    use std::io::Cursor;

    /// Writes into a shared buffer the test reads back.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn replies(&self) -> Vec<FromProcess> {
            String::from_utf8(self.0.lock().clone())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    /// Answers "echo: <last message>" in two pieces; fails a prompt saying "fail"; waits for a
    /// cancel on "wait".
    struct Echo;

    impl ChatModel for Echo {
        fn chat(
            &self,
            params: LlmParams,
            cancelled: &dyn Fn() -> bool,
            on_piece: &mut dyn FnMut(&str),
        ) -> anyhow::Result<serde_json::Value> {
            let text = params
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            if text == "fail" {
                anyhow::bail!("creating llama context");
            }
            if text == "unsupported" {
                return Err(crate::vision::ImageRefusal::TooMany { count: 99 }.into());
            }
            if text == "wait" {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !cancelled() {
                    assert!(std::time::Instant::now() < deadline, "never cancelled");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                anyhow::bail!("cancelled");
            }
            on_piece("echo: ");
            on_piece(&text);
            Ok(serde_json::json!({ "content": format!("echo: {text}") }))
        }

        fn tokenize(&self, text: &str, add_special: bool) -> anyhow::Result<Vec<i32>> {
            Ok(vec![text.len() as i32, add_special as i32])
        }
    }

    fn load_line() -> String {
        encode(&ToProcess::Load(LoadSpec {
            model: "m".into(),
            model_path: "/m.gguf".into(),
            projector_path: None,
            defaults: Default::default(),
        }))
    }

    fn chat_line(text: &str) -> String {
        encode(&ToProcess::Chat {
            params: LlmParams {
                messages: vec![crate::types::ChatMessage {
                    role: "user".into(),
                    content: text.into(),
                }],
                ..Default::default()
            },
        })
    }

    fn run(input: String) -> (anyhow::Result<()>, Vec<FromProcess>) {
        let buffer = Buffer::default();
        let out = Output::new(Box::new(buffer.clone()));
        let result = serve(Cursor::new(input), &out, |_| Ok(Box::new(Echo)));
        (result, buffer.replies())
    }

    #[test]
    fn it_loads_then_answers_each_request_in_turn() {
        let input = [
            load_line(),
            chat_line("hi"),
            "not json\n".into(),
            encode(&ToProcess::Tokenize {
                text: "abc".into(),
                add_special: true,
            }),
            chat_line("fail"),
            chat_line("unsupported"),
            load_line(),
        ]
        .concat();
        let (result, replies) = run(input);
        assert!(result.is_ok());
        let version = env!("CARGO_PKG_VERSION").to_string();
        assert_eq!(replies[0], FromProcess::Loaded { version });
        assert_eq!(
            replies[1],
            FromProcess::Piece {
                text: "echo: ".into()
            }
        );
        assert_eq!(replies[2], FromProcess::Piece { text: "hi".into() });
        assert_eq!(
            replies[3],
            FromProcess::Done {
                completion: serde_json::json!({"content": "echo: hi"})
            }
        );
        assert_eq!(replies[4], FromProcess::Tokens { tokens: vec![3, 1] });
        assert_eq!(
            replies[5],
            FromProcess::Failed {
                error: "creating llama context".into(),
                permanent: false
            }
        );
        assert!(
            matches!(
                &replies[6],
                FromProcess::Failed {
                    permanent: true,
                    ..
                }
            ),
            "{:?}",
            replies[6]
        );
        assert!(
            matches!(&replies[7], FromProcess::Failed { error, .. } if error.contains("already loaded")),
            "{:?}",
            replies[7]
        );
        assert_eq!(replies.len(), 8);
    }

    #[test]
    fn a_failed_load_is_reported_and_ends_the_process() {
        let buffer = Buffer::default();
        let out = Output::new(Box::new(buffer.clone()));
        let input = Cursor::new([load_line(), chat_line("hi")].concat());
        let result = serve(input, &out, |spec| {
            anyhow::bail!(
                "loading model {} from {}",
                spec.model,
                spec.model_path.display()
            )
        });
        assert!(result.is_err());
        assert_eq!(
            buffer.replies(),
            vec![FromProcess::Failed {
                error: "loading model m from /m.gguf".into(),
                permanent: false
            }]
        );
    }

    #[test]
    fn the_first_request_must_be_load() {
        let (result, replies) = run(chat_line("hi"));
        assert!(result.is_err());
        assert!(
            matches!(&replies[0], FromProcess::Failed { error, .. } if error.contains("must be `load`"))
        );
    }

    #[test]
    fn stdin_closing_before_load_ends_quietly() {
        let (result, replies) = run(String::new());
        assert!(result.is_ok());
        assert!(replies.is_empty());
    }

    #[test]
    fn stdin_closing_stops_the_request_in_progress() {
        // The request waits for a stop; stdin ends right after it, which is the stop.
        let (result, replies) = run([load_line(), chat_line("wait")].concat());
        assert!(result.is_ok());
        assert!(
            matches!(&replies[1], FromProcess::Failed { error, .. } if error == "cancelled"),
            "{replies:?}"
        );
    }

    #[test]
    fn a_cancel_stops_the_request_in_progress_and_the_next_one_runs() {
        let (reader, writer) = std::io::pipe().unwrap();
        let buffer = Buffer::default();
        let out = Output::new(Box::new(buffer.clone()));
        let served = std::thread::spawn(move || {
            serve(std::io::BufReader::new(reader), &out, |_| {
                Ok(Box::new(Echo))
            })
        });
        let mut writer = writer;
        writer
            .write_all([load_line(), chat_line("wait")].concat().as_bytes())
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        writer
            .write_all(encode(&ToProcess::Cancel).as_bytes())
            .unwrap();
        writer.write_all(chat_line("after").as_bytes()).unwrap();
        drop(writer);
        served.join().unwrap().unwrap();
        let replies = buffer.replies();
        assert!(
            matches!(&replies[1], FromProcess::Failed { error, .. } if error == "cancelled"),
            "{replies:?}"
        );
        assert_eq!(
            replies.last(),
            Some(&FromProcess::Done {
                completion: serde_json::json!({"content": "echo: after"})
            })
        );
    }

    #[test]
    fn tracing_events_become_log_lines() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let buffer = Buffer::default();
        let layer = ProtocolLogLayer::new(Output::new(Box::new(buffer.clone())));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                target: "llama-cpp-2",
                module = "ggml::alloc",
                "cudaMalloc failed: out of memory\n"
            );
        });
        assert_eq!(
            buffer.replies(),
            vec![FromProcess::Log {
                level: "ERROR".into(),
                target: "llama-cpp-2".into(),
                message: "cudaMalloc failed: out of memory".into(),
                fields: " module=\"ggml::alloc\"".into(),
            }]
        );
    }

    #[cfg(not(all(feature = "llama", not(target_os = "windows"))))]
    #[test]
    fn a_build_without_llama_refuses_to_load() {
        let spec = LoadSpec {
            model: "m".into(),
            model_path: "/m.gguf".into(),
            projector_path: None,
            defaults: Default::default(),
        };
        let err = load_model(&spec).err().expect("refused");
        assert!(
            err.to_string().contains("no in-process llama engine"),
            "{err}"
        );
    }
}
