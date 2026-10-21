"""Pixal3D (TencentARC/Pixal3D, MIT; its model card disallows the EU, used by Webber's choice).

Follows Pixal3D's `inference.py` (single image, MoGe-2 camera, 1024 cascade in low-VRAM mode),
with BiRefNet in place of the gated RMBG-2.0 and every weight at a pinned revision.
"""

import math
import os

import numpy as np

from . import common

PIXAL3D = ("TencentARC/Pixal3D", "b0cb2e1b794cab9aa0ac38a95d794a4d9337437f")
BIREFNET = ("ZhengPeng7/BiRefNet", "e2bf8e4460fc8fa32bba5ea4d94b3233d367b0e4")
DINOV3 = ("camenduru/dinov3-vitl16-pretrain-lvd1689m", "3c276edd87d6f6e569ff0c4400e086807d0f3881")
MOGE = ("Ruicheng/moge-2-vitl", "39c4d5e957afe587e04eec59dc2bcc3be5ecd968")
# The checkpoints single-image inference loads (Pixal3D's download-local.sh).
CKPTS = [
    "ss_dec_conv3d_16l8_fp16",
    "ss_flow_img_dit_1_3B_64_bf16",
    "shape_dec_next_dc_f16c32_fp16",
    "slat_flow_img2shape_dit_1_3B_512_bf16",
    "slat_flow_img2shape_dit_1_3B_1024_bf16",
    "tex_dec_next_dc_f16c32_fp16",
    "slat_flow_imgshape2tex_dit_1_3B_1024_bf16",
]
# Pixal3D's defaults (`inference.py`): resolution, token limit, sampler shape per stage.
DEFAULT_RESOLUTION = 1024
MAX_NUM_TOKENS = 49152
SS_SAMPLER = {"guidance_rescale": 0.7, "rescale_t": 5.0}
SHAPE_SAMPLER = {"guidance_rescale": 0.5, "rescale_t": 3.0}
TEX_SAMPLER = {"guidance_strength": 1.0, "guidance_rescale": 0.0, "rescale_t": 3.0}
# Pixal3D's frame to glTF's Y-up (`inference.py`).
TO_GLTF = np.array([[-1, 0, 0, 0], [0, 0, -1, 0], [0, -1, 0, 0], [0, 0, 0, 1]], dtype=np.float64)

_paths = {}


def prepare(request):
    pixal = common.snapshot(
        *PIXAL3D,
        allow_patterns=["pipeline.json"]
        + [f"ckpts/{name}.{ext}" for name in CKPTS for ext in ("json", "safetensors")],
    )
    _paths["birefnet"] = common.snapshot(*BIREFNET)
    _paths["dinov3"] = common.snapshot(*DINOV3)
    _paths["moge"] = os.path.join(common.snapshot(*MOGE), "model.pt")

    def edit(config):
        config["args"]["rembg_model"]["args"]["model_name"] = _paths["birefnet"]
        config["args"]["image_cond_model"]["args"]["model_name"] = _paths["dinov3"]

    _paths["folder"] = common.model_folder(request, "pixal3d", pixal, edit)
    common.offline()


def run(request, stage):
    common.source(request, "Pixal3D")
    import torch
    # FlexGEMM reads its autotune cache path once, on import: import it before Pixal3D's
    # `inference.py`, which points that path into its own checkout.
    import flex_gemm  # noqa: F401
    import inference as pixal  # Pixal3D's own helpers: model loading and camera estimation

    for config in pixal.IMAGE_COND_CONFIGS.values():
        config["model_name"] = _paths["dinov3"]

    stage("load")
    pipeline = pixal.init_pipeline(_paths["folder"], low_vram=bool(request["lowVram"]))

    stage("preprocess")
    picture = common.load_picture(request["image"])
    preprocessed = pipeline.preprocess_image(picture)
    preprocessed_path = os.path.join(request["workDir"], "preprocessed.png")
    preprocessed.save(preprocessed_path)
    moge = pixal.load_moge_model(device="cuda", model_name=_paths["moge"])
    camera = pixal.get_camera_params_wild_moge(
        preprocessed_path, moge, device="cuda", mesh_scale=1.0, extend_pixel=0, image_resolution=512
    )
    common.log(f"camera: {math.degrees(camera['camera_angle_x']):.1f}° at {camera['distance']:.3f}")
    moge.cpu()
    del moge
    torch.cuda.empty_cache()

    stage("generate")
    seed = int(request["seed"])
    torch.manual_seed(seed)
    steps = int(request["steps"])
    guidance = float(request["guidance"])
    resolution = int(request.get("resolution") or DEFAULT_RESOLUTION)
    meshes, (_, _, grid) = pipeline.run(
        preprocessed,
        camera_params=camera,
        seed=seed,
        sparse_structure_sampler_params={"steps": steps, "guidance_strength": guidance, **SS_SAMPLER},
        shape_slat_sampler_params={"steps": steps, "guidance_strength": guidance, **SHAPE_SAMPLER},
        tex_slat_sampler_params={"steps": steps, **TEX_SAMPLER},
        preprocess_image=False,
        return_latent=True,
        pipeline_type=f"{resolution}_cascade",
        max_num_tokens=MAX_NUM_TOKENS,
    )

    stage("export")
    return common.export_voxel_mesh(
        request, meshes[0], pipeline.pbr_attr_layout, grid_size=grid, transform=TO_GLTF
    )
