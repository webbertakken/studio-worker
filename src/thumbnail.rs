//! Thumbnails of image jobs, so the tray UI can show what a job made.
//!
//! The daemon decodes an image result, scales it to fit
//! [`THUMBNAIL_MAX_SIDE`] and keeps it as PNG in a bounded [`Thumbnails`]
//! ring keyed by job id.  The tray UI keeps its own copy (fetched from
//! `GET /jobs/:id/thumbnail`) in the same type.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::Arc;

use anyhow::Context as _;
use parking_lot::Mutex;

/// Longer side of a thumbnail, in pixels.  Crisp at the UI's 96 px card
/// size on a 2x display.
pub const THUMBNAIL_MAX_SIDE: u32 = 192;

/// Thumbnails of this many most recent image jobs are kept: the size of
/// the studio and local job rings together.
pub const THUMBNAILS_CAP: usize = 2 * crate::runtime::RECENT_JOBS_CAP;

/// Scale an encoded image (WEBP / PNG / JPEG) to fit
/// [`THUMBNAIL_MAX_SIDE`] and encode it as PNG.  Smaller images keep their
/// size.
pub fn make_thumbnail(encoded: &[u8]) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory(encoded).context("decoding the job's image")?;
    let image = if image.width() > THUMBNAIL_MAX_SIDE || image.height() > THUMBNAIL_MAX_SIDE {
        image.thumbnail(THUMBNAIL_MAX_SIDE, THUMBNAIL_MAX_SIDE)
    } else {
        image
    };
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("encoding the thumbnail")?;
    Ok(png)
}

/// A job id and its PNG thumbnail.
type Entry = (String, Arc<Vec<u8>>);

/// Bounded ring of PNG thumbnails keyed by job id, oldest evicted first.
/// Cheap to clone.
#[derive(Clone, Default)]
pub struct Thumbnails {
    inner: Arc<Mutex<VecDeque<Entry>>>,
}

impl Thumbnails {
    /// Keep `png` as `job_id`'s thumbnail, replacing an older one.
    pub fn insert(&self, job_id: &str, png: Vec<u8>) {
        let mut ring = self.inner.lock();
        ring.retain(|(id, _)| id != job_id);
        ring.push_back((job_id.to_string(), Arc::new(png)));
        while ring.len() > THUMBNAILS_CAP {
            ring.pop_front();
        }
    }

    /// The thumbnail of `job_id`.
    pub fn get(&self, job_id: &str) -> Option<Arc<Vec<u8>>> {
        self.inner
            .lock()
            .iter()
            .find(|(id, _)| id == job_id)
            .map(|(_, png)| png.clone())
    }

    /// Whether `job_id` has a thumbnail.
    pub fn contains(&self, job_id: &str) -> bool {
        self.inner.lock().iter().any(|(id, _)| id == job_id)
    }

    /// Drop every thumbnail whose job id fails `keep`.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.inner.lock().retain(|(id, _)| keep(id));
    }

    /// Drop every thumbnail.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// Number of thumbnails kept.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// True when no thumbnail is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut out), format)
            .unwrap();
        out
    }

    fn dimensions(png: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn a_large_image_is_scaled_to_fit_keeping_its_aspect() {
        let png = make_thumbnail(&encoded(1024, 512, image::ImageFormat::WebP)).unwrap();
        assert_eq!(
            dimensions(&png),
            (THUMBNAIL_MAX_SIDE, THUMBNAIL_MAX_SIDE / 2)
        );
    }

    #[test]
    fn a_small_image_keeps_its_size() {
        let png = make_thumbnail(&encoded(64, 48, image::ImageFormat::Png)).unwrap();
        assert_eq!(dimensions(&png), (64, 48));
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused() {
        let err = make_thumbnail(b"not an image").unwrap_err();
        assert!(err.to_string().contains("decoding"), "got {err:#}");
    }

    #[test]
    fn the_ring_keeps_only_the_most_recent_thumbnails() {
        let thumbnails = Thumbnails::default();
        for i in 0..(THUMBNAILS_CAP + 2) {
            thumbnails.insert(&format!("job-{i}"), vec![i as u8]);
        }
        assert_eq!(thumbnails.len(), THUMBNAILS_CAP);
        assert!(!thumbnails.contains("job-0"));
        assert!(!thumbnails.contains("job-1"));
        assert_eq!(
            thumbnails
                .get(&format!("job-{}", THUMBNAILS_CAP + 1))
                .unwrap()[0],
            (THUMBNAILS_CAP + 1) as u8
        );
    }

    #[test]
    fn inserting_again_replaces_the_thumbnail() {
        let thumbnails = Thumbnails::default();
        thumbnails.insert("a", vec![1]);
        thumbnails.insert("a", vec![2]);
        assert_eq!(thumbnails.len(), 1);
        assert_eq!(*thumbnails.get("a").unwrap(), vec![2]);
    }

    #[test]
    fn retain_and_clear_drop_thumbnails() {
        let thumbnails = Thumbnails::default();
        thumbnails.insert("keep", vec![1]);
        thumbnails.insert("drop", vec![2]);
        thumbnails.retain(|id| id == "keep");
        assert!(thumbnails.contains("keep") && !thumbnails.contains("drop"));
        thumbnails.clear();
        assert!(thumbnails.is_empty());
    }
}
