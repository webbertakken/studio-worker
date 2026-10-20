"""Hunyuan3D 2.1 (tencent/Hunyuan3D-2.1).

Licence: Tencent Hunyuan 3D 2.1 Community License; its Territory excludes the EU, the UK and South
Korea. This adapter ships none of Tencent's material: the operator's machine pulls the code and
the weights, and the operator accepts Tencent's terms by pulling them. Webber uses it for personal
use only, off every public and commercial path (DECISIONS.md, "Image to 3D").

Shape: the flow-matching DiT, then floaters, degenerate faces and the face count trimmed.
Texture: the PBR paint pipeline with 6 views of 512 px and its diffusion model offloaded to the
CPU, so the paint pass fits a 16 GiB budget (Webber, 2026-10-20). The GLB is written here, from the
painted OBJ and its maps: Hunyuan's own conversion needs Blender (`bpy`), which is not installed.
"""

import os
import sys
import types

from . import common

HUNYUAN3D = ("tencent/Hunyuan3D-2.1", "0b94677654c57bb9a6b6845cd7b704ccf551d327")
DINOV2_GIANT = ("facebook/dinov2-giant", "611a9d42f2335e0f921f1e313ad3c1b7178d206d")
REALESRGAN = (
    "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
    "4fa0d38905f75ac06eb49a7951b426670021be3018265fd191d2125df9d682f1",
)
SHAPE_DIR = "hunyuan3d-dit-v2-1"
# Shape sampling (Hunyuan's gradio defaults): octree resolution unless the model sets one.
DEFAULT_OCTREE_RESOLUTION = 256
NUM_CHUNKS = 8000
# Paint (Webber, 2026-10-20): 6 views of 512 px; views rendered at 1024 px for baking.
PAINT_VIEWS = 6
PAINT_VIEW_PX = 512
PAINT_RENDER_PX = 1024

_paths = {}


def prepare(request):
    _paths["hunyuan"] = common.snapshot(
        *HUNYUAN3D, allow_patterns=[f"{SHAPE_DIR}/*", "hunyuan3d-paintpbr-v2-1/*"]
    )
    _paths["dinov2"] = common.snapshot(*DINOV2_GIANT)
    _paths["realesrgan"] = common.download(
        *REALESRGAN, os.path.join(request["pipelinesDir"], "hunyuan3d", "RealESRGAN_x4plus.pth")
    )
    common.offline()


def _import_paths(request):
    root = request["sources"]["Hunyuan3D-2.1"]
    for path in (os.path.join(root, "hy3dshape"), os.path.join(root, "hy3dpaint"), root):
        if path not in sys.path:
            sys.path.insert(0, path)
    # Hunyuan's GLB conversion imports Blender at module level; it is never called here.
    sys.modules.setdefault("bpy", types.ModuleType("bpy"))
    from utils.torchvision_fix import apply_fix  # basicsr needs torchvision's old module path

    apply_fix()
    return root


def _shape(request, torch, picture):
    from hy3dshape import DegenerateFaceRemover, FaceReducer, FloaterRemover
    from hy3dshape.pipelines import Hunyuan3DDiTFlowMatchingPipeline
    from hy3dshape.rembg import BackgroundRemover

    if picture.mode == "RGB":
        picture = BackgroundRemover()(picture)
    folder = os.path.join(_paths["hunyuan"], SHAPE_DIR)
    pipeline = Hunyuan3DDiTFlowMatchingPipeline.from_single_file(
        os.path.join(folder, "model.fp16.ckpt"),
        os.path.join(folder, "config.yaml"),
        device="cuda",
        dtype=torch.float16,
    )
    generator = torch.Generator().manual_seed(int(request["seed"]))
    mesh = pipeline(
        image=picture,
        num_inference_steps=int(request["steps"]),
        guidance_scale=float(request["guidance"]),
        generator=generator,
        octree_resolution=int(request.get("resolution") or DEFAULT_OCTREE_RESOLUTION),
        num_chunks=NUM_CHUNKS,
        output_type="trimesh",
    )[0]
    del pipeline
    torch.cuda.empty_cache()
    mesh = FloaterRemover()(mesh)
    mesh = DegenerateFaceRemover()(mesh)
    return FaceReducer()(mesh, max_facenum=int(request["decimationTarget"]))


def _paint(request, root, torch, shape_path, picture):
    from textureGenPipeline import Hunyuan3DPaintConfig, Hunyuan3DPaintPipeline
    from utils import multiview_utils

    config = Hunyuan3DPaintConfig(max_num_view=PAINT_VIEWS, resolution=PAINT_VIEW_PX)
    config.multiview_cfg_path = os.path.join(root, "hy3dpaint", "cfgs", "hunyuan-paint-pbr.yaml")
    config.realesrgan_ckpt_path = _paths["realesrgan"]
    config.dino_ckpt_path = _paths["dinov2"]
    config.render_size = PAINT_RENDER_PX
    # The painted maps are saved at half the bake size: bake at twice the GLB's.
    config.texture_size = 2 * int(request["textureSize"])
    # The paint model is read from the pinned snapshot, never from the hub.
    multiview_utils.huggingface_hub.snapshot_download = lambda **_: _paths["hunyuan"]
    paint = Hunyuan3DPaintPipeline(config)
    paint.models["multiview_model"].pipeline.enable_model_cpu_offload()
    torch.cuda.empty_cache()
    painted = os.path.join(request["workDir"], "painted", "textured_mesh.obj")
    os.makedirs(os.path.dirname(painted), exist_ok=True)
    paint(mesh_path=shape_path, image_path=picture, output_mesh_path=painted, use_remesh=False,
          save_glb=False)
    return painted


def _glb(request, painted):
    """The painted OBJ and its maps as a GLB: base colour, and metallic-roughness packed the glTF
    way (roughness in green, metallic in blue)."""
    import numpy as np
    import trimesh
    from PIL import Image

    mesh = trimesh.load(painted, force="mesh", process=False)
    base = os.path.splitext(painted)[0]
    colour = Image.open(f"{base}.jpg").convert("RGB")
    material = trimesh.visual.material.PBRMaterial(baseColorTexture=colour, metallicFactor=1.0,
                                                   roughnessFactor=1.0)
    if os.path.exists(f"{base}_metallic.jpg") and os.path.exists(f"{base}_roughness.jpg"):
        metallic = np.asarray(Image.open(f"{base}_metallic.jpg").convert("L"))
        roughness = np.asarray(Image.open(f"{base}_roughness.jpg").convert("L"))
        packed = np.stack([np.zeros_like(roughness), roughness, metallic], axis=-1)
        material.metallicRoughnessTexture = Image.fromarray(packed, "RGB")
    else:
        material.metallicFactor = 0.0
    mesh.visual = trimesh.visual.TextureVisuals(uv=mesh.visual.uv, material=material)
    mesh.export(request["output"], file_type="glb")
    return len(mesh.faces)


def run(request, stage):
    root = _import_paths(request)
    import torch

    stage("load")
    picture = common.load_picture(request["image"])

    stage("generate")
    shape = _shape(request, torch, picture)
    shape_path = os.path.join(request["workDir"], "shape.obj")
    shape.export(shape_path)
    common.log(f"shape: {len(shape.faces)} faces")

    stage("paint")
    painted = _paint(request, root, torch, shape_path, picture)

    stage("export")
    return _glb(request, painted)
