//! Keep `daemon.log` and `ui.log` small by copy-truncate: the live file keeps its name and its
//! writers' handles, `<name>.1` to `<name>.3` keep the past.  See
//! `docs/runtime/daemon-and-tray.md#log-files`.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TRACE_TARGET: &str = "studio_worker::log_trim";

/// The live file is trimmed above this size.
pub const LOG_TRIM_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Numbered copies kept beside the live file.
pub const LOG_TRIM_KEEP: u32 = 3;
/// How often a running process checks its log file.
pub const LOG_TRIM_INTERVAL: Duration = Duration::from_secs(60);

/// `<path>.<n>`.
pub fn numbered(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Trim `path` when it is larger than `max_bytes`: shift the numbered copies up (dropping the
/// one past `keep`), copy the live file to `<path>.1`, truncate it to zero.  The size trimmed,
/// or `None` when the file is missing or within the limit.
pub fn trim(path: &Path, max_bytes: u64, keep: u32) -> io::Result<Option<u64>> {
    let size = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if size <= max_bytes || keep == 0 {
        return Ok(None);
    }
    for n in (1..keep).rev() {
        let from = numbered(path, n);
        match std::fs::rename(&from, numbered(path, n + 1)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    std::fs::copy(path, numbered(path, 1))?;
    OpenOptions::new().write(true).open(path)?.set_len(0)?;
    Ok(Some(size))
}

/// [`trim`] with the default limits, logged: info when it trimmed, a warning when it failed.
pub fn trim_logged(path: &Path) {
    match trim(path, LOG_TRIM_MAX_BYTES, LOG_TRIM_KEEP) {
        Ok(Some(bytes)) => tracing::info!(
            target: TRACE_TARGET,
            op = "log_trim",
            path = %path.display(),
            bytes,
            keep = LOG_TRIM_KEEP,
            "log file trimmed; the previous contents are in <name>.1"
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!(
            target: TRACE_TARGET,
            op = "log_trim",
            path = %path.display(),
            error = %e,
            "could not trim the log file"
        ),
    }
}

/// Trim `path` now and every [`LOG_TRIM_INTERVAL`], on a thread of its own, for the life of the
/// process.
pub fn spawn(path: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("log-trim".into())
        .spawn(move || loop {
            trim_logged(&path);
            std::thread::sleep(LOG_TRIM_INTERVAL);
        });
    if let Err(e) = spawned {
        tracing::warn!(
            target: TRACE_TARGET,
            op = "log_trim",
            error = %e,
            "could not start the log trim thread"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_file_within_the_limit_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        write(&log, b"0123456789");
        assert_eq!(trim(&log, 10, 3).unwrap(), None);
        assert_eq!(read(&log), "0123456789");
        assert!(!numbered(&log, 1).exists());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(trim(&dir.path().join("daemon.log"), 10, 3).unwrap(), None);
    }

    #[test]
    fn a_file_over_the_limit_is_copied_aside_and_truncated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        write(&log, b"first generation\n");
        assert_eq!(trim(&log, 4, 3).unwrap(), Some(17));
        assert_eq!(read(&numbered(&log, 1)), "first generation\n");
        assert_eq!(read(&log), "", "the live file keeps its name, emptied");
    }

    #[test]
    fn copies_shift_up_and_the_oldest_past_keep_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        for generation in ["one", "two", "three", "four", "five"] {
            write(&log, format!("{generation} is long enough\n").as_bytes());
            trim(&log, 4, 3).unwrap();
        }
        assert!(read(&numbered(&log, 1)).starts_with("five"));
        assert!(read(&numbered(&log, 2)).starts_with("four"));
        assert!(read(&numbered(&log, 3)).starts_with("three"));
        assert!(!numbered(&log, 4).exists(), "only three copies are kept");
    }

    #[test]
    fn an_append_writer_carries_on_at_the_start_of_the_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        // The tray UI opens daemon.log in append mode and hands it to the daemon.
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        writer.write_all(b"before the trim, long enough\n").unwrap();
        trim(&log, 4, 3).unwrap();
        writer.write_all(b"after\n").unwrap();
        drop(writer);
        assert_eq!(read(&log), "after\n");
        assert_eq!(read(&numbered(&log, 1)), "before the trim, long enough\n");
    }

    #[test]
    fn a_trim_is_logged_and_a_failure_warns() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        write(&log, &vec![b'x'; (LOG_TRIM_MAX_BYTES + 1) as usize]);
        let trimmed = log.clone();
        let out = crate::test_support::capture(move || trim_logged(&trimmed));
        assert!(out.contains("INFO") && out.contains("log_trim"), "{out}");
        assert!(out.contains("log file trimmed"), "{out}");
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);

        // A non-empty directory where `.2` must move makes the shift fail.
        write(&log, &vec![b'x'; (LOG_TRIM_MAX_BYTES + 1) as usize]);
        write(&numbered(&log, 2), b"an old copy");
        std::fs::create_dir(numbered(&log, 3)).unwrap();
        std::fs::write(numbered(&log, 3).join("blocker"), b"x").unwrap();
        let failing = log.clone();
        let out = crate::test_support::capture(move || trim_logged(&failing));
        assert!(out.contains("WARN"), "{out}");
        assert!(out.contains("could not trim the log file"), "{out}");
    }

    #[test]
    fn numbered_copies_append_to_the_full_name() {
        assert_eq!(
            numbered(Path::new("/c/daemon.log"), 2),
            PathBuf::from("/c/daemon.log.2")
        );
    }
}
