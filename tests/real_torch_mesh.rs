//! Real image-to-3D E2E: a picture through the torch engine on this machine's GPU, provisioning
//! the environment and the weights on first use (`docs/engines/torch.md`).
//!
//! Skipped unless `RUN_REAL_ENGINE_TESTS=1`.  Everything lands under `STUDIO_WORKER_MODELS_ROOT`
//! (else `~/models`).  The picture and models come from the environment, so none is committed:
//!
//! - `MESH_PICTURE`: a picture of an object (not of a person);
//! - `MESH_PIPELINES`: the pipelines to run, comma-separated (default `pixal3d`);
//! - `MESH_OUT`: a directory the GLBs are copied to (optional);
//! - `MESH_BUDGET_GIB`: a device-memory budget for every pipeline, to measure one (optional);
//! - `MESH_RESOLUTION`: a pipeline resolution for every pipeline (optional);
//! - `MESH_LOW_VRAM`: `1` or `0`, low-VRAM mode for every pipeline (optional).
//!
//! Run it through PM2: the first run builds an environment for about 15 minutes.

use std::path::PathBuf;

use studio_worker::engine::torch::{TorchEngine, WEB_GLB_TEXTURE_PX, WEB_GLB_TRIANGLES};
use studio_worker::engine::Engine;
use studio_worker::types::{
    MeshParams, ModelCliDefaults, ModelEngine, ModelSource, Task, TaskResult,
};

/// A pipeline's registry row as the studio holds it (budgets measured on an RTX 4090).
fn source(pipeline: &str) -> ModelSource {
    let (budget, steps, guidance, low_vram, resolution) = match pipeline {
        "pixal3d" => (15.5, 12, 7.5, true, Some(1024)),
        "trellis2" => (8.0, 12, 7.5, true, Some(512)),
        "hunyuan3d" => (16.0, 30, 5.0, false, None),
        other => panic!("unknown pipeline {other}"),
    };
    ModelSource {
        engine: ModelEngine::Torch,
        files: vec![],
        cli_defaults: ModelCliDefaults {
            cfg_scale: guidance,
            steps,
            width: 1024,
            height: 1024,
            pipeline: Some(pipeline.into()),
            max_vram_gib: Some(
                std::env::var("MESH_BUDGET_GIB")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(budget),
            ),
            low_vram: Some(
                std::env::var("MESH_LOW_VRAM")
                    .map(|v| v == "1")
                    .unwrap_or(low_vram),
            ),
            resolution: std::env::var("MESH_RESOLUTION")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(resolution),
            decimation_target: Some(WEB_GLB_TRIANGLES),
            texture_size: Some(WEB_GLB_TEXTURE_PX),
            ..Default::default()
        },
    }
}

#[test]
fn a_picture_becomes_a_web_sized_glb() {
    if std::env::var_os("RUN_REAL_ENGINE_TESTS").is_none() {
        eprintln!("[real_torch_mesh] skipped: set RUN_REAL_ENGINE_TESTS=1 and MESH_PICTURE");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "studio_worker=info".into()))
        .with_ansi(false)
        .with_test_writer()
        .try_init();
    let picture = std::env::var("MESH_PICTURE").expect("MESH_PICTURE must name a picture");
    let models_root = std::env::var("STUDIO_WORKER_MODELS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| studio_worker::config::default_models_root());
    let bytes = std::fs::read(&picture).expect("reading MESH_PICTURE");
    let media = studio_worker::engine::download::sniff_image_extension(&bytes).expect("a picture");
    use base64::Engine as _;
    let image_url = format!(
        "data:image/{};base64,{}",
        if media == "jpg" { "jpeg" } else { media },
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let engine = TorchEngine::new(&models_root);
    let pipelines = std::env::var("MESH_PIPELINES").unwrap_or_else(|_| "pixal3d".into());
    for pipeline in pipelines.split(',').map(str::trim) {
        let started = std::time::Instant::now();
        let task = Task::Mesh(MeshParams {
            image_url: image_url.clone(),
            seed: Some(42),
            ext: "glb".into(),
        });
        let result = engine
            .dispatch_with_source(pipeline, task, &source(pipeline))
            .unwrap_or_else(|e| panic!("{pipeline}: {e:#}"));
        let TaskResult::Mesh { bytes, ext } = result else {
            panic!("expected a mesh")
        };
        assert_eq!(ext, "glb");
        studio_worker::glb::check(&bytes).expect("a whole GLB");
        eprintln!(
            "[real_torch_mesh] {pipeline}: {} bytes in {:.0} s",
            bytes.len(),
            started.elapsed().as_secs_f32()
        );
        assert!(
            bytes.len() < 25 * 1024 * 1024,
            "web-sized: under the studio's 25 MiB cap"
        );
        if let Ok(out) = std::env::var("MESH_OUT") {
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(PathBuf::from(out).join(format!("{pipeline}.glb")), &bytes).unwrap();
        }
    }
}
