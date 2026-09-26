//! One daemon per config directory.
//!
//! `run` holds `<config dir>/daemon.lock` exclusively for its whole life;
//! the OS releases it when the process exits, however it exits.  The tray UI
//! uses [`is_held`] to tell "no daemon" (start one) from "a daemon is
//! starting" (wait).

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TRACE_TARGET: &str = "studio_worker::daemon";

/// The lock file's name, next to `config.toml`.
pub const LOCK_FILE_NAME: &str = "daemon.lock";
/// Where the daemon writes its pid, next to the lock.  Separate from the
/// lock file because Windows locks are mandatory: a locked file cannot be
/// read, even by the process that holds it through another handle.
pub const PID_FILE_NAME: &str = "daemon.pid";

/// How often [`acquire`] tries before concluding another daemon runs, and
/// the pause between tries.  Covers the instant the tray UI's
/// [`is_held`] probe holds the lock shared.
pub const ACQUIRE_ATTEMPTS: u32 = 10;
pub const ACQUIRE_PAUSE: Duration = Duration::from_millis(200);

/// The lock file for the config at `config_path`.
pub fn lock_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(LOCK_FILE_NAME)
}

/// The pid file for the config at `config_path`.
pub fn pid_path_for(config_path: &Path) -> PathBuf {
    lock_path_for(config_path).with_file_name(PID_FILE_NAME)
}

/// The held daemon lock; dropping it releases the lock.
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
    path: PathBuf,
}

impl DaemonLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The outcome of [`acquire`].
#[derive(Debug)]
pub enum Acquired {
    /// This process is the daemon for the config.
    Mine(DaemonLock),
    /// Another process holds the lock.
    HeldElsewhere,
}

fn open(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

/// Take the daemon lock for the config at `config_path`, logging the
/// outcome.
pub fn acquire(config_path: &Path) -> std::io::Result<Acquired> {
    acquire_with(config_path, ACQUIRE_ATTEMPTS, ACQUIRE_PAUSE)
}

/// [`acquire`] with an explicit retry budget.
pub fn acquire_with(
    config_path: &Path,
    attempts: u32,
    pause: Duration,
) -> std::io::Result<Acquired> {
    let path = lock_path_for(config_path);
    let file = open(&path)?;
    for attempt in 1..=attempts.max(1) {
        match file.try_lock() {
            Ok(()) => {
                // Informational only: operators can see which pid is the daemon.
                let pid_path = path.with_file_name(PID_FILE_NAME);
                if let Err(e) = write_pid(&pid_path) {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "daemon_lock",
                        path = %pid_path.display(),
                        error = %e,
                        "could not write the daemon pid file"
                    );
                }
                tracing::info!(
                    target: TRACE_TARGET,
                    op = "daemon_lock",
                    path = %path.display(),
                    pid = std::process::id(),
                    "daemon lock taken"
                );
                return Ok(Acquired::Mine(DaemonLock { _file: file, path }));
            }
            Err(TryLockError::WouldBlock) if attempt < attempts => std::thread::sleep(pause),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    tracing::info!(
        target: TRACE_TARGET,
        op = "daemon_lock",
        path = %path.display(),
        "another daemon is already running for this config"
    );
    Ok(Acquired::HeldElsewhere)
}

fn write_pid(path: &Path) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "{}", std::process::id())
}

/// How often a waiting daemon retries the lock.  Safe range 0.5..=10 s.
pub const WAIT_POLL: Duration = Duration::from_secs(2);

/// Block until this process holds the daemon lock (`run --wait-for-lock`,
/// for a supervised daemon).  Logs once when it starts waiting.
pub fn wait_until_acquired(config_path: &Path, poll: Duration) -> std::io::Result<DaemonLock> {
    let mut said = false;
    loop {
        match acquire_with(config_path, 1, poll)? {
            Acquired::Mine(lock) => return Ok(lock),
            Acquired::HeldElsewhere => {
                if !said {
                    let holder =
                        std::fs::read_to_string(pid_path_for(config_path)).unwrap_or_default();
                    tracing::info!(
                        target: TRACE_TARGET,
                        op = "daemon_lock",
                        holder = holder.trim(),
                        "waiting for the daemon lock; taking over when the other daemon ends"
                    );
                    said = true;
                }
                std::thread::sleep(poll);
            }
        }
    }
}

/// Whether a daemon holds the lock for the config at `config_path`.
pub fn is_held(config_path: &Path) -> std::io::Result<bool> {
    let file = open(&lock_path_for(config_path))?;
    match file.try_lock_shared() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = Duration::from_millis(1);

    #[test]
    fn the_lock_file_sits_next_to_the_config() {
        assert_eq!(
            lock_path_for(Path::new("/etc/sw/config.toml")),
            PathBuf::from("/etc/sw/daemon.lock")
        );
    }

    #[test]
    fn a_waiting_daemon_takes_the_lock_once_it_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let Acquired::Mine(first) = acquire_with(&config, 1, FAST).unwrap() else {
            panic!("first");
        };
        let waiter = {
            let config = config.clone();
            std::thread::spawn(move || {
                crate::test_support::capture(move || {
                    let lock = wait_until_acquired(&config, Duration::from_millis(10)).unwrap();
                    assert!(lock.path().ends_with(LOCK_FILE_NAME));
                })
            })
        };
        std::thread::sleep(Duration::from_millis(80));
        drop(first);
        let logs = waiter.join().unwrap();
        assert_eq!(
            logs.matches("waiting for the daemon lock").count(),
            1,
            "{logs}"
        );
        assert!(logs.contains("daemon lock taken"), "{logs}");
    }

    #[test]
    fn only_one_daemon_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("nested").join("config.toml");
        assert!(!is_held(&config).unwrap());

        let Acquired::Mine(lock) = acquire_with(&config, 2, FAST).unwrap() else {
            panic!("the first daemon must get the lock");
        };
        assert!(lock.path().ends_with(LOCK_FILE_NAME));
        assert!(is_held(&config).unwrap());
        assert!(matches!(
            acquire_with(&config, 2, FAST).unwrap(),
            Acquired::HeldElsewhere
        ));
        // Readable while the lock is held (Windows locks are mandatory, so
        // the pid cannot live in the locked file itself).
        let pid = std::fs::read_to_string(pid_path_for(&config)).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());

        drop(lock);
        assert!(!is_held(&config).unwrap());
        assert!(matches!(
            acquire_with(&config, 1, FAST).unwrap(),
            Acquired::Mine(_)
        ));
    }

    #[test]
    fn the_outcome_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let logs = crate::test_support::capture(move || {
            let _held = acquire_with(&config, 1, FAST).unwrap();
            let _ = acquire_with(&config, 1, FAST).unwrap();
        });
        assert!(logs.contains("op=\"daemon_lock\""), "{logs}");
        assert!(logs.contains("daemon lock taken"), "{logs}");
        assert!(logs.contains("another daemon is already running"), "{logs}");
    }
}
