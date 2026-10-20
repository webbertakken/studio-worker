//! Environments as data: what each Python environment is built from (`docs/engines/torch.md`,
//! "Environments").  Every version is pinned; `DECISIONS.md` ("Image to 3D") says why each pin is
//! the one it is.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::platform::ComputeCapability;

/// Bumped when the way a recipe is built changes, so every environment is rebuilt once.
const RECIPE_FORMAT: u32 = 1;

/// A git source checked out at a pinned revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Source {
    /// The checkout's name, and how pipelines and builds refer to it.
    pub name: &'static str,
    pub repo: &'static str,
    /// A full commit hash.
    pub rev: &'static str,
    /// Check out its git submodules too.
    pub submodules: bool,
    /// Where it lives.
    pub home: SourceHome,
}

/// Where a source is checked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SourceHome {
    /// `<torch>/src/<name>-<rev>`.
    Sources,
    /// `<TORCH_HOME>/hub/<dir>`, where `torch.hub.load` looks first (a pipeline that loads code
    /// through `torch.hub` gets the pinned revision, never upstream's moving branch).
    TorchHub(&'static str),
}

/// Something built into the environment after its packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Build {
    /// The step's name in logs and errors.
    pub name: &'static str,
    pub how: BuildHow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BuildHow {
    /// `uv pip install <source>/<subdir> --no-build-isolation`.
    Source {
        source: &'static str,
        subdir: Option<&'static str>,
    },
    /// `uv pip install <requirement> --no-build-isolation`, built from its sdist.
    Package(&'static str),
    /// `sh -c <script>` in `<source>/<subdir>`, with the environment's `python` first on the PATH.
    Script {
        source: &'static str,
        subdir: &'static str,
        script: &'static str,
    },
}

/// One Python environment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Recipe {
    pub name: &'static str,
    /// The CPython `uv python install` fetches.
    pub python: &'static str,
    /// The NVIDIA redist release `CUDA_HOME` is assembled from; matches torch's CUDA.
    pub cuda: &'static str,
    /// torch and torchvision, from `torch_index`.
    pub torch: &'static [&'static str],
    pub torch_index: &'static str,
    /// Everything else, resolved together.
    pub packages: &'static [&'static str],
    /// Installed without their dependencies (their pins clash with the environment's).
    pub no_deps: &'static [&'static str],
    pub sources: &'static [Source],
    pub builds: &'static [Build],
    /// Modules that must import once the environment is built.
    pub imports: &'static [&'static str],
}

impl Recipe {
    /// What identifies a built environment: the recipe, the format and the GPU it was built for.
    /// A different hash means the environment is rebuilt.
    pub fn hash(&self, capability: ComputeCapability) -> String {
        let mut hasher = Sha256::new();
        hasher.update(RECIPE_FORMAT.to_le_bytes());
        hasher.update(serde_json::to_vec(self).expect("a recipe serialises"));
        hasher.update(capability.to_string());
        hex::encode(hasher.finalize())
    }

    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|source| source.name == name)
    }
}

/// CPython for both environments: the newest 3.10 `uv` 0.13.3 installs (TRELLIS.2, Pixal3D and
/// Hunyuan3D 2.1 are all tested on 3.10).
const PYTHON: &str = "3.10.22";
/// CUDA 12.4.1: the cu124 wheels both environments pin.
const CUDA: &str = "12.4.1";
const CU124_INDEX: &str = "https://download.pytorch.org/whl/cu124";

/// Pixal3D and TRELLIS.2: torch 2.6.0 + cu124 (TRELLIS.2's pin); the packages Pixal3D's
/// prototype ran with (`~/Repositories/Pixal3D/setup-local.sh`).
pub const TRELLIS: Recipe = Recipe {
    name: "trellis",
    python: PYTHON,
    cuda: CUDA,
    torch: &["torch==2.6.0", "torchvision==0.21.0"],
    torch_index: CU124_INDEX,
    packages: &[
        "ninja==1.13.2",
        "setuptools==84.2.0",
        "wheel==0.48.0",
        "numpy==2.2.6",
        "pillow==12.0.0",
        "imageio==2.37.2",
        "imageio-ffmpeg==0.6.0",
        "tqdm==4.67.1",
        "easydict==1.13",
        "opencv-python-headless==4.12.0.88",
        "trimesh==4.10.1",
        "transformers==4.57.3",
        "huggingface-hub==0.36.2",
        "safetensors==0.8.0",
        "tokenizers==0.22.2",
        "zstandard==0.25.0",
        "kornia==0.8.2",
        "timm==1.0.22",
        "diffusers==0.37.1",
        "accelerate==1.13.0",
        "plyfile==1.1.3",
        "click==8.5.0",
        "scipy==1.15.3",
        "matplotlib==3.10.9",
        "einops==0.8.2",
        "psutil==7.2.2",
    ],
    no_deps: &[
        // MoGe before its V3 bump, whose trimesh pin clashes with Pixal3D's.
        "moge @ git+https://github.com/microsoft/MoGe.git@925b8ed835a7a9cdb7578ba15c658a0afc969030",
        "utils3d @ https://github.com/LDYang694/Storages/releases/download/20260430/utils3d-0.0.2-py3-none-any.whl",
        // TRELLIS.2's sparse attention has no SDPA backend: flash-attn, as a prebuilt wheel for
        // torch 2.6 + CUDA 12.  2.7.4.post1, not the latest 2.8.3.post2: every 2.8.x wheel labelled
        // `cxx11abiFALSE` for torch 2.6 is built with the C++11 ABI and does not load (verified).
        "flash-attn @ https://github.com/Dao-AILab/flash-attention/releases/download/v2.7.4.post1/flash_attn-2.7.4.post1%2Bcu12torch2.6cxx11abiFALSE-cp310-cp310-linux_x86_64.whl#sha256=ffe17686fa1a0f288de9eae7c32af209d32a27b037ef28614f042b377af5b15a",
    ],
    sources: &[
        Source {
            name: "nvdiffrast",
            repo: "https://github.com/NVlabs/nvdiffrast.git",
            rev: "253ac4fcea7de5f396371124af597e6cc957bfae", // v0.4.0
            submodules: false,
            home: SourceHome::Sources,
        },
        Source {
            name: "CuMesh",
            repo: "https://github.com/JeffreyXiang/CuMesh.git",
            rev: "12289e1062f0603f2f0d0771b02e1395d247f26f",
            submodules: true,
            home: SourceHome::Sources,
        },
        Source {
            name: "FlexGEMM",
            repo: "https://github.com/JeffreyXiang/FlexGEMM.git",
            rev: "6dd94a859c26ee8246888502eada3dd8ad85532e",
            submodules: true,
            home: SourceHome::Sources,
        },
        Source {
            name: "TRELLIS.2",
            repo: "https://github.com/microsoft/TRELLIS.2.git",
            rev: "75fbf0183001ed9876c8dbb35de6b68552ee08bd",
            submodules: true, // o-voxel builds against its Eigen submodule
            home: SourceHome::Sources,
        },
        Source {
            name: "Pixal3D",
            repo: "https://github.com/TencentARC/Pixal3D.git",
            rev: "f7cf38429b0bd264f1995f0f8743a88b1c728b94",
            submodules: false,
            home: SourceHome::Sources,
        },
        Source {
            name: "NAF",
            repo: "https://github.com/valeoai/NAF.git",
            rev: "37f2dfc180f2de53d98bd601109c0da0dd6b0f43",
            submodules: false,
            home: SourceHome::TorchHub("valeoai_NAF_main"),
        },
    ],
    builds: &[
        Build {
            name: "nvdiffrast",
            how: BuildHow::Source {
                source: "nvdiffrast",
                subdir: None,
            },
        },
        Build {
            name: "CuMesh",
            how: BuildHow::Source {
                source: "CuMesh",
                subdir: None,
            },
        },
        Build {
            name: "FlexGEMM",
            how: BuildHow::Source {
                source: "FlexGEMM",
                subdir: None,
            },
        },
        Build {
            name: "o-voxel",
            how: BuildHow::Source {
                source: "TRELLIS.2",
                subdir: Some("o-voxel"),
            },
        },
        Build {
            // No wheel for torch 2.6 + cu124: built from source (about 10 minutes).
            name: "natten",
            how: BuildHow::Package("natten==0.21.7"),
        },
    ],
    imports: &[
        "torch",
        "nvdiffrast.torch",
        "cumesh",
        "flex_gemm",
        "o_voxel",
        "utils3d",
        "moge.model.v2",
        "natten",
        "flash_attn",
        "huggingface_hub",
    ],
};

/// Hunyuan3D 2.1: its own tested pins (torch 2.5.1, numpy 1.24, transformers 4.46,
/// diffusers 0.30), which the `trellis` environment cannot hold.  Its demo-only packages (gradio,
/// fastapi, uvicorn, deepspeed, pythreejs, tb-nightly, cupy, open3d) and `bpy` are left out: the
/// runner writes the GLB itself.
pub const HUNYUAN3D: Recipe = Recipe {
    name: "hunyuan3d",
    python: PYTHON,
    cuda: CUDA,
    torch: &["torch==2.5.1", "torchvision==0.20.1"],
    torch_index: CU124_INDEX,
    packages: &[
        "ninja==1.11.1.1",
        "pybind11==2.13.4",
        "setuptools==75.8.0",
        "wheel==0.45.1",
        "numpy==1.24.4",
        "scipy==1.14.1",
        "transformers==4.46.0",
        "diffusers==0.30.0",
        "accelerate==1.1.1",
        "pytorch-lightning==1.9.5",
        "huggingface-hub==0.30.2",
        "safetensors==0.4.4",
        "einops==0.8.0",
        "opencv-python-headless==4.10.0.84",
        "imageio==2.36.0",
        "scikit-image==0.24.0",
        "rembg==2.0.65",
        "onnxruntime==1.16.3",
        "realesrgan==0.3.0",
        "basicsr==1.4.2",
        "trimesh==4.4.7",
        "pymeshlab==2022.2.post3",
        "pygltflib==1.16.3",
        "xatlas==0.0.9",
        "omegaconf==2.3.0",
        "pyyaml==6.0.2",
        "tqdm==4.66.5",
        "psutil==6.0.0",
        "timm==1.0.22",
        "torchdiffeq==0.2.5",
    ],
    no_deps: &[],
    sources: &[Source {
        name: "Hunyuan3D-2.1",
        repo: "https://github.com/Tencent-Hunyuan/Hunyuan3D-2.1.git",
        rev: "82920d643c0dc2f7bfd7255f45f62d386edfe60c",
        submodules: false,
        home: SourceHome::Sources,
    }],
    builds: &[
        Build {
            name: "custom_rasterizer",
            how: BuildHow::Source {
                source: "Hunyuan3D-2.1",
                subdir: Some("hy3dpaint/custom_rasterizer"),
            },
        },
        Build {
            // Upstream's compile_mesh_painter.sh, with the environment's python.
            name: "mesh_inpaint_processor",
            how: BuildHow::Script {
                source: "Hunyuan3D-2.1",
                subdir: "hy3dpaint/DifferentiableRenderer",
                script: "c++ -O3 -Wall -shared -std=c++11 -fPIC $(python -m pybind11 --includes) \
                         mesh_inpaint_processor.cpp \
                         -o mesh_inpaint_processor$(python -c 'import sysconfig; print(sysconfig.get_config_var(\"EXT_SUFFIX\"))')",
            },
        },
    ],
    // Not `realesrgan`: its basicsr imports a torchvision module 0.20 dropped, which the pipeline
    // restores (Hunyuan's `torchvision_fix`) before importing it.
    imports: &[
        "torch",
        "custom_rasterizer",
        "pymeshlab",
        "xatlas",
        "rembg",
        "huggingface_hub",
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    const ADA: ComputeCapability = ComputeCapability { major: 8, minor: 9 };
    const AMPERE: ComputeCapability = ComputeCapability { major: 8, minor: 6 };

    #[test]
    fn the_hash_follows_the_recipe_and_the_gpu() {
        assert_eq!(TRELLIS.hash(ADA), TRELLIS.hash(ADA));
        assert_ne!(TRELLIS.hash(ADA), TRELLIS.hash(AMPERE));
        assert_ne!(TRELLIS.hash(ADA), HUNYUAN3D.hash(ADA));
        let changed = Recipe {
            packages: &["numpy==2.2.6"],
            ..TRELLIS
        };
        assert_ne!(TRELLIS.hash(ADA), changed.hash(ADA));
    }

    #[test]
    fn every_pin_is_exact() {
        for recipe in [TRELLIS, HUNYUAN3D] {
            for requirement in recipe.torch.iter().chain(recipe.packages) {
                assert!(
                    requirement.contains("=="),
                    "{} pins {requirement}",
                    recipe.name
                );
            }
            for source in recipe.sources {
                assert_eq!(
                    source.rev.len(),
                    40,
                    "{} is pinned to a full commit",
                    source.name
                );
            }
        }
    }

    #[test]
    fn every_build_names_a_source_of_its_recipe() {
        for recipe in [TRELLIS, HUNYUAN3D] {
            for build in recipe.builds {
                let source = match build.how {
                    BuildHow::Source { source, .. } | BuildHow::Script { source, .. } => source,
                    BuildHow::Package(_) => continue,
                };
                assert!(
                    recipe.source(source).is_some(),
                    "{} needs {source}",
                    build.name
                );
            }
        }
    }
}
