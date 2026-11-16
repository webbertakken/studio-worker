//! The machine id: a stable, pseudonymous identifier of this computer, the same across
//! reinstalls of the worker, so the studio can tell a reinstall from a new machine and keep its
//! contribution together (see `docs/runtime/machine-id.md`).
//!
//! Derived from the operating system's own identifier (`/etc/machine-id`, `IOPlatformUUID`,
//! `MachineGuid`), hashed under a studio-worker prefix: the raw identifier never leaves the
//! machine, and the hash is useless to any other application.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};

const TRACE_TARGET: &str = "studio_worker::machine_id";

/// Separates this hash from any other use of the same OS identifier.
const DOMAIN: &str = "studio-worker/machine-id/v1:";

/// Hex characters sent: 128 bits, what the studio accepts.
const LENGTH: usize = 32;

/// This machine's id, read once per process; `None` when the OS identifier cannot be read.
pub fn machine_id() -> Option<String> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        let id = read_os_identifier().and_then(|raw| {
            derive(&raw).ok_or_else(|| std::io::Error::other("the OS machine identifier is empty"))
        });
        match id {
            Ok(id) => {
                tracing::info!(target: TRACE_TARGET, op = "machine_id", machine_id = %id, "machine id derived");
                Some(id)
            }
            Err(err) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "machine_id",
                    error = %err,
                    "machine id unavailable; the studio cannot recognise this machine across reinstalls"
                );
                None
            }
        }
    })
    .clone()
}

/// The machine id of OS identifier `raw` (case and surrounding space ignored); `None` when empty.
pub fn derive(raw: &str) -> Option<String> {
    let normalised = raw.trim().to_ascii_lowercase();
    if normalised.is_empty() {
        return None;
    }
    let digest = Sha256::digest(format!("{DOMAIN}{normalised}"));
    Some(hex::encode(digest)[..LENGTH].to_string())
}

/// The `IOPlatformUUID` in `ioreg -rd1 -c IOPlatformExpertDevice` output.
pub fn platform_uuid_from_ioreg(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains("\"IOPlatformUUID\""))
        .and_then(|line| line.split('"').nth(3))
        .map(str::to_string)
}

#[cfg(target_os = "linux")]
fn read_os_identifier() -> std::io::Result<String> {
    std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
}

#[cfg(target_os = "macos")]
fn read_os_identifier() -> std::io::Result<String> {
    let output = crate::process::command("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()?;
    platform_uuid_from_ioreg(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| std::io::Error::other("ioreg reported no IOPlatformUUID"))
}

#[cfg(windows)]
fn read_os_identifier() -> std::io::Result<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )?
        .get_value("MachineGuid")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_os_identifier() -> std::io::Result<String> {
    Err(std::io::Error::other(
        "no machine identifier on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_32_lowercase_hex_characters() {
        let id = derive("4c4c4544-0042-3510-8051-b4c04f4a4d32").unwrap();
        assert_eq!(id.len(), 32);
        assert!(
            id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "{id}"
        );
    }

    #[test]
    fn the_same_machine_reads_the_same_whatever_the_case_and_spacing() {
        assert_eq!(
            derive("ABCDEF0123\n"),
            derive("  abcdef0123"),
            "machine-id ends in a newline; MachineGuid and IOPlatformUUID differ in case"
        );
    }

    #[test]
    fn another_machine_reads_differently_and_the_raw_id_never_shows() {
        let raw = "abcdef0123456789abcdef0123456789";
        let id = derive(raw).unwrap();
        assert_ne!(id, derive("abcdef0123456789abcdef012345678a").unwrap());
        assert_ne!(id, raw, "the OS identifier is hashed, never sent");
    }

    #[test]
    fn an_empty_identifier_has_no_id() {
        assert_eq!(derive(""), None);
        assert_eq!(derive(" \n"), None);
    }

    #[test]
    fn ioreg_output_yields_the_platform_uuid() {
        let output = r#"+-o J316sAP  <class IOPlatformExpertDevice, id 0x100000243>
    {
      "IOPlatformSerialNumber" = "C02XYZ"
      "IOPlatformUUID" = "1B2C3D4E-0000-1111-2222-333344445555"
    }"#;
        assert_eq!(
            platform_uuid_from_ioreg(output).as_deref(),
            Some("1B2C3D4E-0000-1111-2222-333344445555")
        );
        assert_eq!(platform_uuid_from_ioreg("no such line"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_machine_has_an_id() {
        assert!(machine_id().is_some_and(|id| id.len() == 32));
    }
}
