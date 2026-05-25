//! Shared test-only helpers.
//!
//! Exposed unconditionally (marked `#[doc(hidden)]` from `lib.rs`) so
//! both library unit tests and integration tests can share a single
//! implementation.  The module is a few dozen lines and unused code
//! gets eliminated by LTO in release builds.
//!
//! `capture` installs **one** process-global `tracing-subscriber` the
//! first time it's called and routes every formatted event into a
//! thread-local buffer.  Each invocation spawns a fresh OS thread,
//! clears that thread's buffer, runs the closure, and returns its
//! contents.  Two side benefits fall out of the spawn:
//!
//! 1. `#[tokio::test]` cases that hit `reqwest::blocking` (which
//!    panics when called from inside a tokio runtime) work without
//!    extra ceremony.
//! 2. Each capture gets a brand-new thread — cargo's test runner can
//!    reuse worker threads, but our captures never share a buffer.
//!
//! The previous pattern installed a fresh `with_default` subscriber on
//! every call, which interacted badly with `tracing`'s callsite
//! Interest cache and produced empty captures under load.  See
//! `LESSONS_LEARNED.md` for the history.

use std::cell::RefCell;
use std::io;
use std::sync::OnceLock;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::Layer as _;

thread_local! {
    /// Per-thread sink that backs every formatted tracing event.
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct ThreadLocalWriter;

impl io::Write for ThreadLocalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        BUF.with(|buf| buf.borrow_mut().extend_from_slice(bytes));
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ThreadLocalMakeWriter;

impl<'a> MakeWriter<'a> for ThreadLocalMakeWriter {
    type Writer = ThreadLocalWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ThreadLocalWriter
    }
}

static GLOBAL_INSTALLED: OnceLock<()> = OnceLock::new();

fn install_once() {
    GLOBAL_INSTALLED.get_or_init(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(ThreadLocalMakeWriter)
            .with_target(true)
            .with_ansi(false)
            .without_time()
            .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
        // try_init() is a no-op if a global subscriber is already
        // installed (e.g. by another test crate's test_support helper
        // when several integration suites link the same lib).  We
        // tolerate that because the existing subscriber will also
        // route through our thread-local writer once installed.
        let _ = tracing_subscriber::registry().with(layer).try_init();
    });
}

/// Run `f` on a freshly spawned OS thread and return everything it
/// emitted via `tracing` as formatted log output.
///
/// Events emitted on threads other than the spawned capture thread
/// are not captured (they land in those threads' own thread-local
/// buffers).  All call sites in this codebase pass closures that emit
/// events synchronously on the spawned thread, so this is the correct
/// trade-off.
pub fn capture<F: FnOnce() + Send + 'static>(f: F) -> String {
    install_once();
    std::thread::spawn(move || {
        BUF.with(|b| b.borrow_mut().clear());
        f();
        BUF.with(|b| String::from_utf8(b.borrow().clone()).expect("tracing output should be UTF-8"))
    })
    .join()
    .expect("capture thread panicked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_collects_events_emitted_inside_the_closure() {
        let out = capture(|| {
            tracing::info!(target: "studio_worker::test_support_demo", marker = "alpha", "hello");
        });
        assert!(out.contains("INFO"), "missing INFO level: {out:?}");
        assert!(
            out.contains("studio_worker::test_support_demo"),
            "missing target: {out:?}"
        );
        assert!(out.contains("marker=\"alpha\""), "missing field: {out:?}");
        assert!(out.contains("hello"), "missing message: {out:?}");
    }

    #[test]
    fn capture_isolates_between_invocations() {
        // Each call spawns its own thread, so even back-to-back calls
        // on the same test cannot bleed into each other.
        let first = capture(|| tracing::info!("first message"));
        let second = capture(|| tracing::info!("second message"));
        assert!(first.contains("first message") && !first.contains("second message"));
        assert!(second.contains("second message") && !second.contains("first message"));
    }

    #[test]
    fn capture_isolates_between_threads() {
        // A sibling thread emitting events while we capture must not
        // pollute the captured buffer.
        let handle = std::thread::spawn(|| {
            for _ in 0..50 {
                tracing::info!("sibling thread noise");
            }
        });
        let out = capture(|| {
            tracing::info!("primary capture message");
        });
        handle.join().unwrap();
        assert!(out.contains("primary capture message"));
        assert!(
            !out.contains("sibling thread noise"),
            "cross-thread leak: {out:?}"
        );
    }

    #[test]
    fn capture_works_inside_a_multi_thread_tokio_runtime() {
        // Mirrors the `#[tokio::test]` cases in tests/http_errors.rs:
        // closures that need a non-tokio thread (e.g. reqwest::blocking)
        // must Just Work via `capture` without manual `detached(...)`
        // wrapping.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(async {
            capture(|| {
                tracing::info!("emitted from spawned capture thread");
            })
        });
        assert!(out.contains("emitted from spawned capture thread"));
    }
}
