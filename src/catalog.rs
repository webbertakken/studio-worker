//! Local model catalog — the offline equivalent of the studio's
//! `studioModels` registry.
//!
//! The studio is normally the single source of truth for a model's
//! [`ModelSource`] (which files to download + the CLI defaults). When generating
//! locally there is no studio, so the worker keeps a small JSON catalog the
//! operator can edit and extend exactly the way they would add a model in the
//! studio. It ships seeded with Z-Image-Turbo (the studio's default image
//! model) so a fresh install can generate out of the box.

use std::path::Path;

use serde::{Deserialize, Serialize};

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
}

fn default_true() -> bool {
    true
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

    /// Load the catalog from `path`. If the file does not exist it is seeded
    /// with the built-in defaults and written to `path`.
    pub fn load_or_seed(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Self::from_json(&contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let seeded = Self::seed();
                seeded.save(path)?;
                Ok(seeded)
            }
            Err(err) => Err(err),
        }
    }

    /// Write the catalog to `path` (creating parent dirs).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
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

    /// Remove a model by id. Returns whether it existed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.models.len();
        self.models.retain(|m| m.id != id);
        self.models.len() != before
    }

    /// The first enabled image model — used when a request names no model.
    pub fn default_image_model(&self) -> Option<&CatalogModel> {
        self.models
            .iter()
            .find(|m| m.enabled && m.kind == TaskKind::Image)
    }
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
}
