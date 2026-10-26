//! Where takes live: `<config dir>/voice-samples/<set>/<kind>/<take>.wav`, owner-only
//! (`docs/engines/wake-word.md`, "Where takes live").  One lock orders every write, delete and
//! hold, so nothing slips between a training's check of its sets and its hold on them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use super::take::{self, check_wav, Kind, Take};

/// Where the store lives, beside the active config file (as every piece of worker state).
pub fn dir_for(config_path: &Path) -> Option<PathBuf> {
    config_path.parent().map(|dir| dir.join("voice-samples"))
}

/// A stored take, as a training reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeFile {
    pub set: String,
    pub kind: Kind,
    pub number: u32,
    pub phrase: Option<String>,
    #[serde(skip)]
    pub path: PathBuf,
    pub sha256: String,
}

/// What an upload did.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    pub created: bool,
    pub seconds: f64,
    pub bytes: u64,
}

/// One set, as `GET /voice-samples` lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSummary {
    pub set: String,
    pub positive: u32,
    pub near_miss: BTreeMap<String, u32>,
    pub speech: u32,
    pub seconds: f64,
    pub bytes: u64,
    pub updated_at: Option<DateTime<Utc>>,
    pub in_training: bool,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    BadWav(String),
    #[error("a training holds set {0}")]
    InTraining(String),
    #[error("no set named {0}")]
    UnknownSet(String),
    #[error("{0}")]
    Io(String),
}

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    /// Sets a training holds, and how many trainings hold each.
    held: Mutex<HashMap<String, usize>>,
}

/// The worker's recording sets.
#[derive(Debug, Clone)]
pub struct VoiceSamples {
    inner: Arc<Inner>,
}

/// A training's hold on its sets: their takes, and no upload or delete until it drops.
#[derive(Debug)]
pub struct Held {
    pub takes: Vec<TakeFile>,
    sets: Vec<String>,
    store: VoiceSamples,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut held = self.store.inner.held.lock();
        for set in &self.sets {
            if let Some(count) = held.get_mut(set) {
                *count -= 1;
                if *count == 0 {
                    held.remove(set);
                }
            }
        }
    }
}

fn io(what: &str) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |e| StoreError::Io(format!("{what}: {e}"))
}

/// Create `dir` (and its parents up to `root`) owner-only.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to `path` owner-only, through a partial file so a reader never sees half a take.
fn private_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let partial = path.with_extension("partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&partial)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&partial, path)
}

impl VoiceSamples {
    pub fn new(root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                held: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    fn set_dir(&self, set: &str) -> PathBuf {
        self.inner.root.join(set)
    }

    /// Store one take of `set`, replacing the same take; the audio is checked first.
    pub fn put(&self, set: &str, take: &Take, bytes: &[u8]) -> Result<Stored, StoreError> {
        let audio = check_wav(bytes).map_err(StoreError::BadWav)?;
        let held = self.inner.held.lock();
        if held.contains_key(set) {
            return Err(StoreError::InTraining(set.into()));
        }
        let dir = self.set_dir(set).join(take.kind.as_str());
        private_dir(&self.inner.root).map_err(io("creating the store"))?;
        private_dir(&self.set_dir(set)).map_err(io("creating the set"))?;
        private_dir(&dir).map_err(io("creating the kind"))?;
        let path = dir.join(format!("{}.wav", take.stem()));
        let previous = std::fs::read(&path).ok();
        if previous.as_deref() != Some(bytes) {
            private_write(&path, bytes).map_err(io("writing the take"))?;
        }
        drop(held);
        Ok(Stored {
            created: previous.is_none(),
            seconds: audio.seconds,
            bytes: bytes.len() as u64,
        })
    }

    /// Every take of `set`, in order of kind, phrase and number.  Files that are not takes are
    /// skipped (a partial write left by a crash, anything put there by hand).
    fn takes_of(&self, set: &str) -> Result<Vec<TakeFile>, StoreError> {
        let mut takes = Vec::new();
        for kind in Kind::ALL {
            let dir = self.set_dir(set).join(kind.as_str());
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StoreError::Io(format!("reading {}: {e}", dir.display()))),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(stem) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".wav"))
                else {
                    continue;
                };
                let Ok(take) = take::parse_take(kind, stem) else {
                    continue;
                };
                takes.push(TakeFile {
                    set: set.into(),
                    kind,
                    number: take.number,
                    phrase: take.phrase,
                    path,
                    sha256: String::new(),
                });
            }
        }
        takes.sort_by(|a, b| (a.kind, &a.phrase, a.number).cmp(&(b.kind, &b.phrase, b.number)));
        Ok(takes)
    }

    fn set_names(&self) -> Result<Vec<String>, StoreError> {
        let entries = match std::fs::read_dir(&self.inner.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(format!("reading the store: {e}"))),
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|name| take::check_set(name).is_ok())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Every set with its counts, newest first.
    pub fn list(&self) -> Result<Vec<SetSummary>, StoreError> {
        let held = self.inner.held.lock().clone();
        let mut sets = Vec::new();
        for set in self.set_names()? {
            let mut summary = SetSummary {
                in_training: held.contains_key(&set),
                set: set.clone(),
                positive: 0,
                near_miss: BTreeMap::new(),
                speech: 0,
                seconds: 0.0,
                bytes: 0,
                updated_at: None,
            };
            for file in self.takes_of(&set)? {
                match file.kind {
                    Kind::Positive => summary.positive += 1,
                    Kind::Speech => summary.speech += 1,
                    Kind::NearMiss => {
                        *summary
                            .near_miss
                            .entry(file.phrase.clone().unwrap_or_default())
                            .or_default() += 1
                    }
                }
                if let Ok(bytes) = std::fs::read(&file.path) {
                    summary.bytes += bytes.len() as u64;
                    summary.seconds += check_wav(&bytes).map(|a| a.seconds).unwrap_or(0.0);
                }
                let modified = std::fs::metadata(&file.path)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(DateTime::<Utc>::from);
                summary.updated_at = summary.updated_at.max(modified);
            }
            sets.push(summary);
        }
        sets.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.set.cmp(&b.set)));
        Ok(sets)
    }

    /// Delete `set`, or every set; a set a training holds stops the whole delete.
    pub fn delete(&self, set: Option<&str>) -> Result<Vec<String>, StoreError> {
        let held = self.inner.held.lock();
        let sets = match set {
            Some(set) => {
                if !self.set_dir(set).is_dir() {
                    return Err(StoreError::UnknownSet(set.into()));
                }
                vec![set.to_string()]
            }
            None => self.set_names()?,
        };
        if let Some(busy) = sets.iter().find(|s| held.contains_key(*s)) {
            return Err(StoreError::InTraining(busy.clone()));
        }
        for set in &sets {
            std::fs::remove_dir_all(self.set_dir(set)).map_err(io("deleting the set"))?;
        }
        Ok(sets)
    }

    /// Hold `sets` for a training: their takes, each with its sha256; no upload to them or delete
    /// of them until the hold drops.
    pub fn hold(&self, sets: &[String]) -> Result<Held, StoreError> {
        use sha2::{Digest, Sha256};
        let mut held = self.inner.held.lock();
        let mut takes = Vec::new();
        for set in sets {
            if !self.set_dir(set).is_dir() {
                return Err(StoreError::UnknownSet(set.clone()));
            }
            for mut file in self.takes_of(set)? {
                let bytes = std::fs::read(&file.path).map_err(io("reading a take"))?;
                file.sha256 = hex::encode(Sha256::digest(&bytes));
                takes.push(file);
            }
        }
        for set in sets {
            *held.entry(set.clone()).or_default() += 1;
        }
        Ok(Held {
            takes,
            sets: sets.to_vec(),
            store: self.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_samples::take::tests::wav;

    fn take(kind: Kind, number: u32, phrase: Option<&str>) -> Take {
        Take {
            kind,
            number,
            phrase: phrase.map(str::to_string),
        }
    }

    fn store() -> (tempfile::TempDir, VoiceSamples) {
        let dir = tempfile::tempdir().unwrap();
        let store = VoiceSamples::new(dir.path().join("voice-samples"));
        (dir, store)
    }

    #[test]
    fn a_take_is_stored_once_and_a_redo_replaces_it() {
        let (_dir, store) = store();
        let first = store
            .put("s1", &take(Kind::Positive, 7, None), &wav(2.0))
            .unwrap();
        assert_eq!(
            first,
            Stored {
                created: true,
                seconds: 2.0,
                bytes: wav(2.0).len() as u64
            }
        );
        let again = store
            .put("s1", &take(Kind::Positive, 7, None), &wav(2.0))
            .unwrap();
        assert!(!again.created);
        let redo = store
            .put("s1", &take(Kind::Positive, 7, None), &wav(1.5))
            .unwrap();
        assert_eq!((redo.created, redo.seconds), (false, 1.5));
        let path = store.root().join("s1/positive/007.wav");
        assert_eq!(std::fs::read(path).unwrap(), wav(1.5));
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, store) = store();
        store
            .put("s1", &take(Kind::NearMiss, 2, Some("hey_luna")), &wav(2.0))
            .unwrap();
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.root().to_path_buf()), 0o700);
        assert_eq!(mode(store.root().join("s1")), 0o700);
        assert_eq!(mode(store.root().join("s1/near_miss")), 0o700);
        assert_eq!(
            mode(store.root().join("s1/near_miss/hey_luna-002.wav")),
            0o600
        );
    }

    #[test]
    fn audio_that_is_not_a_take_is_never_stored() {
        let (_dir, store) = store();
        let error = store
            .put("s1", &take(Kind::Speech, 1, None), b"RIFF")
            .unwrap_err();
        assert!(matches!(error, StoreError::BadWav(_)));
        assert!(!store.root().exists());
    }

    #[test]
    fn sets_are_listed_with_their_counts() {
        let (_dir, store) = store();
        for n in 1..=5 {
            store
                .put("s1", &take(Kind::Positive, n, None), &wav(2.0))
                .unwrap();
        }
        store
            .put("s1", &take(Kind::NearMiss, 1, Some("hey_luna")), &wav(2.0))
            .unwrap();
        store
            .put("s1", &take(Kind::NearMiss, 2, Some("hey_luna")), &wav(2.0))
            .unwrap();
        store
            .put("s1", &take(Kind::Speech, 1, None), &wav(30.0))
            .unwrap();
        std::fs::write(store.root().join("s1/positive/stray.txt"), b"x").unwrap();
        std::fs::create_dir_all(store.root().join("Not A Set")).unwrap();
        let sets = store.list().unwrap();
        assert_eq!(sets.len(), 1);
        let s1 = &sets[0];
        assert_eq!((s1.positive, s1.speech), (5, 1));
        assert_eq!(s1.near_miss, BTreeMap::from([("hey_luna".to_string(), 2)]));
        assert_eq!(s1.seconds, 44.0);
        assert!(s1.updated_at.is_some() && !s1.in_training);
        assert!(VoiceSamples::new(store.root().join("nowhere"))
            .list()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_held_set_takes_no_upload_and_no_delete_until_released() {
        let (_dir, store) = store();
        store
            .put("s1", &take(Kind::Positive, 1, None), &wav(2.0))
            .unwrap();
        store
            .put("s2", &take(Kind::Positive, 1, None), &wav(2.0))
            .unwrap();
        let held = store.hold(&["s1".to_string()]).unwrap();
        assert_eq!(held.takes.len(), 1);
        assert_eq!(held.takes[0].sha256.len(), 64);
        let refused = store
            .put("s1", &take(Kind::Positive, 2, None), &wav(2.0))
            .unwrap_err();
        assert_eq!(refused, StoreError::InTraining("s1".into()));
        assert_eq!(
            store.delete(Some("s1")),
            Err(StoreError::InTraining("s1".into()))
        );
        assert_eq!(store.delete(None), Err(StoreError::InTraining("s1".into())));
        assert!(store
            .list()
            .unwrap()
            .iter()
            .any(|s| s.set == "s1" && s.in_training));
        store
            .put("s2", &take(Kind::Positive, 2, None), &wav(2.0))
            .unwrap();
        drop(held);
        store
            .put("s1", &take(Kind::Positive, 2, None), &wav(2.0))
            .unwrap();
        assert_eq!(store.delete(None).unwrap(), ["s1", "s2"]);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn unknown_sets_are_named() {
        let (_dir, store) = store();
        assert_eq!(
            store.delete(Some("s9")),
            Err(StoreError::UnknownSet("s9".into()))
        );
        assert_eq!(
            store.hold(&["s9".into()]).unwrap_err(),
            StoreError::UnknownSet("s9".into())
        );
        assert_eq!(store.delete(None).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_hold_lists_takes_in_order() {
        let (_dir, store) = store();
        for (kind, n, phrase) in [
            (Kind::Speech, 1, None),
            (Kind::Positive, 12, None),
            (Kind::Positive, 3, None),
            (Kind::NearMiss, 1, Some("runner")),
            (Kind::NearMiss, 1, Some("hey_luna")),
        ] {
            store.put("s1", &take(kind, n, phrase), &wav(1.0)).unwrap();
        }
        let held = store.hold(&["s1".into()]).unwrap();
        let order: Vec<String> = held
            .takes
            .iter()
            .map(|t| {
                format!(
                    "{}/{}",
                    t.kind.as_str(),
                    Take {
                        kind: t.kind,
                        number: t.number,
                        phrase: t.phrase.clone()
                    }
                    .stem()
                )
            })
            .collect();
        assert_eq!(
            order,
            [
                "positive/003",
                "positive/012",
                "near_miss/hey_luna-001",
                "near_miss/runner-001",
                "speech/001"
            ]
        );
        assert_eq!(
            dir_for(Path::new("/c/config.toml")),
            Some(PathBuf::from("/c/voice-samples"))
        );
    }
}
