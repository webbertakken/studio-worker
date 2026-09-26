//! The worker's in-process model loaders, one per engine, behind the
//! model host's `ModelRuntime` (see `docs/runtime/model-lifecycle.md`).

use crate::catalog::CatalogModel;
use crate::host::{LoadedModel, ModelRuntime};
use std::sync::Arc;

/// Dispatches a load to the loader for the model's engine.  An engine
/// with no in-process loader is refused by name, never faked.
#[derive(Default)]
pub struct Loaders;

impl ModelRuntime for Loaders {
    fn load(&self, model: &CatalogModel) -> anyhow::Result<Arc<dyn LoadedModel>> {
        anyhow::bail!(
            "no in-process loader for engine {:?} (model {})",
            model.source.engine,
            model.id
        )
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
        let err = Loaders.load(&model).err().expect("refused");
        assert!(
            err.to_string()
                .contains("no in-process loader for engine SdCpp"),
            "{err}"
        );
    }
}
