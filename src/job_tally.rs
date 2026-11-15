//! The worker's contribution to the studio: how many studio jobs it
//! completed today and in total, per job kind (see
//! `docs/runtime/daemon-and-tray.md#contribution`).
//!
//! Persisted in `job-tally.json` beside the config so the total survives
//! restarts and updates.  Same recovery rules as residency: a corrupt file
//! is quarantined and counting starts afresh, an unreadable one disables
//! persistence so it is never overwritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDate;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::types::TaskKind;

const TRACE_TARGET: &str = "studio_worker::job_tally";
const VERSION: u32 = 1;

/// Completed studio jobs per kind.
pub type KindCounts = BTreeMap<TaskKind, u64>;

/// The tally as the tray UI reads it (`DaemonStatus::tally`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TallySnapshot {
    /// Jobs completed on the daemon's current local day.
    pub today: KindCounts,
    /// Jobs completed since counting began.
    pub total: KindCounts,
    /// The local day counting began; `None` before the first job.
    pub since: Option<NaiveDate>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TallyFile {
    version: u32,
    since: Option<NaiveDate>,
    /// The local day `today` counts.
    day: Option<NaiveDate>,
    today: KindCounts,
    total: KindCounts,
}

/// The tally plus the file it persists to (`None`: memory only).
#[derive(Debug, Default)]
pub struct JobTally {
    file: TallyFile,
    path: Option<PathBuf>,
}

impl JobTally {
    /// Load for serving.  Never fails: every recovery path is logged.
    pub fn load_for_serving(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let file = parse(&contents).unwrap_or_else(|| {
                    quarantine(&path);
                    TallyFile::default()
                });
                Self {
                    file,
                    path: Some(path),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self {
                file: TallyFile::default(),
                path: Some(path),
            },
            Err(err) => {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "load",
                    path = %path.display(),
                    error = %err,
                    "job tally unreadable; counting from zero in memory and persistence \
                     disabled so the file is never clobbered"
                );
                Self::default()
            }
        }
    }

    /// Count one completed studio job of `kind` on local day `day`.  A
    /// failed save is logged; the count stays in memory.
    pub fn record(&mut self, kind: TaskKind, day: NaiveDate) {
        let file = &mut self.file;
        if file.day != Some(day) {
            file.day = Some(day);
            file.today.clear();
        }
        file.since.get_or_insert(day);
        *file.today.entry(kind).or_default() += 1;
        *file.total.entry(kind).or_default() += 1;
        let (today, total) = (sum(&file.today), sum(&file.total));
        tracing::info!(
            target: TRACE_TARGET,
            op = "tally",
            kind = kind.as_str(),
            today,
            total,
            "studio job counted"
        );
        if let Some(path) = &self.path {
            if let Err(err) = save(path, &self.file) {
                tracing::warn!(
                    target: TRACE_TARGET,
                    op = "save",
                    path = %path.display(),
                    error = %format!("{err:#}"),
                    "job tally could not be saved; the count is kept in memory"
                );
            }
        }
    }

    /// The tally as seen on local day `day`: `today` is empty once the
    /// counted day has passed.
    pub fn snapshot(&self, day: NaiveDate) -> TallySnapshot {
        let file = &self.file;
        TallySnapshot {
            today: if file.day == Some(day) {
                file.today.clone()
            } else {
                KindCounts::new()
            },
            total: file.total.clone(),
            since: file.since,
        }
    }

    /// The file the tally persists to, if any.
    pub fn persistence(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// The sum of every kind's count.
pub fn sum(counts: &KindCounts) -> u64 {
    counts.values().sum()
}

/// The daemon's tally, shared by every job path and the status route.
#[derive(Debug, Clone, Default)]
pub struct SharedTally(Arc<Mutex<JobTally>>);

impl SharedTally {
    /// Replace the tally, e.g. with the one loaded from disk at start.
    pub fn replace(&self, tally: JobTally) {
        *self.0.lock() = tally;
    }

    /// Count one completed studio job of `kind` today.
    pub fn record(&self, kind: TaskKind) {
        self.0.lock().record(kind, local_today());
    }

    /// The tally as seen today.
    pub fn snapshot(&self) -> TallySnapshot {
        self.0.lock().snapshot(local_today())
    }
}

/// Today on this machine's clock: "today" is the operator's day.
fn local_today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

fn parse(contents: &str) -> Option<TallyFile> {
    let file: TallyFile = serde_json::from_str(contents).ok()?;
    (file.version == VERSION).then_some(file)
}

fn quarantine(path: &Path) {
    let target = crate::catalog::quarantine_path(path);
    match std::fs::rename(path, &target) {
        Ok(()) => tracing::warn!(
            target: TRACE_TARGET,
            op = "load",
            path = %path.display(),
            quarantine = %target.display(),
            "job tally is not valid JSON (or an unknown version); quarantined it, \
             counting from zero"
        ),
        Err(err) => tracing::error!(
            target: TRACE_TARGET,
            op = "load",
            path = %path.display(),
            error = %err,
            "job tally is not valid JSON and could not be quarantined; counting from zero"
        ),
    }
}

fn save(path: &Path, file: &TallyFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&TallyFile {
        version: VERSION,
        ..file.clone()
    })?;
    crate::config::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job-tally.json");
        (dir, path)
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 5, d).unwrap()
    }

    fn counts(pairs: &[(TaskKind, u64)]) -> KindCounts {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_fresh_tally_is_empty_and_has_no_start_day() {
        let tally = JobTally::default();
        assert_eq!(tally.snapshot(day(1)), TallySnapshot::default());
    }

    #[test]
    fn jobs_count_per_kind_today_and_in_total() {
        let mut tally = JobTally::default();
        tally.record(TaskKind::Image, day(1));
        tally.record(TaskKind::Image, day(1));
        tally.record(TaskKind::Llm, day(1));
        let snap = tally.snapshot(day(1));
        let expected = counts(&[(TaskKind::Image, 2), (TaskKind::Llm, 1)]);
        assert_eq!(snap.today, expected);
        assert_eq!(snap.total, expected);
        assert_eq!(snap.since, Some(day(1)));
        assert_eq!(sum(&snap.total), 3);
    }

    #[test]
    fn today_starts_over_on_a_new_day_and_the_total_keeps_growing() {
        let mut tally = JobTally::default();
        tally.record(TaskKind::Image, day(1));
        tally.record(TaskKind::Llm, day(2));
        let snap = tally.snapshot(day(2));
        assert_eq!(snap.today, counts(&[(TaskKind::Llm, 1)]));
        assert_eq!(
            snap.total,
            counts(&[(TaskKind::Image, 1), (TaskKind::Llm, 1)])
        );
        assert_eq!(snap.since, Some(day(1)), "counting began on the first day");
    }

    #[test]
    fn a_day_without_jobs_reads_nothing_today() {
        let mut tally = JobTally::default();
        tally.record(TaskKind::Video, day(1));
        let snap = tally.snapshot(day(3));
        assert!(snap.today.is_empty());
        assert_eq!(snap.total, counts(&[(TaskKind::Video, 1)]));
    }

    #[test]
    fn the_tally_persists_and_reloads() {
        let (_d, path) = tmp();
        let mut tally = JobTally::load_for_serving(Some(path.clone()));
        assert!(!path.exists(), "loading alone writes nothing");
        tally.record(TaskKind::AudioStt, day(4));
        tally.record(TaskKind::Image, day(4));
        let again = JobTally::load_for_serving(Some(path.clone()));
        assert_eq!(again.snapshot(day(4)), tally.snapshot(day(4)));
        assert_eq!(again.persistence(), Some(path.as_path()));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": 1,
                "since": "2026-05-04",
                "day": "2026-05-04",
                "today": { "image": 1, "audio_stt": 1 },
                "total": { "image": 1, "audio_stt": 1 },
            })
        );
    }

    #[test]
    fn counting_logs_the_kind_and_the_running_sums() {
        let logs = capture(|| {
            let mut tally = JobTally::default();
            tally.record(TaskKind::Image, day(1));
            tally.record(TaskKind::Mesh, day(1));
        });
        assert!(logs.contains("studio job counted"), "{logs}");
        assert!(logs.contains("op=\"tally\""), "{logs}");
        assert!(logs.contains("kind=\"mesh\""), "{logs}");
        assert!(
            logs.contains("today=2") && logs.contains("total=2"),
            "{logs}"
        );
    }

    #[test]
    fn a_corrupt_file_is_quarantined_and_counting_starts_afresh() {
        let (dir, path) = tmp();
        std::fs::write(&path, "{ not json").unwrap();
        let logs = capture({
            let path = path.clone();
            move || {
                let mut tally = JobTally::load_for_serving(Some(path.clone()));
                assert_eq!(tally.snapshot(day(1)), TallySnapshot::default());
                tally.record(TaskKind::Image, day(1));
            }
        });
        assert!(logs.contains("job tally is not valid JSON"), "{logs}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("job-tally.json.corrupt-")),
            "the corrupt bytes are kept for recovery: {names:?}"
        );
        let again = JobTally::load_for_serving(Some(path));
        assert_eq!(sum(&again.snapshot(day(1)).total), 1);
    }

    #[test]
    fn an_unknown_version_is_quarantined_too() {
        let (_d, path) = tmp();
        std::fs::write(
            &path,
            r#"{"version":9,"since":null,"day":null,"today":{},"total":{}}"#,
        )
        .unwrap();
        let tally = JobTally::load_for_serving(Some(path.clone()));
        assert_eq!(tally.snapshot(day(1)), TallySnapshot::default());
        assert!(!path.exists(), "moved aside");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_disables_persistence() {
        let (_d, path) = tmp();
        std::fs::create_dir(&path).unwrap();
        let logs = capture({
            let path = path.clone();
            move || {
                let tally = JobTally::load_for_serving(Some(path));
                assert_eq!(tally.persistence(), None);
            }
        });
        assert!(logs.contains("job tally unreadable"), "{logs}");
        assert!(path.is_dir(), "left alone");
    }

    #[cfg(unix)]
    #[test]
    fn a_corrupt_file_that_cannot_be_quarantined_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = tmp();
        std::fs::write(&path, "nope").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let logs = capture({
            let path = path.clone();
            move || {
                JobTally::load_for_serving(Some(path));
            }
        });
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(logs.contains("could not be quarantined"), "{logs}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_save_keeps_the_count_in_memory_and_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = tmp();
        let mut tally = JobTally::load_for_serving(Some(path));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let logs = capture(move || {
            tally.record(TaskKind::Image, day(1));
            assert_eq!(sum(&tally.snapshot(day(1)).total), 1);
        });
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(logs.contains("job tally could not be saved"), "{logs}");
    }

    #[test]
    fn the_shared_tally_counts_today_on_the_local_clock() {
        let shared = SharedTally::default();
        shared.record(TaskKind::Llm);
        let snap = shared.snapshot();
        assert_eq!(snap.today, counts(&[(TaskKind::Llm, 1)]));
        assert_eq!(snap.since, Some(chrono::Local::now().date_naive()));
        shared.replace(JobTally::default());
        assert_eq!(shared.snapshot(), TallySnapshot::default());
    }

    #[test]
    fn the_snapshot_travels_as_kind_keyed_maps() {
        let mut tally = JobTally::default();
        tally.record(TaskKind::AudioTts, day(7));
        let json = serde_json::to_value(tally.snapshot(day(7))).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "today": { "audio_tts": 1 },
                "total": { "audio_tts": 1 },
                "since": "2026-05-07",
            })
        );
        let back: TallySnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(back, tally.snapshot(day(7)));
    }
}
