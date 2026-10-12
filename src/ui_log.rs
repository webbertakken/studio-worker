//! The tray UI writes its log to `<config dir>/ui.log` however it was launched: from the login
//! entry, by `setup`, by the daemon relaunching it, or in place after an update. The layer is part
//! of every process's subscriber and stays silent until [`open`] names the file (`studio-worker
//! ui` does, first thing). Launchers send the UI's stdout to null and its stderr to the same file,
//! so a line is never written twice and a panic message still lands there.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::Mutex;
use tracing_subscriber::fmt::MakeWriter;

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Start appending this process's log to `path` (once; later calls change nothing).
pub fn open(path: &Path) -> io::Result<()> {
    if FILE.get().is_some() {
        return Ok(());
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let _ = FILE.set(Mutex::new(file));
    Ok(())
}

/// Whether [`open`] named a file.
pub fn is_open() -> bool {
    FILE.get().is_some()
}

/// The fmt layer writing to the opened file, plain text.
pub fn layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(UiLogWriter)
}

/// Writes to the opened file, or nowhere before [`open`].
#[derive(Debug, Clone, Copy, Default)]
pub struct UiLogWriter;

impl Write for UiLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match FILE.get() {
            Some(file) => file.lock().write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match FILE.get() {
            Some(file) => file.lock().flush(),
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for UiLogWriter {
    type Writer = UiLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    // `FILE` is process-wide: one test drives it end to end.
    #[test]
    fn writes_nothing_until_opened_then_appends_every_event_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.log");
        std::fs::write(&path, "earlier line\n").unwrap();
        let subscriber = tracing_subscriber::registry().with(layer());

        tracing::subscriber::with_default(subscriber, || {
            assert!(!is_open());
            tracing::info!("before open");
            open(&path).unwrap();
            assert!(is_open());
            tracing::warn!(op = "probe", "after open");
            open(&dir.path().join("elsewhere.log")).unwrap();
            tracing::info!("still the first file");
        });

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("earlier line\n"), "{text}");
        assert!(!text.contains("before open"), "{text}");
        assert!(text.contains("WARN after open op=\"probe\""), "{text}");
        assert!(text.contains("still the first file"), "{text}");
        assert!(!text.contains('\u{1b}'), "no colour codes: {text}");
        assert!(!dir.path().join("elsewhere.log").exists());
    }
}
