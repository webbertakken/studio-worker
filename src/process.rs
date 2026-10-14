//! Child processes.
//!
//! The daemon and the tray run without a console on Windows (the daemon is started detached, the
//! tray from the login entry). A console program they start, `nvidia-smi`, `powershell`, `sd-cli`
//! or `schtasks`, then opens a console window of its own in front of the player. Every child is
//! therefore built here: [`command`] for a helper the worker waits on, [`detached`] for a process
//! that outlives the worker. `clippy.toml` refuses `std::process::Command::new` anywhere else.

use std::ffi::OsStr;
use std::process::Command;

/// `CREATE_NO_WINDOW`: a console program runs without a console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `DETACHED_PROCESS`: the child gets no console at all, not even its parent's.
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
/// `CREATE_NEW_PROCESS_GROUP`: the child leaves the parent's group, so it outlives it.
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How a child relates to the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// A helper the worker runs and reads: hidden, in the worker's process group.
    Background,
    /// A process that outlives the worker (the daemon, the tray, an updated binary): hidden, in
    /// a process group of its own.
    Detached,
}

impl Launch {
    /// The Windows process creation flags for this launch.
    pub const fn windows_creation_flags(self) -> u32 {
        match self {
            Launch::Background => CREATE_NO_WINDOW,
            Launch::Detached => DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        }
    }
}

/// A helper process that never shows a console window.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    build(program.as_ref(), Launch::Background)
}

/// A process that outlives the worker, in its own process group, without a console window.
pub fn detached(program: impl AsRef<OsStr>) -> Command {
    build(program.as_ref(), Launch::Detached)
}

#[allow(clippy::disallowed_methods)] // The one place a `Command` is made.
fn build(program: &OsStr, launch: Launch) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(launch.windows_creation_flags());
    }
    #[cfg(unix)]
    if launch == Launch::Detached {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_helper_never_opens_a_console_window() {
        assert_eq!(
            Launch::Background.windows_creation_flags(),
            CREATE_NO_WINDOW
        );
    }

    #[test]
    fn a_detached_process_has_no_console_and_its_own_group() {
        let flags = Launch::Detached.windows_creation_flags();
        assert_eq!(flags & CREATE_NO_WINDOW, CREATE_NO_WINDOW);
        assert_eq!(flags & DETACHED_PROCESS, DETACHED_PROCESS);
        assert_eq!(flags & CREATE_NEW_PROCESS_GROUP, CREATE_NEW_PROCESS_GROUP);
    }

    #[test]
    fn command_runs_the_named_program() {
        let cmd = command("nvidia-smi");
        assert_eq!(cmd.get_program(), "nvidia-smi");
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_runs_and_reports_its_output() {
        let output = command("sh").args(["-c", "echo hidden"]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hidden");
    }

    #[cfg(unix)]
    #[test]
    fn a_detached_process_leads_its_own_group() {
        let output = detached("sh")
            .args(["-c", "ps -o pgid= -p $$; echo $$"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines().map(str::trim);
        let group = lines.next().unwrap();
        let pid = lines.next().unwrap();
        assert_eq!(group, pid);
    }
}
