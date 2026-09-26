//! Persisted residency: which catalogue models the operator wants loaded
//! (see `docs/runtime/model-lifecycle.md`).
//!
//! Kept apart from the catalogue so a studio catalogue sync can never
//! change it.  Same recovery rules as the catalogue: a corrupt file is
//! quarantined, an unreadable one disables persistence so it is never
//! overwritten.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const TRACE_TARGET: &str = "studio_worker::residency";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResidencyFile {
    version: u32,
    resident: BTreeSet<String>,
}

/// The resident set plus the file it persists to (`None`: memory only).
#[derive(Debug, Default)]
pub struct Residency {
    resident: BTreeSet<String>,
    path: Option<PathBuf>,
}

impl Residency {
    /// Load for serving.  Never fails: every recovery path is logged.
    pub fn load_for_serving(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => match parse(&contents) {
                Some(resident) => Self {
                    resident,
                    path: Some(path),
                },
                None => {
                    quarantine(&path);
                    Self {
                        resident: BTreeSet::new(),
                        path: Some(path),
                    }
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self {
                resident: BTreeSet::new(),
                path: Some(path),
            },
            Err(err) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "load",
                    path = %path.display(),
                    error = %err,
                    "residency unreadable; nothing restored and persistence \
                     disabled so the file is never clobbered"
                );
                Self::default()
            }
        }
    }

    pub fn is_resident(&self, id: &str) -> bool {
        self.resident.contains(id)
    }

    /// Resident ids, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.resident.iter().map(String::as_str)
    }

    /// The file residency persists to, if any.
    pub fn persistence(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Mark `id` resident or not.  `Ok(true)` when something changed.
    /// Persists before committing, so a failed save changes nothing.
    pub fn set(&mut self, id: &str, resident: bool) -> std::io::Result<bool> {
        if self.is_resident(id) == resident {
            return Ok(false);
        }
        let mut next = self.resident.clone();
        if resident {
            next.insert(id.to_string());
        } else {
            next.remove(id);
        }
        if let Some(path) = &self.path {
            save(path, &next)?;
            tracing::info!(
                target: TRACE_TARGET,
                op = "save",
                path = %path.display(),
                model = id,
                resident,
                "residency saved"
            );
        }
        self.resident = next;
        Ok(true)
    }
}

fn parse(contents: &str) -> Option<BTreeSet<String>> {
    let file: ResidencyFile = serde_json::from_str(contents).ok()?;
    (file.version == VERSION).then_some(file.resident)
}

fn quarantine(path: &Path) {
    let target = crate::catalog::quarantine_path(path);
    match std::fs::rename(path, &target) {
        Ok(()) => tracing::warn!(
            target: TRACE_TARGET,
            op = "load",
            path = %path.display(),
            quarantine = %target.display(),
            "residency is not valid JSON (or an unknown version); quarantined it, \
             nothing restored"
        ),
        Err(err) => tracing::error!(
            target: TRACE_TARGET,
            op = "load",
            path = %path.display(),
            error = %err,
            "residency is not valid JSON and could not be quarantined; nothing restored"
        ),
    }
}

fn save(path: &Path, resident: &BTreeSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&ResidencyFile {
        version: VERSION,
        resident: resident.clone(),
    })
    .map_err(std::io::Error::other)?;
    crate::config::write_atomic(path, json.as_bytes()).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("residency.json");
        (dir, path)
    }

    #[test]
    fn a_missing_file_means_nothing_is_resident() {
        let (_d, path) = tmp();
        let r = Residency::load_for_serving(Some(path.clone()));
        assert_eq!(r.ids().count(), 0);
        assert_eq!(r.persistence(), Some(path.as_path()));
        assert!(!path.exists(), "loading alone writes nothing");
    }

    #[test]
    fn set_persists_and_reloads() {
        let (_d, path) = tmp();
        let mut r = Residency::load_for_serving(Some(path.clone()));
        r.set("stt-a", true).unwrap();
        r.set("llm-b", true).unwrap();
        r.set("stt-a", false).unwrap();
        let again = Residency::load_for_serving(Some(path.clone()));
        assert!(again.is_resident("llm-b"));
        assert!(!again.is_resident("stt-a"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "version": 1, "resident": ["llm-b"] })
        );
    }

    #[test]
    fn set_reports_whether_anything_changed() {
        let (_d, path) = tmp();
        let mut r = Residency::load_for_serving(Some(path));
        assert!(r.set("m", true).unwrap());
        assert!(!r.set("m", true).unwrap());
        assert!(r.set("m", false).unwrap());
        assert!(!r.set("m", false).unwrap());
    }

    #[test]
    fn a_corrupt_file_is_quarantined_and_nothing_is_resident() {
        let (dir, path) = tmp();
        std::fs::write(&path, "{ not json").unwrap();
        let logs = crate::test_support::capture({
            let path = path.clone();
            move || {
                let r = Residency::load_for_serving(Some(path.clone()));
                assert_eq!(r.ids().count(), 0);
                assert_eq!(r.persistence(), Some(path.as_path()));
            }
        });
        assert!(logs.contains("residency is not valid JSON"), "{logs}");
        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("residency.json.corrupt-")
            });
        assert!(quarantined, "the corrupt bytes are kept for recovery");
    }

    #[cfg(unix)]
    #[test]
    fn a_corrupt_file_that_cannot_be_quarantined_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = tmp();
        std::fs::write(&path, "nope").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let logs = crate::test_support::capture({
            let path = path.clone();
            move || {
                let r = Residency::load_for_serving(Some(path));
                assert_eq!(r.ids().count(), 0);
            }
        });
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(logs.contains("could not be quarantined"), "{logs}");
        assert!(path.exists(), "the original bytes stay where they were");
    }

    #[test]
    fn an_unknown_version_is_quarantined_too() {
        let (_d, path) = tmp();
        std::fs::write(&path, r#"{"version":2,"resident":["x"]}"#).unwrap();
        let r = Residency::load_for_serving(Some(path));
        assert!(!r.is_resident("x"));
    }

    #[test]
    fn an_unreadable_file_disables_persistence() {
        let (_d, path) = tmp();
        std::fs::create_dir(&path).unwrap();
        let logs = crate::test_support::capture({
            let path = path.clone();
            move || {
                let mut r = Residency::load_for_serving(Some(path));
                assert_eq!(r.persistence(), None);
                assert!(r.set("m", true).unwrap(), "still tracked in memory");
                assert!(r.is_resident("m"));
            }
        });
        assert!(logs.contains("residency unreadable"), "{logs}");
        assert!(path.is_dir(), "never clobbers what it could not read");
    }

    #[test]
    fn no_path_keeps_residency_in_memory_only() {
        let mut r = Residency::load_for_serving(None);
        assert!(r.set("m", true).unwrap());
        assert!(r.is_resident("m"));
        assert_eq!(r.persistence(), None);
    }

    #[test]
    fn a_failed_save_leaves_memory_unchanged() {
        let (dir, path) = tmp();
        let mut r = Residency::load_for_serving(Some(path.clone()));
        // Make the target a directory after load so the atomic write fails.
        std::fs::create_dir(&path).unwrap();
        assert!(r.set("m", true).is_err());
        assert!(!r.is_resident("m"));
        drop(dir);
    }

    #[test]
    fn saves_leave_a_breadcrumb() {
        let (_d, path) = tmp();
        let logs = crate::test_support::capture(move || {
            let mut r = Residency::load_for_serving(Some(path));
            r.set("llm-b", true).unwrap();
        });
        assert!(logs.contains("residency saved"), "{logs}");
        assert!(logs.contains("llm-b"), "{logs}");
    }
}
