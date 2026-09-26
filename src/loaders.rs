//! The worker's in-process model loaders, one per engine, behind the
//! model host's `ModelRuntime` (see `docs/runtime/model-lifecycle.md`).

use crate::catalog::CatalogModel;
use crate::host::{LoadedModel, ModelRuntime};
use std::path::PathBuf;
use std::sync::Arc;

/// Dispatches a load to the loader for the model's engine.  An engine
/// with no in-process loader is refused by name, never faked.
pub struct Loaders {
    models_root: PathBuf,
}

impl Loaders {
    /// `models_root`: where model files are downloaded to.
    pub fn new(models_root: PathBuf) -> Self {
        Self { models_root }
    }
}

impl ModelRuntime for Loaders {
    fn load(&self, model: &CatalogModel) -> anyhow::Result<Arc<dyn LoadedModel>> {
        match &model.source.engine {
            #[cfg(all(feature = "llama", not(target_os = "windows")))]
            crate::types::ModelEngine::LlamaCpp => Ok(Arc::new(
                crate::engine::llama::load_resident(&self.models_root, model)?,
            )),
            #[cfg(feature = "stt-stream")]
            crate::types::ModelEngine::Parakeet => Ok(Arc::new(
                crate::engine::parakeet::load_resident(&self.models_root, model)?,
            )),
            engine => {
                let _ = &self.models_root;
                anyhow::bail!(
                    "no in-process loader for engine {engine:?} (model {})",
                    model.id
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelEngine, ModelSource, TaskKind};

    #[test]
    fn an_engine_without_a_loader_is_refused_by_name() {
        let model = CatalogModel {
            id: "m".into(),
            display_name: "m".into(),
            kind: TaskKind::Image,
            vram_gb_estimate: 1.0,
            description: None,
            source: ModelSource {
                engine: ModelEngine::SdCpp,
                files: vec![],
                cli_defaults: Default::default(),
            },
            enabled: true,
            origin: "local".into(),
            exclusive_group: None,
        };
        let err = Loaders::new(PathBuf::from("/nonexistent"))
            .load(&model)
            .err()
            .expect("refused");
        assert!(
            err.to_string()
                .contains("no in-process loader for engine SdCpp"),
            "{err}"
        );
    }
}
