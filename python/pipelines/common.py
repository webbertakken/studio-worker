"""What every pipeline shares: weights at pinned revisions, model folders, the picture, the GLB."""

import hashlib
import json
import os
import sys
import urllib.request

from PIL import Image


class BadInput(Exception):
    """The picture cannot be used: the job would fail the same way on any worker."""


# The smallest picture a pipeline is given (each side, in pixels).
MIN_PICTURE_PX = 64


def log(message):
    print(f"[pipeline] {message}", file=sys.stderr, flush=True)


def source(request, name):
    """A source checkout of the environment, put first on the import path."""
    path = request["sources"][name]
    if path not in sys.path:
        sys.path.insert(0, path)
    return path


def snapshot(repo, revision, allow_patterns=None):
    """A Hugging Face snapshot at a pinned revision in HF_HOME: the cache when it holds it, else
    downloaded.  Returns its local directory."""
    from huggingface_hub import snapshot_download

    try:
        return snapshot_download(
            repo, revision=revision, allow_patterns=allow_patterns, local_files_only=True
        )
    except Exception:
        log(f"downloading {repo}@{revision[:8]}")
        return snapshot_download(repo, revision=revision, allow_patterns=allow_patterns)


def offline():
    """Every weight is local from here on: nothing may reach the hub during a run."""
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"


def model_folder(request, name, snapshot_dir, edit):
    """`<pipelines>/<name>`: a `pipeline.json` from the snapshot's, changed by `edit(config)`,
    beside a `ckpts` link to the snapshot's.  Swaps gated parts without editing upstream code."""
    folder = os.path.join(request["pipelinesDir"], name)
    os.makedirs(folder, exist_ok=True)
    with open(os.path.join(snapshot_dir, "pipeline.json"), encoding="utf-8") as file:
        config = json.load(file)
    edit(config)
    partial = os.path.join(folder, "pipeline.json.partial")
    with open(partial, "w", encoding="utf-8") as file:
        json.dump(config, file, indent=2)
    os.replace(partial, os.path.join(folder, "pipeline.json"))
    ckpts = os.path.join(folder, "ckpts")
    target = os.path.join(snapshot_dir, "ckpts")
    if os.path.islink(ckpts) and os.readlink(ckpts) != target:
        os.remove(ckpts)
    if not os.path.lexists(ckpts):
        os.symlink(target, ckpts)
    return folder


def download(url, sha256, dest):
    """A file from `url`, checked against `sha256`, kept at `dest`."""
    if os.path.isfile(dest) and file_sha256(dest) == sha256:
        return dest
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    partial = dest + ".partial"
    log(f"downloading {url}")
    with urllib.request.urlopen(url, timeout=600) as response, open(partial, "wb") as file:
        while chunk := response.read(1 << 20):
            file.write(chunk)
    actual = file_sha256(partial)
    if actual != sha256:
        os.remove(partial)
        raise RuntimeError(f"{url} hashes to {actual}, expected {sha256}")
    os.replace(partial, dest)
    return dest


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        while chunk := file.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def load_picture(path):
    """The job's picture, fully decoded; RGBA when it carries transparency (which then stands in for
    background removal), RGB otherwise."""
    try:
        image = Image.open(path)
        image.load()
    except Exception as error:
        raise BadInput(f"the picture cannot be decoded: {error}") from error
    if min(image.size) < MIN_PICTURE_PX:
        raise BadInput(f"the picture is {image.size[0]}x{image.size[1]}; at least "
                       f"{MIN_PICTURE_PX} pixels a side")
    has_alpha = image.mode in ("RGBA", "LA") or (image.mode == "P" and "transparency" in image.info)
    return image.convert("RGBA" if has_alpha else "RGB")


def sampler(request, steps_key="steps", guidance=None):
    """A flow sampler's overrides from the request: its steps, and its guidance when given."""
    params = {"steps": int(request[steps_key])}
    if guidance is not None:
        params["guidance_strength"] = float(guidance)
    return params


def export_voxel_mesh(request, mesh, attr_layout, grid_size=None, voxel_size=None, transform=None):
    """A TRELLIS-family mesh with its voxel attributes, as a web-sized GLB at the request's
    output: `decimationTarget` triangles, `textureSize` pixels.  Returns its triangle count."""
    import o_voxel

    glb = o_voxel.postprocess.to_glb(
        vertices=mesh.vertices,
        faces=mesh.faces,
        attr_volume=mesh.attrs,
        coords=mesh.coords,
        attr_layout=attr_layout,
        grid_size=grid_size,
        voxel_size=voxel_size,
        aabb=[[-0.5, -0.5, -0.5], [0.5, 0.5, 0.5]],
        decimation_target=int(request["decimationTarget"]),
        texture_size=int(request["textureSize"]),
        remesh=True,
        remesh_band=1,
        remesh_project=0,
    )
    if transform is not None:
        glb.apply_transform(transform)
    glb.export(request["output"], extension_webp=True)
    return len(glb.faces)
