//! Notice when the executable a process started from is replaced (an update, a reinstall), so
//! the tray UI can restart on the new binary.  See
//! `docs/runtime/daemon-and-tray.md#restart-after-an-update`.
//!
//! [`ExeWatch`] is the pure decision; the tray UI feeds it [`ExeIdentity::of`] every
//! [`EXE_WATCH_INTERVAL`].

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// How often the launch path is checked.
pub const EXE_WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// How long a new file must stay unchanged before it is trusted (an installer may still be
/// writing it).
pub const EXE_SETTLE: Duration = Duration::from_secs(3);

/// What identifies one version of a file: size and modification time, plus device and inode
/// on Unix (a replaced file is a new inode even when size and time match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExeIdentity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl ExeIdentity {
    /// The identity of the file at `path`, or `None` when it cannot be read (missing).
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            dev: meta.dev(),
            #[cfg(unix)]
            ino: meta.ino(),
        })
    }
}

/// What one check decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStep {
    /// The file is the one the process started from.
    Unchanged,
    /// The file is missing; the first check that finds it missing reports `first: true`.
    Missing { first: bool },
    /// The file changed and has not settled yet.
    Settling,
    /// The file changed and held still for [`EXE_SETTLE`]: restart on it.
    Restart,
}

/// The launch path's identity over time.
#[derive(Debug)]
pub struct ExeWatch {
    baseline: Option<ExeIdentity>,
    pending: Option<(ExeIdentity, Instant)>,
    missing: bool,
    settle: Duration,
}

impl ExeWatch {
    /// Watch from `baseline`, the identity when the process started.
    pub fn new(baseline: Option<ExeIdentity>, settle: Duration) -> Self {
        Self {
            baseline,
            pending: None,
            missing: false,
            settle,
        }
    }

    /// One check: `now` is the identity read at `at`.  After [`WatchStep::Restart`] the new
    /// identity becomes the baseline, so a failed restart is not retried until the file changes
    /// again.
    pub fn observe(&mut self, now: Option<ExeIdentity>, at: Instant) -> WatchStep {
        let Some(now) = now else {
            self.pending = None;
            let first = !self.missing;
            self.missing = true;
            return WatchStep::Missing { first };
        };
        self.missing = false;
        if Some(now) == self.baseline {
            self.pending = None;
            return WatchStep::Unchanged;
        }
        match self.pending {
            Some((pending, since)) if pending == now => {
                if at.duration_since(since) >= self.settle {
                    self.baseline = Some(now);
                    self.pending = None;
                    WatchStep::Restart
                } else {
                    WatchStep::Settling
                }
            }
            _ => {
                self.pending = Some((now, at));
                WatchStep::Settling
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: Duration = Duration::from_secs(3);

    fn file(dir: &Path, body: &[u8]) -> Option<ExeIdentity> {
        let path = dir.join("studio-worker");
        // Replace the way installers do: a new file, not an overwrite in place.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, body).unwrap();
        ExeIdentity::of(&path)
    }

    #[test]
    fn an_unchanged_file_never_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let id = file(dir.path(), b"v1");
        let mut watch = ExeWatch::new(id, SETTLE);
        let t0 = Instant::now();
        for s in 0..10 {
            assert_eq!(
                watch.observe(id, t0 + Duration::from_secs(s)),
                WatchStep::Unchanged
            );
        }
    }

    #[test]
    fn a_replaced_file_restarts_once_it_has_settled() {
        let dir = tempfile::tempdir().unwrap();
        let old = file(dir.path(), b"v1");
        let new = file(dir.path(), b"v2, longer");
        assert_ne!(old, new);
        let mut watch = ExeWatch::new(old, SETTLE);
        let t0 = Instant::now();
        assert_eq!(watch.observe(new, t0), WatchStep::Settling);
        assert_eq!(
            watch.observe(new, t0 + Duration::from_secs(2)),
            WatchStep::Settling
        );
        assert_eq!(
            watch.observe(new, t0 + Duration::from_secs(3)),
            WatchStep::Restart
        );
        // The new file is now the baseline: no second restart.
        assert_eq!(
            watch.observe(new, t0 + Duration::from_secs(4)),
            WatchStep::Unchanged
        );
    }

    #[test]
    fn a_file_still_being_written_restarts_only_after_it_stops_changing() {
        let dir = tempfile::tempdir().unwrap();
        let old = file(dir.path(), b"v1");
        let mut watch = ExeWatch::new(old, SETTLE);
        let t0 = Instant::now();
        let mut body = b"v2".to_vec();
        for s in 0..5 {
            body.extend_from_slice(b" more");
            let partial = file(dir.path(), &body);
            assert_eq!(
                watch.observe(partial, t0 + Duration::from_secs(s * 2)),
                WatchStep::Settling,
                "second {s}"
            );
        }
        let done = ExeIdentity::of(&dir.path().join("studio-worker"));
        assert_eq!(
            watch.observe(done, t0 + Duration::from_secs(8 + 3)),
            WatchStep::Restart
        );
    }

    #[test]
    fn a_missing_file_waits_and_says_so_once() {
        let dir = tempfile::tempdir().unwrap();
        let old = file(dir.path(), b"v1");
        let mut watch = ExeWatch::new(old, SETTLE);
        let t0 = Instant::now();
        assert_eq!(watch.observe(None, t0), WatchStep::Missing { first: true });
        assert_eq!(
            watch.observe(None, t0 + Duration::from_secs(10)),
            WatchStep::Missing { first: false }
        );
        let new = file(dir.path(), b"v2, longer");
        assert_eq!(
            watch.observe(new, t0 + Duration::from_secs(11)),
            WatchStep::Settling
        );
        assert_eq!(
            watch.observe(new, t0 + Duration::from_secs(14)),
            WatchStep::Restart
        );
    }

    #[test]
    fn going_back_to_the_original_file_cancels_the_pending_restart() {
        let dir = tempfile::tempdir().unwrap();
        let old = file(dir.path(), b"v1");
        let mut watch = ExeWatch::new(old, SETTLE);
        let t0 = Instant::now();
        let other = Some(ExeIdentity {
            len: 999,
            ..old.unwrap()
        });
        assert_eq!(watch.observe(other, t0), WatchStep::Settling);
        assert_eq!(
            watch.observe(old, t0 + Duration::from_secs(1)),
            WatchStep::Unchanged
        );
        assert_eq!(
            watch.observe(other, t0 + Duration::from_secs(5)),
            WatchStep::Settling,
            "the settle clock starts again"
        );
    }

    #[test]
    fn an_unreadable_path_has_no_identity() {
        assert_eq!(ExeIdentity::of(Path::new("/definitely/not/here")), None);
    }
}
