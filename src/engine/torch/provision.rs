//! Provisioning: `uv`, then an environment built from its recipe, step by step, each step logged
//! (`docs/engines/torch.md`, "Provisioning").  An environment is usable only once its
//! `recipe.json` names its recipe's hash, which is written last.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt as _;

use super::cuda;
use super::error::TorchError;
use super::layout::Layout;
use super::platform::ComputeCapability;
use super::recipe::{BuildHow, Recipe, Source, SourceHome};
use super::stream::{self, Ended};
use super::TRACE_TARGET;

/// The `uv` the engine provisions.  Latest on 2026-10-20.
pub const UV_VERSION: &str = "0.13.3";
/// `uv-x86_64-unknown-linux-gnu.tar.gz` of [`UV_VERSION`], from its release's `.sha256`.
const UV_SHA256: &str = "e4f169ec380f1793b898a6f1fcbacd22db25cf7ca0349a6e96ffb85b69998d04";
/// The longest any one provisioning step may run (NATTEN's build is about 10 minutes).
/// Safe range 20..=55 minutes.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(50 * 60);
/// Parallel compile jobs for an extension build: each nvcc job can take a few GiB of RAM.
/// Safe range 1..=16.
pub const BUILD_JOBS_MAX: usize = 8;
/// The marker an environment is usable by.
const MARKER: &str = "recipe.json";

/// Where `uv` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UvSource {
    /// An existing binary (`$STUDIO_WORKER_UV`, or a test's fake).
    Binary(PathBuf),
    /// A release tarball, checked against its sha256.
    Release { url: String, sha256: String },
}

impl UvSource {
    /// `$STUDIO_WORKER_UV` when set, else the pinned release.
    pub fn from_env() -> Self {
        match std::env::var_os("STUDIO_WORKER_UV").filter(|v| !v.is_empty()) {
            Some(path) => UvSource::Binary(PathBuf::from(path)),
            None => UvSource::Release {
                url: format!(
                    "https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/uv-x86_64-unknown-linux-gnu.tar.gz"
                ),
                sha256: UV_SHA256.into(),
            },
        }
    }
}

/// Where NVIDIA's redist archives come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaSource {
    pub redist_url: String,
    /// The manifest's sha256 per CUDA release; `None` uses the pinned ones.
    pub manifest_sha256: Option<String>,
}

impl Default for CudaSource {
    fn default() -> Self {
        Self {
            redist_url: cuda::REDIST_URL.into(),
            manifest_sha256: None,
        }
    }
}

/// A built environment, ready to run a pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub dir: PathBuf,
    pub python: PathBuf,
    pub cuda_home: PathBuf,
    /// Every source of the recipe: its name and where it is checked out.
    pub sources: Vec<(String, PathBuf)>,
}

impl Environment {
    fn of(layout: &Layout, recipe: &Recipe) -> Self {
        let dir = layout.environment(recipe);
        Self {
            python: dir.join(".venv").join("bin").join("python"),
            cuda_home: dir.join("cuda"),
            sources: recipe
                .sources
                .iter()
                .map(|s| (s.name.to_string(), layout.source(s)))
                .collect(),
            dir,
        }
    }

    pub fn venv(&self) -> PathBuf {
        self.dir.join(".venv")
    }
}

/// Builds environments under a [`Layout`] for one GPU.
#[derive(Debug, Clone)]
pub struct Provisioner {
    pub layout: Layout,
    pub capability: ComputeCapability,
    pub uv: UvSource,
    pub cuda: CudaSource,
}

/// The tools provisioning runs besides `uv`, with what to do when one is missing.
const TOOLS: [(&str, &str); 3] = [
    ("git", "install git (e.g. `sudo apt install git`)"),
    (
        "tar",
        "install tar with xz support (e.g. `sudo apt install tar xz-utils`)",
    ),
    (
        "c++",
        "install a C++ compiler (e.g. `sudo apt install build-essential`)",
    ),
];

/// Whether `tool` is on the `PATH`.
fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
}

/// Run one step, logging its start, end and time; a failure is logged with its message.
fn step<T>(
    name: &str,
    recipe: &str,
    run: impl FnOnce() -> Result<T, TorchError>,
) -> Result<T, TorchError> {
    let started = Instant::now();
    tracing::info!(target: TRACE_TARGET, op = "provision", environment = recipe, step = name, "started");
    let result = run();
    let seconds = started.elapsed().as_secs_f32();
    match &result {
        Ok(_) => tracing::info!(
            target: TRACE_TARGET,
            op = "provision",
            environment = recipe,
            step = name,
            seconds,
            "done"
        ),
        Err(e) => tracing::error!(
            target: TRACE_TARGET,
            op = "provision",
            environment = recipe,
            step = name,
            seconds,
            error = %e,
            "failed"
        ),
    }
    result
}

/// Run `command` for `step_name`; its output goes to the debug log and, on failure, its last lines
/// into the error.
fn run_command(step_name: &str, command: Command) -> Result<(), TorchError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let ended = stream::run(command, STEP_TIMEOUT, |_, line| {
        tracing::debug!(target: TRACE_TARGET, op = "provision", step = step_name, line);
    })
    .map_err(|e| TorchError::provision(step_name, format!("running {program}: {e}")))?;
    match ended {
        Ended::Exited { status, .. } if status.success() => Ok(()),
        Ended::Exited { status, tail } => Err(TorchError::Provision {
            step: step_name.into(),
            message: format!("{program}: {}", stream::describe(&status)),
            tail,
        }),
        Ended::TimedOut { tail } => Err(TorchError::Provision {
            step: step_name.into(),
            message: format!(
                "{program} passed {} minutes and was stopped",
                STEP_TIMEOUT.as_secs() / 60
            ),
            tail,
        }),
    }
}

fn io_error(step_name: &str) -> impl Fn(std::io::Error) -> TorchError + '_ {
    move |e| TorchError::provision(step_name, e)
}

impl Provisioner {
    /// The environment `recipe` describes, built first when it is missing or stale.  One
    /// provisioning at a time, across processes.
    pub fn ensure(&self, recipe: &Recipe) -> Result<Environment, TorchError> {
        let environment = Environment::of(&self.layout, recipe);
        let root = self.layout.root();
        std::fs::create_dir_all(&root).map_err(io_error("lock"))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.layout.lock())
            .map_err(io_error("lock"))?;
        lock.lock_exclusive().map_err(io_error("lock"))?;
        if self.is_ready(recipe, &environment) {
            tracing::debug!(target: TRACE_TARGET, op = "provision", environment = recipe.name, "ready");
            return Ok(environment);
        }
        tracing::info!(
            target: TRACE_TARGET,
            op = "provision",
            environment = recipe.name,
            compute_capability = %self.capability,
            "building the environment (first job, or its recipe changed)"
        );
        for (tool, remedy) in TOOLS {
            if !on_path(tool) {
                return Err(TorchError::MissingTool { tool, remedy });
            }
        }
        let uv = step("uv", recipe.name, || self.ensure_uv())?;
        self.build(recipe, &environment, &uv)?;
        let marker = serde_json::json!({
            "hash": recipe.hash(self.capability),
            "recipe": recipe,
            "computeCapability": self.capability.to_string(),
            "builtAt": chrono::Utc::now().to_rfc3339(),
        });
        std::fs::write(
            environment.dir.join(MARKER),
            serde_json::to_vec_pretty(&marker).expect("a marker serialises"),
        )
        .map_err(io_error("marker"))?;
        Ok(environment)
    }

    /// Built, for this recipe and this GPU.
    pub fn is_ready(&self, recipe: &Recipe, environment: &Environment) -> bool {
        let Ok(text) = std::fs::read_to_string(environment.dir.join(MARKER)) else {
            return false;
        };
        let hash = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|marker| marker["hash"].as_str().map(str::to_string));
        hash.as_deref() == Some(&recipe.hash(self.capability)) && environment.python.is_file()
    }

    /// The `uv` binary: the given one, or the pinned release, downloaded once.
    pub fn ensure_uv(&self) -> Result<PathBuf, TorchError> {
        let (url, sha256) = match &self.uv {
            UvSource::Binary(path) => return Ok(path.clone()),
            UvSource::Release { url, sha256 } => (url, sha256),
        };
        let uv = self.layout.uv();
        let marker = uv.with_extension("release");
        if uv.is_file() && std::fs::read_to_string(&marker).ok().as_deref() == Some(url.as_str()) {
            return Ok(uv);
        }
        let archives = self.layout.archives();
        let tarball = archives.join(format!("uv-{}.tar.gz", &sha256[..12]));
        crate::engine::download::download_file_verified(url, &tarball, Some(sha256))
            .map_err(|e| TorchError::provision("uv", format!("{e:#}")))?;
        let unpacked = archives.join("uv-unpacked");
        if unpacked.exists() {
            std::fs::remove_dir_all(&unpacked).map_err(io_error("uv"))?;
        }
        std::fs::create_dir_all(&unpacked).map_err(io_error("uv"))?;
        let mut tar = crate::process::command("tar");
        tar.arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&unpacked)
            .arg("--strip-components=1");
        run_command("uv", tar)?;
        std::fs::create_dir_all(uv.parent().expect("uv lives in bin/")).map_err(io_error("uv"))?;
        std::fs::rename(unpacked.join("uv"), &uv).map_err(io_error("uv"))?;
        std::fs::write(&marker, url).map_err(io_error("uv"))?;
        std::fs::remove_dir_all(&unpacked).map_err(io_error("uv"))?;
        Ok(uv)
    }

    /// The variables every provisioning command and every run sees: caches under `models_root`.
    pub fn base_env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("UV_CACHE_DIR", self.layout.uv_cache()),
            ("UV_PYTHON_INSTALL_DIR", self.layout.pythons()),
            ("XDG_CACHE_HOME", self.layout.cache()),
            ("HF_HOME", self.layout.hf_home()),
            ("TORCH_HOME", self.layout.torch_home()),
        ]
    }

    /// The variables an extension build sees: the toolkit first on the `PATH`, this GPU's arch.
    pub fn build_env(&self, environment: &Environment) -> Vec<(String, String)> {
        let lossy = |p: &Path| p.to_string_lossy().into_owned();
        let cuda_bin = environment.cuda_home.join("bin");
        let venv_bin = environment.venv().join("bin");
        let lib64 = lossy(&environment.cuda_home.join("lib64"));
        let inherited = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let join = |first: String, rest: Option<String>| match rest {
            Some(rest) => format!("{first}:{rest}"),
            None => first,
        };
        let jobs = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, BUILD_JOBS_MAX))
            .unwrap_or(1)
            .to_string();
        let arch = self.capability.to_string();
        vec![
            ("CUDA_HOME".into(), lossy(&environment.cuda_home)),
            ("VIRTUAL_ENV".into(), lossy(&environment.venv())),
            (
                "PATH".into(),
                join(
                    format!("{}:{}", lossy(&cuda_bin), lossy(&venv_bin)),
                    inherited("PATH"),
                ),
            ),
            (
                "LIBRARY_PATH".into(),
                join(lib64.clone(), inherited("LIBRARY_PATH")),
            ),
            (
                "LD_LIBRARY_PATH".into(),
                join(lib64, inherited("LD_LIBRARY_PATH")),
            ),
            ("TORCH_CUDA_ARCH_LIST".into(), arch.clone()),
            ("NATTEN_CUDA_ARCH".into(), arch),
            ("MAX_JOBS".into(), jobs.clone()),
            ("NATTEN_N_WORKERS".into(), jobs),
        ]
    }

    fn uv_command(&self, uv: &Path, args: &[&str]) -> Command {
        let mut command = crate::process::command(uv);
        command.args(args);
        command.envs(self.base_env());
        command.env("UV_PYTHON_PREFERENCE", "only-managed");
        command.env("UV_NO_PROGRESS", "1");
        command
    }

    /// `uv pip install --python <env python> <args>`.
    fn pip(&self, uv: &Path, environment: &Environment, args: &[&str]) -> Command {
        let python = environment.python.to_string_lossy().into_owned();
        let mut all = vec!["pip", "install", "--python", python.as_str()];
        all.extend_from_slice(args);
        self.uv_command(uv, &all)
    }

    /// Every step of a fresh build, in order.
    fn build(
        &self,
        recipe: &Recipe,
        environment: &Environment,
        uv: &Path,
    ) -> Result<(), TorchError> {
        let name = recipe.name;
        step("python", name, || {
            run_command(
                "python",
                self.uv_command(uv, &["python", "install", recipe.python]),
            )
        })?;
        step("venv", name, || {
            if environment.dir.exists() {
                std::fs::remove_dir_all(&environment.dir).map_err(io_error("venv"))?;
            }
            std::fs::create_dir_all(&environment.dir).map_err(io_error("venv"))?;
            let venv = environment.venv().to_string_lossy().into_owned();
            run_command(
                "venv",
                self.uv_command(uv, &["venv", "--python", recipe.python, &venv]),
            )
        })?;
        step("torch", name, || {
            let mut args = recipe.torch.to_vec();
            args.extend(["--index-url", recipe.torch_index]);
            run_command("torch", self.pip(uv, environment, &args))
        })?;
        step("cuda", name, || {
            let manifest_sha = match &self.cuda.manifest_sha256 {
                Some(sha) => sha.clone(),
                None => cuda::manifest_sha256(recipe.cuda)
                    .ok_or_else(|| {
                        TorchError::provision(
                            "cuda",
                            format!("CUDA {} has no pinned manifest", recipe.cuda),
                        )
                    })?
                    .to_string(),
            };
            cuda::assemble(
                &self.cuda.redist_url,
                recipe.cuda,
                &manifest_sha,
                &self.layout.archives(),
                &environment.cuda_home,
                &cuda::site_packages(&environment.venv(), recipe.python),
            )
        })?;
        step("packages", name, || {
            run_command("packages", self.pip(uv, environment, recipe.packages))
        })?;
        if !recipe.no_deps.is_empty() {
            step("no-deps packages", name, || {
                let mut args = vec!["--no-deps"];
                args.extend_from_slice(recipe.no_deps);
                run_command("no-deps packages", self.pip(uv, environment, &args))
            })?;
        }
        for source in recipe.sources {
            step(&format!("source {}", source.name), name, || {
                self.checkout(source)
            })?;
        }
        for build in recipe.builds {
            step(&format!("build {}", build.name), name, || {
                let mut command = match build.how {
                    BuildHow::Source { source, subdir } => {
                        let dir = self.source_dir(recipe, source)?;
                        let dir = match subdir {
                            Some(subdir) => dir.join(subdir),
                            None => dir,
                        };
                        let dir = dir.to_string_lossy().into_owned();
                        self.pip(uv, environment, &["--no-build-isolation", &dir])
                    }
                    BuildHow::Package(requirement) => {
                        self.pip(uv, environment, &["--no-build-isolation", requirement])
                    }
                    BuildHow::Script {
                        source,
                        subdir,
                        script,
                    } => {
                        let mut command = crate::process::command("sh");
                        command
                            .args(["-c", script])
                            .current_dir(self.source_dir(recipe, source)?.join(subdir));
                        command.envs(self.base_env());
                        command
                    }
                };
                command.envs(self.build_env(environment));
                run_command(build.name, command)
            })?;
        }
        step("imports", name, || {
            let script = format!("import {}", recipe.imports.join(", "));
            let mut command = crate::process::command(&environment.python);
            command.args(["-c", &script]);
            command.envs(self.base_env());
            command.envs(self.build_env(environment));
            run_command("imports", command)
        })
    }

    fn source_dir(&self, recipe: &Recipe, name: &str) -> Result<PathBuf, TorchError> {
        recipe
            .source(name)
            .map(|s| self.layout.source(s))
            .ok_or_else(|| TorchError::provision("build", format!("no source named {name}")))
    }

    /// Check `source` out at its revision, unless it already is.  A `torch.hub` checkout that is
    /// there already is left alone (another tool may have put it there).
    fn checkout(&self, source: &Source) -> Result<(), TorchError> {
        let dest = self.layout.source(source);
        let step_name = format!("source {}", source.name);
        if dest.exists() {
            if let SourceHome::TorchHub(_) = source.home {
                tracing::info!(
                    target: TRACE_TARGET,
                    op = "provision",
                    source = source.name,
                    path = %dest.display(),
                    "a torch.hub checkout is already there; left as it is"
                );
                return Ok(());
            }
            if self.head(&dest).as_deref() == Some(source.rev) {
                return if source.submodules {
                    self.submodules(&step_name, &dest)
                } else {
                    Ok(())
                };
            }
            std::fs::remove_dir_all(&dest).map_err(io_error(&step_name))?;
        }
        let partial = dest.with_extension("partial");
        if partial.exists() {
            std::fs::remove_dir_all(&partial).map_err(io_error(&step_name))?;
        }
        std::fs::create_dir_all(dest.parent().expect("a source lives in a directory"))
            .map_err(io_error(&step_name))?;
        let mut clone = crate::process::command("git");
        clone
            .args(["clone", "--quiet", "--filter=blob:none", source.repo])
            .arg(&partial);
        run_command(&step_name, clone)?;
        let mut checkout = crate::process::command("git");
        checkout.arg("-C").arg(&partial).args([
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--quiet",
            source.rev,
        ]);
        run_command(&step_name, checkout)?;
        if source.submodules {
            let mut submodules = crate::process::command("git");
            submodules.arg("-C").arg(&partial).args([
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--quiet",
            ]);
            run_command(&step_name, submodules)?;
        }
        if let SourceHome::TorchHub(_) = source.home {
            // torch.hub trusts what it finds only once the repo is on its trusted list.
            let trusted = self.layout.torch_home().join("hub").join("trusted_list");
            let owner_repo = source
                .repo
                .trim_start_matches("https://github.com/")
                .trim_end_matches(".git")
                .replace('/', "_");
            let listed = std::fs::read_to_string(&trusted).unwrap_or_default();
            if !listed.lines().any(|line| line == owner_repo) {
                let mut text = listed;
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&owner_repo);
                text.push('\n');
                std::fs::write(&trusted, text).map_err(io_error(&step_name))?;
            }
        }
        std::fs::rename(&partial, &dest).map_err(io_error(&step_name))
    }

    /// Check a checkout's submodules out (a no-op when they are).
    fn submodules(&self, step_name: &str, dir: &Path) -> Result<(), TorchError> {
        let mut submodules = crate::process::command("git");
        submodules.arg("-C").arg(dir).args([
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--quiet",
        ]);
        run_command(step_name, submodules)
    }

    /// The commit a checkout is at.
    fn head(&self, dir: &Path) -> Option<String> {
        let output = crate::process::command("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::torch::recipe::TRELLIS;

    fn provisioner(root: &Path) -> Provisioner {
        Provisioner {
            layout: Layout::new(root),
            capability: ComputeCapability { major: 8, minor: 9 },
            uv: UvSource::Binary(PathBuf::from("/opt/uv")),
            cuda: CudaSource::default(),
        }
    }

    #[test]
    fn the_release_is_the_default_and_an_override_wins() {
        match UvSource::from_env() {
            UvSource::Release { url, sha256 } => {
                assert!(url.contains(UV_VERSION));
                assert_eq!(sha256, UV_SHA256);
            }
            // A developer may have set it; then it is the binary.
            UvSource::Binary(path) => assert!(!path.as_os_str().is_empty()),
        }
    }

    #[test]
    fn an_environment_is_ready_only_with_its_own_hash_and_a_python() {
        let dir = tempfile::tempdir().unwrap();
        let p = provisioner(dir.path());
        let environment = Environment::of(&p.layout, &TRELLIS);
        assert!(!p.is_ready(&TRELLIS, &environment));
        std::fs::create_dir_all(environment.python.parent().unwrap()).unwrap();
        std::fs::write(&environment.python, "").unwrap();
        std::fs::write(
            environment.dir.join(MARKER),
            serde_json::json!({ "hash": "stale" }).to_string(),
        )
        .unwrap();
        assert!(!p.is_ready(&TRELLIS, &environment));
        std::fs::write(
            environment.dir.join(MARKER),
            serde_json::json!({ "hash": TRELLIS.hash(p.capability) }).to_string(),
        )
        .unwrap();
        assert!(p.is_ready(&TRELLIS, &environment));
        std::fs::remove_file(&environment.python).unwrap();
        assert!(!p.is_ready(&TRELLIS, &environment));
    }

    #[test]
    fn a_ready_environment_is_handed_back_without_building() {
        let dir = tempfile::tempdir().unwrap();
        let p = provisioner(dir.path());
        let environment = Environment::of(&p.layout, &TRELLIS);
        std::fs::create_dir_all(environment.python.parent().unwrap()).unwrap();
        std::fs::write(&environment.python, "").unwrap();
        std::fs::write(
            environment.dir.join(MARKER),
            serde_json::json!({ "hash": TRELLIS.hash(p.capability) }).to_string(),
        )
        .unwrap();
        let got = p.ensure(&TRELLIS).unwrap();
        assert_eq!(got, environment);
        assert_eq!(got.sources.len(), TRELLIS.sources.len());
        assert!(got.cuda_home.ends_with("envs/trellis/cuda"));
    }

    #[test]
    fn builds_see_the_toolkit_first_and_this_gpu() {
        let dir = tempfile::tempdir().unwrap();
        let p = provisioner(dir.path());
        let environment = Environment::of(&p.layout, &TRELLIS);
        let env: std::collections::BTreeMap<String, String> =
            p.build_env(&environment).into_iter().collect();
        assert_eq!(env["TORCH_CUDA_ARCH_LIST"], "8.9");
        assert_eq!(env["NATTEN_CUDA_ARCH"], "8.9");
        assert!(
            env["PATH"].starts_with(&format!("{}:", environment.cuda_home.join("bin").display()))
        );
        assert!(env["LD_LIBRARY_PATH"].starts_with(
            &environment
                .cuda_home
                .join("lib64")
                .to_string_lossy()
                .into_owned()
        ));
        let jobs: usize = env["MAX_JOBS"].parse().unwrap();
        assert!((1..=BUILD_JOBS_MAX).contains(&jobs));
        let base: Vec<&str> = p.base_env().iter().map(|(k, _)| *k).collect();
        assert_eq!(
            base,
            [
                "UV_CACHE_DIR",
                "UV_PYTHON_INSTALL_DIR",
                "XDG_CACHE_HOME",
                "HF_HOME",
                "TORCH_HOME"
            ]
        );
        for (_, path) in p.base_env() {
            assert!(path.starts_with(dir.path()), "{}", path.display());
        }
    }

    #[test]
    fn pip_targets_the_environments_python() {
        let dir = tempfile::tempdir().unwrap();
        let p = provisioner(dir.path());
        let environment = Environment::of(&p.layout, &TRELLIS);
        let command = p.pip(Path::new("/opt/uv"), &environment, &["einops==0.8.2"]);
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), "/opt/uv");
        assert_eq!(
            args,
            [
                "pip",
                "install",
                "--python",
                &environment.python.to_string_lossy(),
                "einops==0.8.2"
            ]
        );
        let preference = command
            .get_envs()
            .find(|(k, _)| *k == "UV_PYTHON_PREFERENCE")
            .and_then(|(_, v)| v);
        assert_eq!(preference, Some(std::ffi::OsStr::new("only-managed")));
    }

    #[test]
    fn a_given_uv_is_used_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            provisioner(dir.path()).ensure_uv().unwrap(),
            PathBuf::from("/opt/uv")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_command_names_its_step_and_keeps_its_tail() {
        let mut command = crate::process::command("sh");
        command.args(["-c", "echo compiling; echo 'nvcc fatal' >&2; exit 2"]);
        let err = run_command("build natten", command).unwrap_err();
        let TorchError::Provision {
            step,
            message,
            tail,
        } = err
        else {
            panic!("expected a provisioning error")
        };
        assert_eq!(step, "build natten");
        assert_eq!(message, "sh: exit 2");
        assert!(tail.contains("nvcc fatal"));
        assert!(run_command("x", crate::process::command("/nonexistent/x")).is_err());
    }

    #[test]
    fn steps_are_logged_with_their_outcome() {
        let logs = crate::test_support::capture(|| {
            let _ = step("venv", "trellis", || Ok(()));
            let _ = step("natten", "trellis", || -> Result<(), TorchError> {
                Err(TorchError::provision("natten", "exit 1"))
            });
        });
        assert!(logs.contains("op=\"provision\""), "{logs}");
        assert!(logs.contains("step=\"venv\""), "{logs}");
        assert!(logs.contains("done"), "{logs}");
        assert!(logs.contains("failed"), "{logs}");
    }
}
