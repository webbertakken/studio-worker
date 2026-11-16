//! The worker's contribution to the studio: how many studio jobs it
//! completed today and in total, per job kind (see
//! `docs/runtime/daemon-and-tray.md#contribution`).
//!
//! Once the studio has said what it counted for this machine (`welcome`), its figure is the
//! total and only the jobs done since are added here: the studio keeps the history across
//! reinstalls (see `docs/runtime/machine-id.md`).
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
    /// What the studio counted for this machine, as last told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    studio: Option<StudioBaseline>,
    /// Jobs completed since the studio last told.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    since_studio: KindCounts,
}

/// The studio's count for this machine, kept as the base of the total.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StudioBaseline {
    total: KindCounts,
    since: Option<NaiveDate>,
}

/// What `welcome` carries: the studio's count for this worker and every worker on its machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioContribution {
    /// Jobs per kind, keyed by the studio's kind names.
    #[serde(default)]
    pub total: BTreeMap<String, u64>,
    /// The earliest (UTC) day counted.
    #[serde(default)]
    pub since: Option<NaiveDate>,
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
        if file.studio.is_some() {
            *file.since_studio.entry(kind).or_default() += 1;
        }
        let (today, total) = (sum(&file.today), sum(&file.total));
        tracing::info!(
            target: TRACE_TARGET,
            op = "tally",
            kind = kind.as_str(),
            today,
            total,
            "studio job counted"
        );
        self.persist();
    }

    /// Take the studio's count for this machine as the base of the total; the jobs counted here
    /// before are in it.  Kinds this worker does not know are left out, and logged.
    pub fn adopt_studio(&mut self, contribution: &StudioContribution) {
        let mut total = KindCounts::new();
        for (name, &jobs) in &contribution.total {
            match serde_json::from_value::<TaskKind>(serde_json::Value::String(name.clone())) {
                Ok(kind) => *total.entry(kind).or_default() += jobs,
                Err(_) => tracing::warn!(
                    target: TRACE_TARGET,
                    op = "tally",
                    kind = %name,
                    jobs,
                    "the studio counted a job kind this worker does not know; left out"
                ),
            }
        }
        tracing::info!(
            target: TRACE_TARGET,
            op = "tally",
            total = sum(&total),
            since = ?contribution.since,
            "studio contribution adopted"
        );
        self.file.studio = Some(StudioBaseline {
            total,
            since: contribution.since,
        });
        self.file.since_studio.clear();
        self.persist();
    }

    fn persist(&self) {
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
            total: match &file.studio {
                Some(studio) => add(&studio.total, &file.since_studio),
                None => file.total.clone(),
            },
            since: file.studio.as_ref().and_then(|s| s.since).or(file.since),
        }
    }

    /// The file the tally persists to, if any.
    pub fn persistence(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// `a` and `b` added per kind.
fn add(a: &KindCounts, b: &KindCounts) -> KindCounts {
    let mut out = a.clone();
    for (kind, jobs) in b {
        *out.entry(*kind).or_default() += jobs;
    }
    out
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

    /// Take the studio's count for this machine as the base of the total.
    pub fn adopt_studio(&self, contribution: &StudioContribution) {
        self.0.lock().adopt_studio(contribution);
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

    fn studio(total: &[(&str, u64)], since: Option<NaiveDate>) -> StudioContribution {
        StudioContribution {
            total: total.iter().map(|(k, n)| (k.to_string(), *n)).collect(),
            since,
        }
    }

    #[test]
    fn the_studio_figure_becomes_the_total_and_later_jobs_add_to_it() {
        let mut tally = JobTally::default();
        tally.record(TaskKind::Image, day(9));
        tally.adopt_studio(&studio(&[("image", 120), ("llm", 4)], Some(day(1))));
        tally.record(TaskKind::Image, day(9));
        let snap = tally.snapshot(day(9));
        assert_eq!(
            snap.total,
            counts(&[(TaskKind::Image, 121), (TaskKind::Llm, 4)]),
            "the first image is in the studio's 120; the second is added"
        );
        assert_eq!(
            snap.since,
            Some(day(1)),
            "the studio's history starts earlier"
        );
        assert_eq!(
            snap.today,
            counts(&[(TaskKind::Image, 2)]),
            "today stays this worker's own count"
        );
    }

    #[test]
    fn a_later_studio_figure_replaces_the_earlier_one() {
        let mut tally = JobTally::default();
        tally.adopt_studio(&studio(&[("image", 10)], Some(day(1))));
        tally.record(TaskKind::Image, day(2));
        tally.adopt_studio(&studio(&[("image", 11)], Some(day(1))));
        assert_eq!(
            tally.snapshot(day(2)).total,
            counts(&[(TaskKind::Image, 11)])
        );
    }

    #[test]
    fn an_empty_studio_figure_keeps_the_local_start_day() {
        let mut tally = JobTally::default();
        tally.record(TaskKind::Llm, day(3));
        tally.adopt_studio(&studio(&[], None));
        let snap = tally.snapshot(day(3));
        assert!(snap.total.is_empty());
        assert_eq!(snap.since, Some(day(3)));
    }

    #[test]
    fn a_kind_this_worker_does_not_know_is_left_out_and_logged() {
        let logs = capture(|| {
            let mut tally = JobTally::default();
            tally.adopt_studio(&studio(&[("image", 2), ("hologram", 5)], Some(day(1))));
            assert_eq!(
                tally.snapshot(day(1)).total,
                counts(&[(TaskKind::Image, 2)])
            );
        });
        assert!(logs.contains("does not know"), "{logs}");
        assert!(logs.contains("hologram"), "{logs}");
        assert!(logs.contains("studio contribution adopted"), "{logs}");
    }

    #[test]
    fn the_studio_figure_persists() {
        let (_d, path) = tmp();
        let mut tally = JobTally::load_for_serving(Some(path.clone()));
        tally.adopt_studio(&studio(&[("video", 3)], Some(day(2))));
        tally.record(TaskKind::Video, day(4));
        let again = JobTally::load_for_serving(Some(path));
        assert_eq!(again.snapshot(day(4)), tally.snapshot(day(4)));
        assert_eq!(
            again.snapshot(day(4)).total,
            counts(&[(TaskKind::Video, 4)])
        );
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
