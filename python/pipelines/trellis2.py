"""TRELLIS.2 (microsoft/TRELLIS.2-4B, MIT).

Follows TRELLIS.2's `example.py` (low-VRAM, 1024 cascade), with BiRefNet in place of the gated
RMBG-2.0, the ungated DINOv3 mirror in place of `facebook/dinov3-vitl16-pretrain-lvd1689m`, and
every weight at a pinned revision.
"""

import os

from . import common

TRELLIS2 = ("microsoft/TRELLIS.2-4B", "af44b45f2e35a493886929c6d786e563ec68364d")
TRELLIS_IMAGE_LARGE = ("microsoft/TRELLIS-image-large", "25e0d31ffbebe4b5a97464dd851910efc3002d96")
SS_DECODER = "ckpts/ss_dec_conv3d_16l8_fp16"
BIREFNET = ("ZhengPeng7/BiRefNet", "e2bf8e4460fc8fa32bba5ea4d94b3233d367b0e4")
DINOV3 = ("camenduru/dinov3-vitl16-pretrain-lvd1689m", "3c276edd87d6f6e569ff0c4400e086807d0f3881")
DEFAULT_RESOLUTION = 1024
MAX_NUM_TOKENS = 49152
# nvdiffrast's limit on a mesh before export (TRELLIS.2's example.py).
MAX_FACES_BEFORE_EXPORT = 16777216

_paths = {}


def pipeline_type(resolution):
    """TRELLIS.2's pipeline for a resolution: 512 samples once; 1024 and 1536 cascade from 512."""
    return "512" if resolution <= 512 else f"{resolution}_cascade"


def prepare(request):
    trellis = common.snapshot(*TRELLIS2, allow_patterns=["pipeline.json", "ckpts/*"])
    large = common.snapshot(
        *TRELLIS_IMAGE_LARGE, allow_patterns=[f"{SS_DECODER}.json", f"{SS_DECODER}.safetensors"]
    )
    birefnet = common.snapshot(*BIREFNET)
    dinov3 = common.snapshot(*DINOV3)

    def edit(config):
        args = config["args"]
        # The sparse-structure decoder lives in TRELLIS-image-large: named by its local path.
        args["models"]["sparse_structure_decoder"] = f"{large}/{SS_DECODER}"
        args["rembg_model"]["args"]["model_name"] = birefnet
        args["image_cond_model"]["args"]["model_name"] = dinov3

    _paths["folder"] = common.model_folder(request, "trellis2", trellis, edit)
    common.offline()


def run(request, stage):
    # TRELLIS.2's sparse attention runs on flash-attn (it has no SDPA backend); set before import.
    os.environ["ATTN_BACKEND"] = "flash_attn"
    common.source(request, "TRELLIS.2")
    import torch
    from trellis2.pipelines import Trellis2ImageTo3DPipeline

    stage("load")
    pipeline = Trellis2ImageTo3DPipeline.from_pretrained(_paths["folder"])
    pipeline.low_vram = bool(request["lowVram"])
    pipeline.cuda()

    stage("preprocess")
    picture = pipeline.preprocess_image(common.load_picture(request["image"]))

    stage("generate")
    seed = int(request["seed"])
    steps = int(request["steps"])
    resolution = int(request.get("resolution") or DEFAULT_RESOLUTION)
    meshes = pipeline.run(
        picture,
        seed=seed,
        sparse_structure_sampler_params={"steps": steps, "guidance_strength": float(request["guidance"])},
        shape_slat_sampler_params={"steps": steps, "guidance_strength": float(request["guidance"])},
        tex_slat_sampler_params={"steps": steps},
        preprocess_image=False,
        pipeline_type=pipeline_type(resolution),
        max_num_tokens=MAX_NUM_TOKENS,
    )
    mesh = meshes[0]
    mesh.simplify(MAX_FACES_BEFORE_EXPORT)
    torch.cuda.empty_cache()

    stage("export")
    return common.export_voxel_mesh(request, mesh, mesh.layout, voxel_size=mesh.voxel_size)
