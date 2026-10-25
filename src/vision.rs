//! Pictures a chat model reads with its messages (the studio's `images` on an `llm` task).
//!
//! A model reads images when its model source carries a vision projector: a file with the
//! `text-encoder-vision` role (llama.cpp's `mmproj`).  Every other model refuses a chat task
//! that carries images, so a picture is never silently dropped and answered about unseen.
//! The pictures arrive as absolute URLs (the studio's job inputs) or inline `data:` URLs;
//! each is fetched, bounded and decoded to RGB here, before any engine sees it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use crate::types::{ChatMessage, ModelFileRole, ModelSource, Task};

/// Tracing target for image inputs.
const TRACE_TARGET: &str = "studio_worker::vision";

/// Most pictures one chat reads.  The Glimpse pre-screen sends six (the full picture and five
/// styles' state 0); each costs several hundred context tokens.  Safe range 1..=64.
pub const MAX_IMAGES: usize = 16;

/// Largest picture fetched, in bytes.  Safe range 1 MiB..=256 MiB.
pub const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

/// How long one picture's fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a chat task with images is refused before it runs.  Permanent: no retry or other worker
/// can make the same model read them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageRefusal {
    #[error(
        "model `{model}` reads no images: its model source has no vision projector \
         (a `text-encoder-vision` file)"
    )]
    Unsupported { model: String },
    #[error("the task carries {count} images; a chat reads at most {MAX_IMAGES}")]
    TooMany { count: usize },
}

/// Whether a model with `source` reads images: it carries a vision projector.
pub fn reads_images(source: &ModelSource) -> bool {
    source
        .files
        .iter()
        .any(|f| f.role == ModelFileRole::TextEncoderVision)
}

/// The images a task carries: the `llm` task's, none for any other kind.
pub fn images_of(task: &Task) -> &[String] {
    match task {
        Task::Llm(p) => &p.images,
        _ => &[],
    }
}

/// Refuse `task` on `model` when it carries images the model cannot read (`source` is `None`
/// for a model known only by a local file, which has no projector), or too many of them.
pub fn check(model: &str, task: &Task, source: Option<&ModelSource>) -> Result<(), ImageRefusal> {
    let count = images_of(task).len();
    if count == 0 {
        return Ok(());
    }
    if !source.is_some_and(reads_images) {
        return Err(ImageRefusal::Unsupported {
            model: model.to_string(),
        });
    }
    if count > MAX_IMAGES {
        return Err(ImageRefusal::TooMany { count });
    }
    Ok(())
}

/// The vision projector among a model's downloaded files: the `text-encoder-vision` file.
pub fn projector(files: &[(ModelFileRole, PathBuf)]) -> Option<&Path> {
    files
        .iter()
        .find(|(role, _)| *role == ModelFileRole::TextEncoderVision)
        .map(|(_, path)| path.as_path())
}

/// A decoded picture: `pixels` holds `width * height` RGB triples, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decode a PNG, JPEG or WebP picture to RGB (alpha dropped).
pub fn decode_rgb(bytes: &[u8]) -> Result<RgbImage> {
    let image = image::load_from_memory(bytes).context("decoding an input image")?;
    let rgb = image.to_rgb8();
    Ok(RgbImage {
        width: rgb.width(),
        height: rgb.height(),
        pixels: rgb.into_raw(),
    })
}

/// The bytes of one picture: an inline `data:image/...;base64,` URL, or an `https` (loopback
/// `http`) URL fetched with no credentials, at most [`MAX_IMAGE_BYTES`].
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    if url.starts_with("data:") {
        let bytes = crate::engine::download::decode_image_data_url(url)?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            bail!(
                "the inline input image is {} bytes; at most {MAX_IMAGE_BYTES}",
                bytes.len()
            );
        }
        return Ok(bytes);
    }
    crate::net::validate_download_url(url, "input image")?;
    let client = crate::net::http_client()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("studio-worker/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the input image client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetching input image {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("fetching input image {url}: HTTP {status}");
    }
    if let Some(len) = response.content_length() {
        if len > MAX_IMAGE_BYTES {
            bail!("input image {url} is {len} bytes; at most {MAX_IMAGE_BYTES}");
        }
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading input image {url}"))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!("input image {url} is larger than {MAX_IMAGE_BYTES} bytes");
    }
    if bytes.is_empty() {
        bail!("input image {url} is empty");
    }
    Ok(bytes)
}

/// Fetch and decode every picture, in order; the first failure fails them all.
pub fn load_all(urls: &[String]) -> Result<Vec<RgbImage>> {
    let started = std::time::Instant::now();
    let mut images = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        let image = fetch(url)
            .and_then(|bytes| decode_rgb(&bytes))
            .with_context(|| format!("input image {} of {}", index + 1, urls.len()))
            .inspect_err(|e| {
                warn!(
                    target: TRACE_TARGET,
                    op = "images",
                    index,
                    count = urls.len(),
                    error = %format!("{e:#}"),
                    "an input image could not be read"
                );
            })?;
        images.push(image);
    }
    let sizes: Vec<String> = images
        .iter()
        .map(|i| format!("{}x{}", i.width, i.height))
        .collect();
    info!(
        target: TRACE_TARGET,
        op = "images",
        count = images.len(),
        sizes = %sizes.join(","),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "input images read"
    );
    Ok(images)
}

/// `messages` with `count` media markers at the start of the last user message, one line each,
/// so the backend puts the pictures there in order.  Refused when no user message exists.
pub fn with_markers(
    mut messages: Vec<ChatMessage>,
    marker: &str,
    count: usize,
) -> Result<Vec<ChatMessage>> {
    if count == 0 {
        return Ok(messages);
    }
    let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") else {
        bail!("the task carries images but no user message to read them with");
    };
    let mut content = format!("{marker}\n").repeat(count);
    content.push_str(&last_user.content);
    last_user.content = content;
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LlmParams, ModelCliDefaults, ModelEngine, ModelFile};

    fn file(role: ModelFileRole, name: &str) -> ModelFile {
        ModelFile {
            role,
            url: format!("https://models.invalid/{name}"),
            filename: name.into(),
            approx_bytes: None,
            sha256: None,
        }
    }

    fn source(files: Vec<ModelFile>) -> ModelSource {
        ModelSource {
            engine: ModelEngine::LlamaCpp,
            files,
            cli_defaults: ModelCliDefaults::default(),
        }
    }

    fn vision_source() -> ModelSource {
        source(vec![
            file(ModelFileRole::Model, "m.gguf"),
            file(ModelFileRole::TextEncoderVision, "m-mmproj.gguf"),
        ])
    }

    fn chat(images: usize) -> Task {
        Task::Llm(LlmParams {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "what is it?".into(),
            }],
            images: (0..images)
                .map(|i| format!("data:image/png;base64,{i}"))
                .collect(),
            ..Default::default()
        })
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn data_url(bytes: &[u8]) -> String {
        use base64::Engine as _;
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    #[test]
    fn a_model_reads_images_only_with_a_vision_projector() {
        assert!(reads_images(&vision_source()));
        assert!(!reads_images(&source(vec![file(
            ModelFileRole::Model,
            "m.gguf"
        )])));
    }

    #[test]
    fn a_chat_without_images_passes_on_any_model() {
        assert_eq!(check("m", &chat(0), None), Ok(()));
        assert_eq!(check("m", &chat(0), Some(&source(vec![]))), Ok(()));
    }

    #[test]
    fn images_are_refused_on_a_model_without_a_projector() {
        let refusal = check("text-only", &chat(1), Some(&source(vec![]))).unwrap_err();
        assert_eq!(
            refusal,
            ImageRefusal::Unsupported {
                model: "text-only".into()
            }
        );
        assert!(refusal.to_string().contains("reads no images"), "{refusal}");
    }

    #[test]
    fn images_are_refused_on_a_model_known_only_by_its_file() {
        assert!(matches!(
            check("local", &chat(2), None),
            Err(ImageRefusal::Unsupported { .. })
        ));
    }

    #[test]
    fn images_are_read_on_a_model_with_a_projector() {
        assert_eq!(
            check("vl", &chat(MAX_IMAGES), Some(&vision_source())),
            Ok(())
        );
    }

    #[test]
    fn more_than_the_most_images_is_refused() {
        assert_eq!(
            check("vl", &chat(MAX_IMAGES + 1), Some(&vision_source())),
            Err(ImageRefusal::TooMany {
                count: MAX_IMAGES + 1
            })
        );
    }

    #[test]
    fn other_kinds_carry_no_images() {
        let image = Task::Image(Default::default());
        assert!(images_of(&image).is_empty());
        assert_eq!(check("m", &image, None), Ok(()));
    }

    #[test]
    fn the_projector_is_the_text_encoder_vision_file() {
        let files = vec![
            (ModelFileRole::Model, PathBuf::from("/m/m.gguf")),
            (ModelFileRole::TextEncoderVision, PathBuf::from("/m/p.gguf")),
        ];
        assert_eq!(projector(&files), Some(Path::new("/m/p.gguf")));
        assert_eq!(projector(&files[..1]), None);
    }

    #[test]
    fn a_png_decodes_to_rgb_triples() {
        let rgb = decode_rgb(&png(3, 2)).unwrap();
        assert_eq!((rgb.width, rgb.height), (3, 2));
        assert_eq!(rgb.pixels.len(), 3 * 2 * 3);
        assert_eq!(&rgb.pixels[..3], &[10, 20, 30]);
    }

    #[test]
    fn a_webp_decodes_to_rgb_triples() {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lossy-vp8.webp"),
        )
        .unwrap();
        let rgb = decode_rgb(&bytes).unwrap();
        assert_eq!(
            rgb.pixels.len(),
            (rgb.width * rgb.height * 3) as usize,
            "{}x{}",
            rgb.width,
            rgb.height
        );
    }

    #[test]
    fn bytes_that_are_no_picture_are_refused() {
        let err = decode_rgb(b"not a picture").unwrap_err();
        assert!(err.to_string().contains("decoding an input image"), "{err}");
    }

    #[test]
    fn an_inline_picture_is_read_without_a_request() {
        let bytes = png(1, 1);
        assert_eq!(fetch(&data_url(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn a_plaintext_remote_url_is_refused_before_any_request() {
        let err = fetch("http://example.invalid/a.png").unwrap_err();
        assert!(err.to_string().contains("must use https"), "{err}");
    }

    #[test]
    fn markers_lead_the_last_user_message_in_order() {
        let messages = vec![
            ChatMessage {
                role: "user".into(),
                content: "first".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "ok".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "what are these?".into(),
            },
        ];
        let out = with_markers(messages, "<m>", 2).unwrap();
        assert_eq!(out[0].content, "first");
        assert_eq!(out[1].content, "ok");
        assert_eq!(out[2].content, "<m>\n<m>\nwhat are these?");
    }

    #[test]
    fn no_images_leave_the_messages_alone() {
        let messages = vec![ChatMessage {
            role: "system".into(),
            content: "s".into(),
        }];
        assert_eq!(
            with_markers(messages.clone(), "<m>", 0).unwrap()[0].content,
            "s"
        );
    }

    #[test]
    fn images_without_a_user_message_are_refused() {
        let messages = vec![ChatMessage {
            role: "system".into(),
            content: "s".into(),
        }];
        let err = with_markers(messages, "<m>", 1).unwrap_err();
        assert!(err.to_string().contains("no user message"), "{err}");
    }

    #[test]
    fn every_picture_is_read_in_order() {
        let images = load_all(&[data_url(&png(2, 1)), data_url(&png(1, 3))]).unwrap();
        let sizes: Vec<(u32, u32)> = images.iter().map(|i| (i.width, i.height)).collect();
        assert_eq!(sizes, vec![(2, 1), (1, 3)]);
    }

    #[test]
    fn one_unreadable_picture_fails_them_all_and_says_which() {
        let logs = crate::test_support::capture(|| {
            let err = load_all(&[data_url(&png(1, 1)), data_url(b"junk")]).unwrap_err();
            assert!(format!("{err:#}").contains("input image 2 of 2"), "{err:#}");
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("op=\"images\""), "{logs}");
    }
}
