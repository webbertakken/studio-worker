//! The build variant: which GPU backend this binary was compiled for.
//!
//! Each release ships cargo-dist's CPU archive for every target and, on x86_64 Linux, a CUDA
//! archive beside it (`studio-worker-<target>-cuda.tar.xz`).  The shell installer picks one;
//! the auto-updater keeps the one it runs as ([`crate::update::choose_variant`]).
//! Docs: `docs/operations/release.md`.

use std::fmt;

/// A build variant of the worker binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// cargo-dist's default build: the in-process LLM runs on the CPU.
    Cpu,
    /// Built with `--features cuda`: the in-process LLM runs on an NVIDIA GPU.
    Cuda,
}

impl Variant {
    /// The variant this binary was built as.
    pub const fn current() -> Self {
        #[cfg(feature = "cuda")]
        let variant = Self::Cuda;
        #[cfg(not(feature = "cuda"))]
        let variant = Self::Cpu;
        variant
    }

    /// The value the shell installer takes in `STUDIO_WORKER_VARIANT`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The environment variable the shell installer reads the variant from.
pub const INSTALLER_ENV: &str = "STUDIO_WORKER_VARIANT";

/// The release target this binary was built for, when it is one the releases ship.
pub const fn release_target() -> Option<&'static str> {
    RELEASE_TARGET
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
const RELEASE_TARGET: Option<&str> = Some("x86_64-unknown-linux-gnu");
#[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
const RELEASE_TARGET: Option<&str> = Some("aarch64-unknown-linux-gnu");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const RELEASE_TARGET: Option<&str> = Some("aarch64-apple-darwin");
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const RELEASE_TARGET: Option<&str> = Some("x86_64-apple-darwin");
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
const RELEASE_TARGET: Option<&str> = Some("x86_64-pc-windows-msvc");
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
)))]
const RELEASE_TARGET: Option<&str> = None;

/// The release archive holding the CUDA variant for `target`.
pub fn cuda_archive_name(target: &str) -> String {
    format!("studio-worker-{target}-cuda.tar.xz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_matches_the_cuda_feature() {
        let expected = if cfg!(feature = "cuda") {
            Variant::Cuda
        } else {
            Variant::Cpu
        };
        assert_eq!(Variant::current(), expected);
    }

    #[test]
    fn variants_spell_as_the_installer_reads_them() {
        assert_eq!(Variant::Cpu.as_str(), "cpu");
        assert_eq!(Variant::Cuda.to_string(), "cuda");
    }

    #[test]
    fn cuda_archive_sits_beside_cargo_dists_archive() {
        assert_eq!(
            cuda_archive_name("x86_64-unknown-linux-gnu"),
            "studio-worker-x86_64-unknown-linux-gnu-cuda.tar.xz"
        );
    }

    #[test]
    fn release_target_names_a_shipped_target_on_ci_hosts() {
        // CI runs on x86_64 Linux, macOS and Windows: all release targets.
        let target = release_target().expect("CI hosts are release targets");
        assert!(target.contains(std::env::consts::ARCH), "{target}");
    }
}
