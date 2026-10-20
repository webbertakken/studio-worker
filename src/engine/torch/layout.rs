//! Where the torch engine keeps everything: under `models_root`, nowhere else
//! (`docs/engines/torch.md`, "Layout").

use std::path::{Path, PathBuf};

use super::recipe::{Recipe, Source, SourceHome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    models_root: PathBuf,
}

impl Layout {
    pub fn new(models_root: impl Into<PathBuf>) -> Self {
        Self {
            models_root: models_root.into(),
        }
    }

    pub fn models_root(&self) -> &Path {
        &self.models_root
    }

    /// `<models_root>/torch`: the engine's own tree.
    pub fn root(&self) -> PathBuf {
        self.models_root.join("torch")
    }

    /// The provisioned `uv`.
    pub fn uv(&self) -> PathBuf {
        self.models_root.join("bin").join("uv")
    }

    /// `UV_PYTHON_INSTALL_DIR`: the interpreters `uv` installs.
    pub fn pythons(&self) -> PathBuf {
        self.root().join("python")
    }

    /// `UV_CACHE_DIR`.
    pub fn uv_cache(&self) -> PathBuf {
        self.root().join("uv-cache")
    }

    /// NVIDIA redist archives, kept so a rebuilt environment downloads nothing again.
    pub fn archives(&self) -> PathBuf {
        self.root().join("archives")
    }

    /// Where a source is checked out.
    pub fn source(&self, source: &Source) -> PathBuf {
        match source.home {
            SourceHome::Sources => {
                self.root()
                    .join("src")
                    .join(format!("{}-{}", source.name, &source.rev[..12]))
            }
            SourceHome::TorchHub(dir) => self.torch_home().join("hub").join(dir),
        }
    }

    /// An environment's directory.
    pub fn environment(&self, recipe: &Recipe) -> PathBuf {
        self.root().join("envs").join(recipe.name)
    }

    /// The runner's Python files.
    pub fn runner(&self) -> PathBuf {
        self.root().join("runner")
    }

    /// Model folders and other files pipelines write once (`docs/engines/torch.md`,
    /// "Gated parts").
    pub fn pipelines(&self) -> PathBuf {
        self.root().join("models")
    }

    /// `XDG_CACHE_HOME` and the extensions' autotune caches.
    pub fn cache(&self) -> PathBuf {
        self.root().join("cache")
    }

    /// Held while provisioning, so one job provisions at a time.
    pub fn lock(&self) -> PathBuf {
        self.root().join("provision.lock")
    }

    /// `HF_HOME`: every Hugging Face weight.
    pub fn hf_home(&self) -> PathBuf {
        self.models_root.join("hf-cache")
    }

    /// `TORCH_HOME`: `torch.hub` code and weights.
    pub fn torch_home(&self) -> PathBuf {
        self.models_root.join("torch-hub")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::torch::recipe::TRELLIS;

    #[test]
    fn everything_lives_under_the_models_root() {
        let layout = Layout::new("/m");
        let nvdiffrast = TRELLIS.source("nvdiffrast").unwrap();
        let naf = TRELLIS.source("NAF").unwrap();
        assert_eq!(layout.uv(), Path::new("/m/bin/uv"));
        assert_eq!(layout.hf_home(), Path::new("/m/hf-cache"));
        assert_eq!(layout.torch_home(), Path::new("/m/torch-hub"));
        assert_eq!(
            layout.environment(&TRELLIS),
            Path::new("/m/torch/envs/trellis")
        );
        assert_eq!(
            layout.source(nvdiffrast),
            Path::new("/m/torch/src/nvdiffrast-253ac4fcea7d")
        );
        assert_eq!(
            layout.source(naf),
            Path::new("/m/torch-hub/hub/valeoai_NAF_main")
        );
        for path in [
            layout.pythons(),
            layout.uv_cache(),
            layout.archives(),
            layout.runner(),
            layout.pipelines(),
            layout.cache(),
            layout.lock(),
        ] {
            assert!(path.starts_with("/m/torch"), "{}", path.display());
        }
        assert_eq!(layout.models_root(), Path::new("/m"));
    }
}
