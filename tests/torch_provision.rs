//! Integration cover for the torch engine's provisioning (`docs/engines/torch.md`,
//! "Provisioning"), with no GPU and no network: a `wiremock` server serves a fake `uv` release and
//! a fake NVIDIA redist release, the fake `uv` builds a fake environment, and the sources are local
//! git repositories.  It proves the order of the steps, what each one is given, that a built
//! environment is reused, and that a failed step is named with its output and leaves nothing
//! usable behind.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use studio_worker::engine::torch::layout::Layout;
use studio_worker::engine::torch::platform::ComputeCapability;
use studio_worker::engine::torch::provision::{CudaSource, Provisioner, UvSource};
use studio_worker::engine::torch::recipe::{Build, BuildHow, Recipe, Source, SourceHome};
use studio_worker::engine::torch::TorchError;
use wiremock::matchers::{method, path as match_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PARTS: [&str; 5] = [
    "cuda_nvcc",
    "cuda_cudart",
    "cuda_cccl",
    "cuda_profiler_api",
    "cuda_nvtx",
];

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let status = studio_worker::process::command(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "{program} {args:?}");
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A `uv` that logs every call to `log` and builds just enough of an environment.
fn fake_uv(log: &Path) -> String {
    let log = log.display();
    format!(
        r#"#!/bin/sh
echo "uv $*" >> {log}
case "$1" in
  python) exit 0 ;;
  venv)
    mkdir -p "$4/bin" "$4/lib/python3.10/site-packages"
    printf '#!/bin/sh\necho "python $*" >> {log}\nexit 0\n' > "$4/bin/python"
    chmod +x "$4/bin/python"
    exit 0 ;;
  pip)
    case "$*" in *FAIL_ME*) echo "error: building FAIL_ME failed" >&2; exit 1 ;; esac
    site="$(dirname "$(dirname "$4")")/lib/python3.10/site-packages"
    case "$*" in *torch==*)
      mkdir -p "$site/nvidia/cublas/include" "$site/nvidia/cublas/lib"
      echo header > "$site/nvidia/cublas/include/cublas_v2.h"
      echo library > "$site/nvidia/cublas/lib/libcublas.so.12" ;;
    esac
    exit 0 ;;
esac
exit 9
"#
    )
}

/// `tar -c<flag>f` of `dir`'s entry `name` into bytes.
fn tarball(dir: &Path, name: &str, flag: &str) -> Vec<u8> {
    let out = dir.join(format!("{name}.archive"));
    run(
        dir,
        "tar",
        &[&format!("-c{flag}f"), out.to_str().unwrap(), name],
    );
    std::fs::read(out).unwrap()
}

struct Fixture {
    _server_runtime: tokio::runtime::Runtime,
    server: MockServer,
    root: tempfile::TempDir,
    log: PathBuf,
    uv_sha: String,
    manifest_sha: String,
    source: &'static str,
    rev: &'static str,
}

fn fixture() -> Fixture {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let log = root.path().join("calls.log");

    // The uv release: `uv-x86_64-unknown-linux-gnu/uv`.
    let uv_dir = staging.join("uv-x86_64-unknown-linux-gnu");
    std::fs::create_dir_all(&uv_dir).unwrap();
    std::fs::write(uv_dir.join("uv"), fake_uv(&log)).unwrap();
    std::fs::set_permissions(uv_dir.join("uv"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let uv = tarball(&staging, "uv-x86_64-unknown-linux-gnu", "z");
    let uv_sha = sha256(&uv);

    // The redist release: one xz archive per part, each with a top directory to strip.
    let mut manifest = serde_json::Map::new();
    for part in PARTS {
        let top = format!("{part}-linux-x86_64-0.0.0-archive");
        let dir = staging.join(&top);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("lib").join(format!("lib{part}.so.12")), part).unwrap();
        if part == "cuda_nvcc" {
            std::fs::write(dir.join("bin/nvcc"), "nvcc").unwrap();
        }
        let archive = tarball(&staging, &top, "J");
        let relative = format!("{part}/linux-x86_64/{top}.tar.xz");
        manifest.insert(
            part.into(),
            serde_json::json!({ "linux-x86_64": { "relative_path": relative, "sha256": sha256(&archive) } }),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(match_path(format!("/{relative}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
                .mount(&server),
        );
    }
    let manifest = serde_json::Value::Object(manifest).to_string();
    let manifest_sha = sha256(manifest.as_bytes());
    runtime.block_on(async {
        Mock::given(method("GET"))
            .and(match_path("/redistrib_fake.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(manifest))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/uv.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(uv))
            .mount(&server)
            .await;
    });

    // A source: a local git repository with one commit.
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, "git", &["init", "--quiet"]);
    std::fs::File::create(repo.join("setup.py"))
        .unwrap()
        .write_all(b"# a package")
        .unwrap();
    run(&repo, "git", &["add", "."]);
    run(&repo, "git", &["commit", "--quiet", "-m", "one"]);
    let rev = String::from_utf8(
        studio_worker::process::command("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    Fixture {
        _server_runtime: runtime,
        server,
        source: leak(format!("file://{}", repo.display())),
        rev: leak(rev.trim().to_string()),
        root,
        log,
        uv_sha,
        manifest_sha,
    }
}

fn recipe(f: &Fixture, packages: &'static [&'static str]) -> Recipe {
    let sources: &'static [Source] = Box::leak(Box::new([Source {
        name: "lib",
        repo: f.source,
        rev: f.rev,
        submodules: false,
        home: SourceHome::Sources,
    }]));
    Recipe {
        name: "fake",
        python: "3.10.22",
        cuda: "fake",
        torch: &["torch==0.0.0"],
        torch_index: "https://example.invalid/whl/cu000",
        packages,
        no_deps: &["thing @ https://example.invalid/thing.whl"],
        sources,
        builds: &[
            Build {
                name: "lib",
                how: BuildHow::Source {
                    source: "lib",
                    subdir: None,
                },
            },
            Build {
                name: "script",
                how: BuildHow::Script {
                    source: "lib",
                    subdir: ".",
                    script: "touch built-by-script",
                },
            },
        ],
        imports: &["torch", "lib"],
    }
}

fn provisioner(f: &Fixture) -> Provisioner {
    Provisioner {
        layout: Layout::new(f.root.path().join("models")),
        capability: ComputeCapability { major: 8, minor: 9 },
        uv: UvSource::Release {
            url: format!("{}/uv.tar.gz", f.server.uri()),
            sha256: f.uv_sha.clone(),
        },
        cuda: CudaSource {
            redist_url: f.server.uri(),
            manifest_sha256: Some(f.manifest_sha.clone()),
        },
    }
}

fn calls(f: &Fixture) -> Vec<String> {
    std::fs::read_to_string(&f.log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn an_environment_is_built_step_by_step_reused_and_rebuilt_when_its_recipe_changes() {
    let f = fixture();
    let p = provisioner(&f);
    let fake = recipe(&f, &["einops==0.8.2"]);

    let environment = p.ensure(&fake).expect("the environment builds");

    let called = calls(&f);
    let python = environment.python.display().to_string();
    let lib = p.layout.source(&fake.sources[0]).display().to_string();
    let expected = [
        "uv python install 3.10.22".to_string(),
        format!("uv venv --python 3.10.22 {}", environment.venv().display()),
        format!("uv pip install --python {python} torch==0.0.0 --index-url https://example.invalid/whl/cu000"),
        format!("uv pip install --python {python} einops==0.8.2"),
        format!("uv pip install --python {python} --no-deps thing @ https://example.invalid/thing.whl"),
        format!("uv pip install --python {python} --no-build-isolation {lib}"),
        "python -c import torch, lib".to_string(),
    ];
    assert_eq!(called, expected);
    // CUDA_HOME: the archives, lib folded into lib64, the wheel's libraries linked in.
    let cuda = &environment.cuda_home;
    assert_eq!(
        std::fs::read_to_string(cuda.join("bin/nvcc")).unwrap(),
        "nvcc"
    );
    assert!(cuda.join("lib64/libcuda_cudart.so").exists());
    assert!(!cuda.join("lib").exists());
    assert_eq!(
        std::fs::read_to_string(cuda.join("include/cublas_v2.h"))
            .unwrap()
            .trim(),
        "header"
    );
    assert!(cuda.join("lib64/libcublas.so").exists());
    // The script build ran in its source.
    assert!(Path::new(&lib).join("built-by-script").exists());
    assert!(environment.dir.join("recipe.json").exists());
    assert_eq!(
        environment.sources,
        vec![("lib".to_string(), PathBuf::from(&lib))]
    );

    // Ready: nothing runs again.
    p.ensure(&fake).expect("reused");
    assert_eq!(calls(&f).len(), expected.len());

    // A changed recipe rebuilds; a failed step says which and why, and leaves it unusable.
    let failing = recipe(&f, &["FAIL_ME==1"]);
    let err = p.ensure(&failing).expect_err("the packages step fails");
    let TorchError::Provision {
        step,
        message,
        tail,
    } = err
    else {
        panic!("expected a provisioning error, got {err}")
    };
    assert_eq!(step, "packages");
    assert!(message.contains("exit 1"), "{message}");
    assert!(tail.contains("error: building FAIL_ME failed"), "{tail}");
    assert!(!p.is_ready(&fake, &environment));
    assert!(!p.is_ready(&failing, &environment));

    // Fixed again: the source checkout is kept, everything else rebuilt.
    p.ensure(&fake).expect("rebuilt");
    assert!(p.is_ready(&fake, &environment));
    // `uv` was downloaded once, by the first build.
    let uv_downloads = f
        ._server_runtime
        .block_on(f.server.received_requests())
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/uv.tar.gz")
        .count();
    assert_eq!(uv_downloads, 1);
}

#[test]
fn a_uv_release_that_does_not_match_its_checksum_is_refused() {
    let f = fixture();
    let mut p = provisioner(&f);
    p.uv = UvSource::Release {
        url: format!("{}/uv.tar.gz", f.server.uri()),
        sha256: "0".repeat(64),
    };
    let err = p.ensure_uv().unwrap_err();
    assert!(err.to_string().contains("sha256 mismatch"), "{err}");
    assert!(!p.layout.uv().exists());
}
