//! Binary glTF (GLB): the file a `mesh` job answers with.
//!
//! [`check`] reads a GLB's header, so an engine never hands back a file that is not one;
//! [`textured_cube`] writes a small, valid GLB for the synthetic engine, so a mesh job runs end
//! to end without a GPU.

use std::io::Cursor;

/// `glTF` in little-endian ASCII: the first four bytes of every GLB.
const MAGIC: u32 = 0x4654_6C67;
/// The only GLB container version (glTF 2.0).
const VERSION: u32 = 2;
/// Chunk type of the JSON chunk (`JSON`).
const CHUNK_JSON: u32 = 0x4E4F_534A;
/// Chunk type of the binary chunk (`BIN\0`).
const CHUNK_BIN: u32 = 0x004E_4942;
/// Header (magic, version, length) plus the first chunk's header (length, type).
const MIN_LEN: usize = 20;

/// Why bytes are not a GLB.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlbError {
    #[error("not a GLB: {0} bytes is shorter than a GLB header")]
    TooShort(usize),
    #[error("not a GLB: the file does not start with `glTF`")]
    Magic,
    #[error("not a GLB: container version {0}, expected 2")]
    Version(u32),
    #[error("not a GLB: the header says {declared} bytes, the file has {actual}")]
    Length { declared: usize, actual: usize },
    #[error("not a GLB: the first chunk is not JSON")]
    FirstChunk,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Check that `bytes` is a whole GLB: magic, version 2, the declared length, a JSON first chunk.
pub fn check(bytes: &[u8]) -> Result<(), GlbError> {
    if bytes.len() < MIN_LEN {
        return Err(GlbError::TooShort(bytes.len()));
    }
    if u32_at(bytes, 0) != MAGIC {
        return Err(GlbError::Magic);
    }
    let version = u32_at(bytes, 4);
    if version != VERSION {
        return Err(GlbError::Version(version));
    }
    let declared = u32_at(bytes, 8) as usize;
    if declared != bytes.len() {
        return Err(GlbError::Length {
            declared,
            actual: bytes.len(),
        });
    }
    if u32_at(bytes, 16) != CHUNK_JSON {
        return Err(GlbError::FirstChunk);
    }
    Ok(())
}

/// Pad `data` to a multiple of four bytes with `fill`.
fn padded(mut data: Vec<u8>, fill: u8) -> Vec<u8> {
    while !data.len().is_multiple_of(4) {
        data.push(fill);
    }
    data
}

/// Assemble a GLB from its JSON document and binary buffer.
fn assemble(json: &serde_json::Value, bin: Vec<u8>) -> Vec<u8> {
    let json = padded(json.to_string().into_bytes(), b' ');
    let bin = padded(bin, 0);
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    for word in [MAGIC, VERSION, total as u32, json.len() as u32, CHUNK_JSON] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
    out.extend_from_slice(&bin);
    out
}

/// The six faces of a unit cube: outward normal, then its four corners counter-clockwise.
const FACES: [([f32; 3], [[f32; 3]; 4]); 6] = [
    (
        [1.0, 0.0, 0.0],
        [
            [1.0, -1.0, 1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [1.0, 1.0, 1.0],
        ],
    ),
    (
        [-1.0, 0.0, 0.0],
        [
            [-1.0, -1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [-1.0, 1.0, 1.0],
            [-1.0, 1.0, -1.0],
        ],
    ),
    (
        [0.0, 1.0, 0.0],
        [
            [-1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
        ],
    ),
    (
        [0.0, -1.0, 0.0],
        [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, -1.0, 1.0],
            [-1.0, -1.0, 1.0],
        ],
    ),
    (
        [0.0, 0.0, 1.0],
        [
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ],
    ),
    (
        [0.0, 0.0, -1.0],
        [
            [1.0, -1.0, -1.0],
            [-1.0, -1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [1.0, 1.0, -1.0],
        ],
    ),
];
const UVS: [[f32; 2]; 4] = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
/// Pixels per side of the synthetic cube's checker texture.
const TEXTURE_PX: u32 = 64;
/// Checker cells per side of the texture.
const CHECKER_CELLS: u32 = 4;

/// A checker in `colour` and its darker half, as PNG.
fn checker_png(colour: [u8; 3]) -> Vec<u8> {
    let dark = colour.map(|channel| channel / 2);
    let cell = TEXTURE_PX / CHECKER_CELLS;
    let image = image::RgbImage::from_fn(TEXTURE_PX, TEXTURE_PX, |x, y| {
        image::Rgb(if (x / cell + y / cell).is_multiple_of(2) {
            colour
        } else {
            dark
        })
    });
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encoding a PNG to memory cannot fail");
    out.into_inner()
}

/// A unit cube with a checker texture in `colour`, as a GLB: 12 triangles, positions, normals,
/// texture coordinates and an embedded PNG.
pub fn textured_cube(colour: [u8; 3]) -> Vec<u8> {
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut uvs = Vec::new();
    let mut indices: Vec<u16> = Vec::new();
    for (face, (normal, corners)) in FACES.iter().enumerate() {
        let first = (face * 4) as u16;
        for (corner, uv) in corners.iter().zip(UVS) {
            positions.extend(corner.iter().map(|c| c * 0.5));
            normals.extend(normal);
            uvs.extend(uv);
        }
        indices.extend([first, first + 1, first + 2, first, first + 2, first + 3]);
    }
    let floats =
        |values: &[f32]| -> Vec<u8> { values.iter().flat_map(|v| v.to_le_bytes()).collect() };
    let mut bin = Vec::new();
    let mut views = Vec::new();
    let mut push_view = |bin: &mut Vec<u8>, bytes: Vec<u8>, target: Option<u32>| -> usize {
        let offset = bin.len();
        bin.extend(&bytes);
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let mut view =
            serde_json::json!({ "buffer": 0, "byteOffset": offset, "byteLength": bytes.len() });
        if let Some(target) = target {
            view["target"] = target.into();
        }
        views.push(view);
        views.len() - 1
    };
    const ARRAY_BUFFER: u32 = 34962;
    const ELEMENT_ARRAY_BUFFER: u32 = 34963;
    let position_view = push_view(&mut bin, floats(&positions), Some(ARRAY_BUFFER));
    let normal_view = push_view(&mut bin, floats(&normals), Some(ARRAY_BUFFER));
    let uv_view = push_view(&mut bin, floats(&uvs), Some(ARRAY_BUFFER));
    let index_bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let index_view = push_view(&mut bin, index_bytes, Some(ELEMENT_ARRAY_BUFFER));
    let image_view = push_view(&mut bin, checker_png(colour), None);
    const FLOAT: u32 = 5126;
    const UNSIGNED_SHORT: u32 = 5123;
    let vertex_count = positions.len() / 3;
    let json = serde_json::json!({
        "asset": { "version": "2.0", "generator": "studio-worker synthetic" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0, "name": "cube" }],
        "meshes": [{ "primitives": [{
            "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
            "indices": 3,
            "material": 0
        }] }],
        "materials": [{ "pbrMetallicRoughness": {
            "baseColorTexture": { "index": 0 }, "metallicFactor": 0.0, "roughnessFactor": 0.8
        } }],
        "textures": [{ "source": 0 }],
        "images": [{ "bufferView": image_view, "mimeType": "image/png" }],
        "accessors": [
            { "bufferView": position_view, "componentType": FLOAT, "count": vertex_count, "type": "VEC3",
              "min": [-0.5, -0.5, -0.5], "max": [0.5, 0.5, 0.5] },
            { "bufferView": normal_view, "componentType": FLOAT, "count": vertex_count, "type": "VEC3" },
            { "bufferView": uv_view, "componentType": FLOAT, "count": vertex_count, "type": "VEC2" },
            { "bufferView": index_view, "componentType": UNSIGNED_SHORT, "count": indices.len(), "type": "SCALAR" }
        ],
        "bufferViews": views,
        "buffers": [{ "byteLength": bin.len() }]
    });
    assemble(&json, bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_of(glb: &[u8]) -> serde_json::Value {
        let length = u32_at(glb, 12) as usize;
        serde_json::from_slice(&glb[20..20 + length]).unwrap()
    }

    #[test]
    fn the_synthetic_cube_is_a_whole_glb() {
        let glb = textured_cube([200, 120, 40]);
        assert_eq!(check(&glb), Ok(()));
        assert_eq!(glb.len() % 4, 0, "chunks are 4-byte aligned");
    }

    #[test]
    fn the_synthetic_cube_has_twelve_triangles_and_an_embedded_png() {
        let glb = textured_cube([10, 20, 30]);
        let json = json_of(&glb);
        assert_eq!(json["accessors"][3]["count"], 36);
        assert_eq!(json["accessors"][0]["count"], 24);
        assert_eq!(json["images"][0]["mimeType"], "image/png");
        let view = &json["bufferViews"][json["images"][0]["bufferView"].as_u64().unwrap() as usize];
        let json_len = u32_at(&glb, 12) as usize;
        let bin_start = 20 + json_len + 8;
        let offset = bin_start + view["byteOffset"].as_u64().unwrap() as usize;
        let png = &glb[offset..offset + view["byteLength"].as_u64().unwrap() as usize];
        let decoded = image::load_from_memory(png).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (TEXTURE_PX, TEXTURE_PX)
        );
    }

    #[test]
    fn the_buffer_length_matches_the_binary_chunk() {
        let glb = textured_cube([1, 2, 3]);
        let json = json_of(&glb);
        let json_len = u32_at(&glb, 12) as usize;
        let bin_len = u32_at(&glb, 20 + json_len) as usize;
        assert_eq!(
            json["buffers"][0]["byteLength"].as_u64().unwrap() as usize,
            bin_len
        );
    }

    #[test]
    fn check_refuses_what_is_not_a_glb() {
        assert_eq!(check(b"glTF"), Err(GlbError::TooShort(4)));
        assert_eq!(check(&[0u8; 20]), Err(GlbError::Magic));
        let mut glb = textured_cube([0, 0, 0]);
        glb[4] = 1;
        assert_eq!(check(&glb), Err(GlbError::Version(1)));
        let mut glb = textured_cube([0, 0, 0]);
        let actual = glb.len();
        glb.truncate(actual - 4);
        assert!(matches!(check(&glb), Err(GlbError::Length { .. })));
        let mut glb = textured_cube([0, 0, 0]);
        glb[16] = 0;
        assert_eq!(check(&glb), Err(GlbError::FirstChunk));
    }
}
