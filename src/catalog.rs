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
    /// Device memory the model needs in GiB; admission checks it before a
    /// load or transient job (see `docs/runtime/model-lifecycle.md`).
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
    /// Models sharing a group are loaded one at a time: loading one
    /// unloads the other first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_group: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_origin() -> String {
    "local".into()
}

/// A collection of locally-available models.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub models: Vec<CatalogModel>,
    /// Seed ids the operator deleted; startup seeding never re-adds them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dismissed_seeds: Vec<String>,
}

impl Catalog {
    /// The built-in catalog: every model the worker ships seeded with.
    pub fn seed() -> Self {
        Catalog {
            models: vec![
                zimage_turbo(),
                qwen35_08b(),
                nemotron_stream(),
                parakeet_eou(),
            ],
            dismissed_seeds: Vec::new(),
        }
    }

    /// Add every seed this catalogue lacks (an install that predates it),
    /// never replacing the operator's entry and never re-adding a seed they
    /// deleted.  `true` when anything was added.
    pub fn ensure_seeds(&mut self) -> bool {
        let missing: Vec<CatalogModel> = Self::seed()
            .models
            .into_iter()
            .filter(|m| self.get(&m.id).is_none() && !self.dismissed_seeds.contains(&m.id))
            .collect();
        for model in &missing {
            tracing::info!(target: TRACE_TARGET, op = "seed", model = %model.id, "added a seed model the catalogue lacked");
        }
        let added = !missing.is_empty();
        self.models.extend(missing);
        added
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
                Ok(mut catalog) => {
                    if catalog.ensure_seeds() {
                        catalog.save(path)?;
                    }
                    Ok(catalog)
                }
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
        self.dismissed_seeds.retain(|d| *d != model.id);
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
        let removed = self.models.len() != before;
        let is_seed = Self::seed().models.iter().any(|m| m.id == id);
        if removed && is_seed && !self.dismissed_seeds.iter().any(|d| d == id) {
            self.dismissed_seeds.push(id.to_string());
        }
        removed
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
pub(crate) fn quarantine_path(path: &Path) -> PathBuf {
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

/// A model file on Hugging Face, pinned to a repository revision.
fn hf_file(repo: &str, revision: &str, path: &str, bytes: u64, sha256: &str) -> ModelFile {
    ModelFile {
        role: ModelFileRole::Model,
        url: format!("https://huggingface.co/{repo}/resolve/{revision}/{path}"),
        filename: path.rsplit('/').next().unwrap_or(path).to_string(),
        approx_bytes: Some(bytes),
        sha256: Some(sha256.into()),
    }
}

/// Revision of `altunenes/parakeet-rs` the streaming speech seeds pin.
const PARAKEET_RS_REVISION: &str = "4d2a8bc71f5c896ec40faa59732e6716295edaf2";

/// Qwen3.5 0.8B instruct (unsloth Q8_0): a small chat model for one-shot
/// jobs such as titles and summaries.  Reasoning off (the template thinks
/// only when asked; a small model left to think runs to its token cap);
/// 32K context for long transcripts.  Apache-2.0.
fn qwen35_08b() -> CatalogModel {
    let mut kwargs = serde_json::Map::new();
    kwargs.insert("enable_thinking".into(), serde_json::Value::Bool(false));
    CatalogModel {
        id: "qwen3.5-0.8b".into(),
        display_name: "Qwen3.5 0.8B instruct (Q8_0)".into(),
        kind: TaskKind::Llm,
        vram_gb_estimate: 1.5,
        description: Some("Small chat model: reasoning off, 32K context".into()),
        source: ModelSource {
            engine: ModelEngine::LlamaCpp,
            files: vec![hf_file(
                "unsloth/Qwen3.5-0.8B-GGUF",
                "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0",
                "Qwen3.5-0.8B-Q8_0.gguf",
                811_843_840,
                "0ad885ffd4bb022fc4f0d33a3308fa108ef8613159d3b3a67e23abca056b7a6c",
            )],
            cli_defaults: ModelCliDefaults {
                context_size: Some(32_768),
                chat_template_kwargs: Some(kwargs),
                ..Default::default()
            },
        },
        enabled: true,
        origin: "local".into(),
        exclusive_group: None,
    }
}

/// Nemotron 3.5 streaming ASR (0.6B, multilingual, punctuated): 560 ms
/// chunks.  Measured ~3.5 GiB on CUDA.  NVIDIA Open Model License.
fn nemotron_stream() -> CatalogModel {
    let dir = "nemotron-3.5-asr-streaming-0.6b-onnx";
    let f = |name: &str, bytes: u64, sha: &str| {
        hf_file(
            "altunenes/parakeet-rs",
            PARAKEET_RS_REVISION,
            &format!("{dir}/{name}"),
            bytes,
            sha,
        )
    };
    CatalogModel {
        id: "nemotron-3.5-stream".into(),
        display_name: "Nemotron 3.5 streaming (0.6B, multilingual)".into(),
        kind: TaskKind::AudioStt,
        vram_gb_estimate: 3.5,
        description: Some("Streaming speech-to-text with punctuation".into()),
        source: ModelSource {
            engine: ModelEngine::Parakeet,
            files: vec![
                f(
                    "config.json",
                    2_979,
                    "b0289e196d11a17e3c661bbadfe455c87de4baffc1a5e652a5779f5d687c5db0",
                ),
                f(
                    "decoder_joint.onnx",
                    97_590_054,
                    "634dfadf24cb4f73c2fae170b36611d68db48186426882cbc8f7e02ed9f2bb29",
                ),
                f(
                    "encoder.onnx",
                    42_164_972,
                    "d569fbe78b48fbb04e169d324f5d25463838ceed7b5fc3bfe209872441979bd9",
                ),
                f(
                    "encoder.onnx.data",
                    2_454_405_120,
                    "7584f85df76bc9ae6fbdfa53aa8d97b07a842525d1c501d536d77fd9e4f57ac7",
                ),
                f(
                    "tokenizer.model",
                    406_554,
                    "ce3895e40806f02a26c3a225161b96ef682d6c0054bae32a245dec4258d7d291",
                ),
            ],
            cli_defaults: ModelCliDefaults::default(),
        },
        enabled: true,
        origin: "local".into(),
        exclusive_group: Some("stt".into()),
    }
}

/// Parakeet realtime EOU (120M, English): 160 ms chunks with
/// end-of-utterance detection.  Measured ~1.1 GiB on CUDA.  CC-BY-4.0.
fn parakeet_eou() -> CatalogModel {
    let dir = "realtime_eou_120m-v1-onnx";
    let f = |name: &str, bytes: u64, sha: &str| {
        hf_file(
            "altunenes/parakeet-rs",
            PARAKEET_RS_REVISION,
            &format!("{dir}/{name}"),
            bytes,
            sha,
        )
    };
    CatalogModel {
        id: "parakeet-eou-120m".into(),
        display_name: "Parakeet EOU (120M, English)".into(),
        kind: TaskKind::AudioStt,
        vram_gb_estimate: 1.2,
        description: Some("Light streaming speech-to-text, English".into()),
        source: ModelSource {
            engine: ModelEngine::Parakeet,
            files: vec![
                f(
                    "decoder_joint.onnx",
                    21_347_639,
                    "9d2553ac043c2fc5f69e970769b0fb8ab9103fbfdeb7d26a1ea9729d4bd2dddd",
                ),
                f(
                    "encoder.onnx",
                    459_341_289,
                    "d472887cc38a784a5bfc21c2dbe247639edc3b3f9992388d8ceceaec07256b5b",
                ),
                f(
                    "tokenizer.json",
                    20_053,
                    "f6b0ad8690559351fa478116fe0985a203b76f7c040f3a9381f485c99c0325f8",
                ),
            ],
            cli_defaults: ModelCliDefaults::default(),
        },
        enabled: true,
        origin: "local".into(),
        exclusive_group: Some("stt".into()),
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
                context_size: None,
                chat_template_kwargs: None,
                mmap: None,
                max_vram_gib: None,
            },
        },
        enabled: true,
        origin: "local".into(),
        exclusive_group: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(c: &Catalog) -> Vec<&str> {
        c.models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn the_seed_carries_image_llm_and_both_streaming_speech_models() {
        assert_eq!(
            ids(&Catalog::seed()),
            [
                "z-image-turbo-q4_k_m.gguf",
                "qwen3.5-0.8b",
                "nemotron-3.5-stream",
                "parakeet-eou-120m"
            ]
        );
    }

    #[test]
    fn every_seed_file_is_pinned_and_checksummed() {
        for model in Catalog::seed().models {
            for file in &model.source.files {
                assert!(
                    file.sha256.as_deref().is_some_and(|h| h.len() == 64),
                    "{} {}",
                    model.id,
                    file.filename
                );
                assert!(
                    file.approx_bytes.is_some_and(|b| b > 0),
                    "{} {}",
                    model.id,
                    file.filename
                );
                if model.id != "z-image-turbo-q4_k_m.gguf" {
                    // Z-Image mirrors the studio's seed; the rest pin a revision.
                    assert!(
                        !file.url.contains("/resolve/main/"),
                        "{} pins a revision",
                        file.url
                    );
                }
            }
        }
    }

    #[test]
    fn the_streaming_seeds_swap_with_each_other() {
        let seed = Catalog::seed();
        for id in ["nemotron-3.5-stream", "parakeet-eou-120m"] {
            let m = seed.get(id).unwrap();
            assert_eq!(m.kind, TaskKind::AudioStt);
            assert_eq!(m.source.engine, ModelEngine::Parakeet);
            assert_eq!(m.exclusive_group.as_deref(), Some("stt"));
        }
    }

    #[test]
    fn the_llm_seed_answers_without_reasoning_in_a_long_context() {
        let seed = Catalog::seed();
        let m = seed.get("qwen3.5-0.8b").unwrap();
        assert_eq!(m.kind, TaskKind::Llm);
        assert_eq!(m.source.engine, ModelEngine::LlamaCpp);
        assert_eq!(m.source.cli_defaults.context_size, Some(32768));
        assert_eq!(
            m.source.cli_defaults.chat_template_kwargs.as_ref().unwrap()["enable_thinking"],
            false
        );
    }

    #[test]
    fn missing_seeds_are_added_to_an_existing_catalogue() {
        let mut c = Catalog {
            models: vec![zimage_turbo()],
            ..Default::default()
        };
        assert!(c.ensure_seeds());
        assert_eq!(ids(&c), ids(&Catalog::seed()));
        assert!(!c.ensure_seeds(), "idempotent");
    }

    #[test]
    fn seeding_never_overwrites_the_operators_entry() {
        let mut mine = Catalog::seed().get("qwen3.5-0.8b").unwrap().clone();
        mine.display_name = "my tuned qwen".into();
        let mut c = Catalog {
            models: vec![mine],
            ..Default::default()
        };
        c.ensure_seeds();
        assert_eq!(c.get("qwen3.5-0.8b").unwrap().display_name, "my tuned qwen");
    }

    #[test]
    fn a_deleted_seed_stays_deleted() {
        let mut c = Catalog::seed();
        assert!(c.remove("parakeet-eou-120m"));
        assert_eq!(c.dismissed_seeds, ["parakeet-eou-120m"]);
        assert!(!c.ensure_seeds());
        assert!(c.get("parakeet-eou-120m").is_none());
        let reloaded = Catalog::from_json(&c.to_json().unwrap()).unwrap();
        assert_eq!(reloaded.dismissed_seeds, ["parakeet-eou-120m"]);
    }

    #[test]
    fn re_adding_a_dismissed_seed_forgets_the_dismissal() {
        let mut c = Catalog::seed();
        let eou = c.get("parakeet-eou-120m").unwrap().clone();
        c.remove("parakeet-eou-120m");
        c.upsert(eou);
        assert!(c.dismissed_seeds.is_empty());
    }

    #[test]
    fn removing_a_non_seed_records_nothing() {
        let mut c = Catalog::seed();
        c.upsert(studio_model("mine"));
        c.remove("mine");
        assert!(c.dismissed_seeds.is_empty());
    }

    #[test]
    fn loading_an_older_catalogue_adds_the_new_seeds_and_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let old = Catalog {
            models: vec![zimage_turbo()],
            ..Default::default()
        };
        std::fs::write(&path, old.to_json().unwrap()).unwrap();
        let loaded = Catalog::load_or_seed(&path).unwrap();
        assert_eq!(ids(&loaded), ids(&Catalog::seed()));
        let on_disk = Catalog::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(ids(&on_disk), ids(&Catalog::seed()));
    }

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

        // Read the bytes back raw: load_or_seed would top up the seeds.
        let reloaded = Catalog::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
