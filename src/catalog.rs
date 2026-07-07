//! Local model catalog — the offline equivalent of the studio's
//! `studioModels` registry.
//!
//! The studio is normally the single source of truth for a model's
//! [`ModelSource`] (which files to download + the CLI defaults). When generating
//! locally there is no studio, so the worker keeps a small JSON catalog the
//! operator can edit and extend exactly the way they would add a model in the
//! studio. It ships seeded with Z-Image-Turbo (the studio's default image
//! model) so a fresh install can generate out of the box.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Tracing target for catalog persistence.  Stable so operators can
/// filter with `RUST_LOG=studio_worker::catalog=debug`.
const TRACE_TARGET: &str = "studio_worker::catalog";

use crate::types::{
    ModelCliDefaults, ModelEngine, ModelFile, ModelFileRole, ModelSource, TaskKind,
};

/// One catalog entry: a model id plus everything needed to run it. Mirrors the
/// columns of the studio's `studioModels` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    /// The model id the operator references (e.g. `z-image-turbo-q4_k_m.gguf`).
    pub id: String,
    /// Human-readable name shown in the UI.
    pub display_name: String,
    /// Task kind this model serves.
    pub kind: TaskKind,
    /// Rough VRAM requirement in GB (informational).
    #[serde(default)]
    pub vram_gb_estimate: f32,
    /// Optional human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Download spec + engine + CLI defaults (same shape the studio sends).
    pub source: ModelSource,
    /// Whether the model is selectable.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Where this entry came from: `"local"` (operator-added / seeded)
    /// or `"studio"` (mirrored from a studio job offer).  A studio
    /// re-offer refreshes studio-origin entries; a local-origin entry
    /// of the same id is never clobbered by the sync.
    #[serde(default = "default_origin")]
    pub origin: String,
}

fn default_true() -> bool {
    true
}
fn default_origin() -> String {
    "local".into()
}

/// A collection of locally-available models.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub models: Vec<CatalogModel>,
}

impl Catalog {
    /// The built-in catalog: every model the worker ships seeded with.
    pub fn seed() -> Self {
        Catalog {
            models: vec![zimage_turbo()],
        }
    }

    /// Parse a catalog from a JSON string.
    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Serialise to pretty JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Load the catalog from `path`.
    ///
    /// * Missing file → seeded with the built-in defaults and written.
    /// * Corrupt JSON → the file is **quarantined** (renamed to
    ///   `models.json.corrupt-<unix-ts>`) and a fresh seed written in
    ///   its place.  The old behaviour — erroring so the caller fell
    ///   back to an in-memory seed while keeping the save path — meant
    ///   the next persist silently overwrote the operator's hand-edited
    ///   catalog; quarantining preserves their bytes for recovery.
    /// * Any other IO error propagates (nothing is renamed or written).
    pub fn load_or_seed(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => match Self::from_json(&contents) {
                Ok(catalog) => Ok(catalog),
                Err(parse_err) => {
                    let quarantine = quarantine_path(path);
                    std::fs::rename(path, &quarantine)?;
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "load",
                        path = %path.display(),
                        quarantine = %quarantine.display(),
                        error = %parse_err,
                        "catalog is not valid JSON; quarantined the file and reseeded"
                    );
                    let seeded = Self::seed();
                    seeded.save(path)?;
                    Ok(seeded)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let seeded = Self::seed();
                seeded.save(path)?;
                Ok(seeded)
            }
            Err(err) => Err(err),
        }
    }

    /// Load for serving: the catalog plus the path future saves may
    /// write to.  A quarantine/seed recovery keeps the path (the file
    /// is now healthy); an unreadable file (permissions, IO) drops it
    /// so the worker can never overwrite a file it couldn't read.
    pub fn load_for_serving(path: Option<PathBuf>) -> (Self, Option<PathBuf>) {
        match path {
            Some(path) => match Self::load_or_seed(&path) {
                Ok(catalog) => (catalog, Some(path)),
                Err(err) => {
                    tracing::warn!(
                        target: TRACE_TARGET,
                        op = "load",
                        path = %path.display(),
                        error = %err,
                        "catalog unreadable; serving the in-memory seed and \
                         disabling persistence so the file is never clobbered"
                    );
                    (Self::seed(), None)
                }
            },
            None => (Self::seed(), None),
        }
    }

    /// Write the catalog to `path` (creating parent dirs) — atomically,
    /// via the same temp-file + rename dance as `config.toml`, so a
    /// crash mid-write can't truncate the operator's model catalog.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::config::write_atomic(path, json.as_bytes()).map_err(std::io::Error::other)
    }

    /// Look up a model by id.
    pub fn get(&self, id: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|m| m.id == id)
    }

    /// All catalog entries.
    pub fn list(&self) -> &[CatalogModel] {
        &self.models
    }

    /// Insert a model, replacing any existing entry with the same id.
    pub fn upsert(&mut self, model: CatalogModel) {
        if let Some(existing) = self.models.iter_mut().find(|m| m.id == model.id) {
            *existing = model;
        } else {
            self.models.push(model);
        }
    }

    /// Mirror a model seen on a studio job offer into the catalog so
    /// the local API can serve it too.  Returns whether the catalog
    /// changed (a no-op returns `false`, so the caller can skip the
    /// disk write).  A **local-origin** entry of the same id is never
    /// clobbered — the operator's own edits win; an unchanged
    /// studio-origin entry is left alone so a re-offer every job
    /// doesn't churn the file.
    pub fn sync_studio_model(&mut self, incoming: CatalogModel) -> bool {
        if let Some(existing) = self.models.iter_mut().find(|m| m.id == incoming.id) {
            if existing.origin == "local" {
                return false; // never overwrite operator-owned entries
            }
            if *existing == incoming {
                return false; // already up to date
            }
            *existing = incoming;
            return true;
        }
        self.models.push(incoming);
        true
    }

    /// Remove a model by id. Returns whether it existed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.models.len();
        self.models.retain(|m| m.id != id);
        self.models.len() != before
    }

    /// The first enabled image model — used when a request names no model.
    pub fn default_image_model(&self) -> Option<&CatalogModel> {
        self.default_model_for(TaskKind::Image)
    }

    /// The first enabled model of `kind` — used when a request for that
    /// kind names no explicit model.  Generalises
    /// [`default_image_model`](Self::default_image_model) so the local
    /// API can serve every modality the worker's engines support.
    pub fn default_model_for(&self, kind: TaskKind) -> Option<&CatalogModel> {
        self.models.iter().find(|m| m.enabled && m.kind == kind)
    }
}

/// Where a corrupt catalog gets parked: `<name>.corrupt-<unix-ts>`,
/// beside the original so the operator can recover their edits.
fn quarantine_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "models.json".to_string());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    path.with_file_name(format!("{name}.corrupt-{ts}"))
}

/// The canonical Z-Image-Turbo entry, mirroring the studio seed
/// (`migrations/graphics/0017_seed_registry.sql`).
fn zimage_turbo() -> CatalogModel {
    CatalogModel {
        id: "z-image-turbo-q4_k_m.gguf".into(),
        display_name: "Z-Image Turbo (Q4_K_M)".into(),
        kind: TaskKind::Image,
        vram_gb_estimate: 12.0,
        description: Some(
            "Distilled 8-step diffusion model packaged for sd.cpp. Diffusion (Q4_K), \
             Qwen3-4B text encoder, Flux ae.safetensors VAE."
                .into(),
        ),
        source: ModelSource {
            engine: ModelEngine::SdCpp,
            files: vec![
                // sha256 pins sourced from the HF LFS oids
                // (`/api/models/<repo>/tree/main`) and cross-checked
                // against freshly downloaded copies — the out-of-the-box
                // model must never be swappable in transit or at rest.
                ModelFile {
                    role: ModelFileRole::DiffusionModel,
                    url: "https://huggingface.co/leejet/Z-Image-Turbo-GGUF/resolve/main/z_image_turbo-Q4_K.gguf".into(),
                    filename: "z_image_turbo-Q4_K.gguf".into(),
                    approx_bytes: Some(3_864_250_304),
                    sha256: Some(
                        "14b375ab4f226bc5378f68f37e899ef3c2242b8541e61e2bc1aff40976086fbd".into(),
                    ),
                },
                ModelFile {
                    role: ModelFileRole::TextEncoder,
                    url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf".into(),
                    filename: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf".into(),
                    approx_bytes: Some(2_497_281_120),
                    sha256: Some(
                        "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597".into(),
                    ),
                },
                ModelFile {
                    role: ModelFileRole::Vae,
                    url: "https://huggingface.co/Comfy-Org/Lumina_Image_2.0_Repackaged/resolve/main/split_files/vae/ae.safetensors".into(),
                    filename: "ae.safetensors".into(),
                    approx_bytes: Some(335_304_388),
                    sha256: Some(
                        "afc8e28272cd15db3919bacdb6918ce9c1ed22e96cb12c4d5ed0fba823529e38".into(),
                    ),
                },
            ],
            cli_defaults: ModelCliDefaults {
                cfg_scale: 1.0,
                steps: 8,
                width: 1024,
                height: 1024,
                sampling_method: Some("euler".into()),
                flow_shift: None,
                zero_cond_t: None,
                offload_to_cpu: None,
            },
        },
        enabled: true,
        origin: "local".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_contains_zimage_with_three_files() {
        let catalog = Catalog::seed();
        let model = catalog
            .get("z-image-turbo-q4_k_m.gguf")
            .expect("z-image seeded");
        assert_eq!(model.kind, TaskKind::Image);
        assert_eq!(model.source.engine, ModelEngine::SdCpp);
        assert_eq!(model.source.files.len(), 3);
        assert_eq!(model.source.cli_defaults.steps, 8);
        assert!(model.enabled);
    }

    #[test]
    fn every_seeded_file_is_https_and_integrity_pinned() {
        // The out-of-the-box downloads must be tamper-evident: each
        // file carries a 64-hex sha256 and a true byte count (used by
        // the disk-space preflight), served over https.
        for model in Catalog::seed().list() {
            for file in &model.source.files {
                assert!(
                    file.url.starts_with("https://"),
                    "{} must be https",
                    file.url
                );
                let sha = file
                    .sha256
                    .as_deref()
                    .unwrap_or_else(|| panic!("{} has no sha256 pin", file.filename));
                assert_eq!(sha.len(), 64, "{} pin must be 64 hex", file.filename);
                assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
                assert!(
                    file.approx_bytes.unwrap_or(0) > 0,
                    "{} needs a real approx_bytes for the disk preflight",
                    file.filename
                );
            }
        }
    }

    #[test]
    fn default_image_model_is_zimage() {
        let catalog = Catalog::seed();
        assert_eq!(
            catalog.default_image_model().map(|m| m.id.as_str()),
            Some("z-image-turbo-q4_k_m.gguf")
        );
    }

    #[test]
    fn json_round_trips() {
        let catalog = Catalog::seed();
        let json = catalog.to_json().unwrap();
        // camelCase wire keys, mirroring the studio.
        assert!(json.contains("\"displayName\""));
        assert!(json.contains("\"cliDefaults\""));
        assert!(json.contains("\"diffusion-model\""));
        let parsed = Catalog::from_json(&json).unwrap();
        assert_eq!(parsed, catalog);
    }

    #[test]
    fn upsert_adds_then_replaces() {
        let mut catalog = Catalog::default();
        let mut model = zimage_turbo();
        catalog.upsert(model.clone());
        assert_eq!(catalog.list().len(), 1);

        model.display_name = "Renamed".into();
        catalog.upsert(model);
        assert_eq!(catalog.list().len(), 1);
        assert_eq!(
            catalog
                .get("z-image-turbo-q4_k_m.gguf")
                .unwrap()
                .display_name,
            "Renamed"
        );
    }

    #[test]
    fn remove_reports_presence() {
        let mut catalog = Catalog::seed();
        assert!(catalog.remove("z-image-turbo-q4_k_m.gguf"));
        assert!(!catalog.remove("z-image-turbo-q4_k_m.gguf"));
        assert!(catalog.get("z-image-turbo-q4_k_m.gguf").is_none());
    }

    #[test]
    fn load_or_seed_writes_then_reads_back() {
        let dir = std::env::temp_dir().join(format!("sw-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("models.json");

        // Missing -> seeded + persisted.
        let seeded = Catalog::load_or_seed(&path).unwrap();
        assert!(path.exists());
        assert!(seeded.get("z-image-turbo-q4_k_m.gguf").is_some());

        // Existing -> read back unchanged.
        let reloaded = Catalog::load_or_seed(&path).unwrap();
        assert_eq!(reloaded, seeded);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn equality_derives_hold_for_catalog_model() {
        assert_eq!(zimage_turbo(), zimage_turbo());
    }

    // -----------------------------------------------------------------
    // Persistence safety: atomic writes + corrupt-file quarantine.
    // -----------------------------------------------------------------

    #[test]
    fn save_atomically_replaces_without_temp_litter() {
        // A second save must fully replace the file and leave no
        // temp-file siblings from the write-then-rename dance — a crash
        // mid-write must never truncate the operator's catalog.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        Catalog::seed().save(&path).unwrap();
        let mut small = Catalog::default();
        small.upsert(CatalogModel {
            description: None,
            ..zimage_turbo()
        });
        small.save(&path).unwrap();

        let reloaded = Catalog::load_or_seed(&path).unwrap();
        assert_eq!(reloaded, small);

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["models.json".to_string()],
            "atomic save must leave only the target file, found: {names:?}"
        );
    }

    #[test]
    fn corrupt_catalog_is_quarantined_not_overwritten() {
        // The exact data-loss shape this guards against: a corrupt
        // models.json used to make the caller fall back to the seed
        // while keeping the save path — the next persist silently
        // destroyed the operator's hand-edited catalog.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let operator_bytes = b"{ this is my hand-edited catalog, now corrupt";
        std::fs::write(&path, operator_bytes).unwrap();

        let logs = crate::test_support::capture({
            let path = path.clone();
            move || {
                let recovered = Catalog::load_or_seed(&path).unwrap();
                assert_eq!(recovered, Catalog::seed(), "reseeded in place");
            }
        });

        // The original bytes survive in a quarantine sibling.
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("models.json.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
        assert_eq!(
            std::fs::read(dir.path().join(&quarantined[0])).unwrap(),
            operator_bytes,
            "the operator's bytes must survive verbatim"
        );
        // The live path now holds a healthy seed.
        assert_eq!(Catalog::load_or_seed(&path).unwrap(), Catalog::seed());
        // And the recovery left a breadcrumb naming both paths.
        assert!(logs.contains("quarantined"), "got: {logs}");
        assert!(logs.contains("models.json.corrupt-"), "got: {logs}");
    }

    #[test]
    fn load_for_serving_keeps_the_path_after_quarantine_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, b"not json").unwrap();
        let (catalog, save_path) = Catalog::load_for_serving(Some(path.clone()));
        assert_eq!(catalog, Catalog::seed());
        assert_eq!(
            save_path,
            Some(path),
            "a quarantined-and-reseeded file is healthy; persistence stays on"
        );
    }

    #[test]
    fn load_for_serving_disables_persistence_when_the_file_is_unreadable() {
        // A directory where the file should be makes the read fail with
        // a non-NotFound error on every platform.  The worker must
        // serve the seed but never gain a path it could clobber.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::create_dir(&path).unwrap();
        let (catalog, save_path) = Catalog::load_for_serving(Some(path));
        assert_eq!(catalog, Catalog::seed());
        assert_eq!(
            save_path, None,
            "an unreadable catalog must not be writable"
        );
    }

    #[test]
    fn load_for_serving_without_a_path_serves_the_seed() {
        let (catalog, save_path) = Catalog::load_for_serving(None);
        assert_eq!(catalog, Catalog::seed());
        assert_eq!(save_path, None);
    }

    // -----------------------------------------------------------------
    // sync_studio_model — mirror studio-offered models into the catalog
    // without clobbering the operator's own entries.
    // -----------------------------------------------------------------

    /// Clone a model with a different id (test helper).
    fn with_id(mut m: CatalogModel, id: &str) -> CatalogModel {
        m.id = id.to_string();
        m
    }

    fn studio_model(id: &str) -> CatalogModel {
        with_id(
            CatalogModel {
                origin: "studio".into(),
                ..zimage_turbo()
            },
            id,
        )
    }

    #[test]
    fn sync_adds_a_new_studio_model_and_reports_change() {
        let mut cat = Catalog::default();
        assert!(cat.sync_studio_model(studio_model("m1")));
        assert_eq!(cat.get("m1").unwrap().origin, "studio");
        // Re-syncing the identical model is a no-op (no file churn).
        assert!(!cat.sync_studio_model(studio_model("m1")));
    }

    #[test]
    fn sync_refreshes_a_changed_studio_model() {
        let mut cat = Catalog::default();
        cat.sync_studio_model(studio_model("m1"));
        let mut updated = studio_model("m1");
        updated.display_name = "Renamed by studio".into();
        assert!(cat.sync_studio_model(updated));
        assert_eq!(cat.get("m1").unwrap().display_name, "Renamed by studio");
    }

    #[test]
    fn sync_never_clobbers_a_local_origin_entry() {
        let mut cat = Catalog::default();
        let mut local = with_id(zimage_turbo(), "m1");
        local.display_name = "my hand-tuned model".into();
        // origin defaults to "local".
        cat.upsert(local);
        // A studio offer for the same id must not overwrite it.
        assert!(!cat.sync_studio_model(studio_model("m1")));
        assert_eq!(cat.get("m1").unwrap().display_name, "my hand-tuned model");
        assert_eq!(cat.get("m1").unwrap().origin, "local");
    }
}
