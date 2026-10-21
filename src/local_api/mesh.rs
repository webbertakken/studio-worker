//! `POST /mesh`: a picture in, a 3D model (GLB) out (`docs/engines/torch.md`).
//!
//! The picture and its options (`model`, `seed`) arrive in any of the forms [`super::picture`]
//! reads.  The job runs like every transient local job: one at a time, admitted next to the loaded
//! models, recorded in the local-jobs ring.

use base64::Engine as _;
use tiny_http::Request;

use super::picture::{self, number, PictureRequest, Refusal};
use super::{LocalApi, MAX_IMAGE_BODY_BYTES};
use crate::local::run_kind;
use crate::types::{MeshParams, Task, TaskKind, TaskResult};

/// The options `POST /mesh` reads.
const OPTIONS: &[&str] = &["model", "seed"];

/// The task a mesh request runs: the picture inline as a data URL, the seed if given.
fn mesh_task(request: &PictureRequest) -> Result<Task, Refusal> {
    let extension = crate::engine::download::sniff_image_extension(&request.picture)
        .ok_or_else(|| Refusal::bad("the picture is not a PNG, JPEG or WebP"))?;
    crate::vision::decode_rgb(&request.picture)
        .map_err(|e| Refusal::bad(format!("the picture cannot be read: {e:#}")))?;
    let media = if extension == "jpg" {
        "jpeg"
    } else {
        extension
    };
    Ok(Task::Mesh(MeshParams {
        image_url: format!(
            "data:image/{media};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&request.picture)
        ),
        seed: number::<u64>(&request.options, "seed")?,
        ext: "glb".into(),
    }))
}

impl LocalApi {
    pub(super) fn handle_mesh(&self, mut request: Request) -> std::io::Result<()> {
        let body = match super::read_bytes_capped(&mut request, MAX_IMAGE_BODY_BYTES)? {
            Some(body) => body,
            None => return super::respond_too_large_capped(request, MAX_IMAGE_BODY_BYTES),
        };
        let content_type = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("content-type"))
            .map(|h| h.value.as_str().to_string());
        let query = request.url().split_once('?').map(|(_, q)| q.to_string());
        let parsed = match picture::parse(OPTIONS, content_type.as_deref(), query.as_deref(), body)
        {
            Ok(parsed) => parsed,
            Err(refusal) => return picture::refuse(request, "mesh", refusal),
        };
        let task = match mesh_task(&parsed) {
            Ok(task) => task,
            Err(refusal) => return picture::refuse(request, "mesh", refusal),
        };
        let Some(_reservation) = self.gate.try_reserve() else {
            return super::respond_busy(request);
        };
        let catalog = self.catalog.lock().clone();
        let model = parsed.options.get("model").map(String::as_str);
        match run_kind(
            self.engine.as_ref(),
            &catalog,
            &self.observers,
            TaskKind::Mesh,
            model,
            "",
            task,
        ) {
            Ok(TaskResult::Mesh { bytes, ext }) => {
                super::respond(request, 200, crate::http::mime_for_ext(&ext), &bytes)
            }
            Ok(_) => super::respond(request, 500, "text/plain", b"unexpected non-mesh result"),
            Err(err) => super::respond_local_err(request, err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(2, 2, image::Rgb([9, 9, 9]))
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn request(picture: Vec<u8>, options: &[(&str, &str)]) -> PictureRequest {
        PictureRequest {
            picture,
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn a_picture_becomes_an_inline_mesh_task_with_its_seed() {
        let picture = png();
        let Task::Mesh(params) = mesh_task(&request(picture.clone(), &[("seed", "42")])).unwrap()
        else {
            panic!("expected a mesh task")
        };
        assert_eq!(params.seed, Some(42));
        assert_eq!(params.ext, "glb");
        let decoded = crate::engine::download::decode_image_data_url(&params.image_url).unwrap();
        assert_eq!(decoded, picture);
        assert!(params.image_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn what_is_not_a_picture_is_refused() {
        let refusal = mesh_task(&request(b"not a picture".to_vec(), &[])).unwrap_err();
        assert_eq!(refusal.status, 400);
        assert!(refusal.message.contains("not a PNG, JPEG or WebP"));
        let mut broken = png();
        broken.truncate(20);
        let refusal = mesh_task(&request(broken, &[])).unwrap_err();
        assert!(
            refusal.message.contains("cannot be read"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_seed_that_is_not_a_number_is_refused() {
        let refusal = mesh_task(&request(png(), &[("seed", "soon")])).unwrap_err();
        assert!(refusal.message.contains("seed"), "{}", refusal.message);
    }
}
