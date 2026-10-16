//! A picture into a network's input: one `1 x 3 x height x width` f32 tensor, planar RGB.

use image::RgbImage;

use super::BoundingBox;

/// Grey the letterbox pads with (YOLOv5's 114).
const PAD_GREY: f32 = 114.0 / 255.0;

/// How a picture was placed in a letterboxed input: scaled by `scale`, then shifted by the
/// padding.  Maps the network's coordinates back onto the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
}

impl Letterbox {
    /// A point of the input in the picture's pixels.
    pub fn to_picture(&self, x: f32, y: f32) -> (f32, f32) {
        ((x - self.pad_x) / self.scale, (y - self.pad_y) / self.scale)
    }
}

/// YOLO's letterbox: the picture scaled to fit `width` x `height` with its aspect kept, centred
/// on grey, scaled to 0..1.
pub fn letterbox(image: &RgbImage, width: u32, height: u32) -> (Vec<f32>, Letterbox) {
    let (w, h) = image.dimensions();
    let scale = (width as f32 / w as f32).min(height as f32 / h as f32);
    let inner_w = ((w as f32 * scale).round() as u32).clamp(1, width);
    let inner_h = ((h as f32 * scale).round() as u32).clamp(1, height);
    let pad_x = (width - inner_w) / 2;
    let pad_y = (height - inner_h) / 2;
    let resized = resize_bilinear(image, inner_w, inner_h);

    let plane = (width * height) as usize;
    let mut input = vec![PAD_GREY; 3 * plane];
    for (x, y, pixel) in resized.enumerate_pixels() {
        let at = ((y + pad_y) * width + x + pad_x) as usize;
        for (c, value) in pixel.0.iter().enumerate() {
            input[c * plane + at] = *value as f32 / 255.0;
        }
    }
    (
        input,
        Letterbox {
            scale,
            pad_x: pad_x as f32,
            pad_y: pad_y as f32,
        },
    )
}

/// The picture stretched to `width` x `height`, scaled to 0..1, then `(v - mean) / std` per
/// channel: the classifiers' input.
pub fn normalised(
    image: &RgbImage,
    width: u32,
    height: u32,
    mean: [f32; 3],
    std: [f32; 3],
) -> Vec<f32> {
    let resized = resize_bilinear(image, width, height);
    let plane = (width * height) as usize;
    let mut input = vec![0.0; 3 * plane];
    for (i, pixel) in resized.pixels().enumerate() {
        for c in 0..3 {
            input[c * plane + i] = (pixel.0[c] as f32 / 255.0 - mean[c]) / std[c];
        }
    }
    input
}

/// `image` scaled to `width` x `height` by plain bilinear interpolation with half-pixel centres,
/// without antialiasing: what the networks saw in training (YOLOv5's `cv2.INTER_LINEAR`,
/// SpeciesNet's `torchvision` resize with `antialias=False`).  `image`'s antialiased filters
/// widen their kernel when shrinking, which softens the input those networks expect.
pub fn resize_bilinear(image: &RgbImage, width: u32, height: u32) -> RgbImage {
    let (src_w, src_h) = image.dimensions();
    if (src_w, src_h) == (width, height) {
        return image.clone();
    }
    let scale_x = src_w as f32 / width as f32;
    let scale_y = src_h as f32 / height as f32;
    // Source position of an output pixel's centre: its two neighbours and the weight of the far one.
    let axis = |out: u32, scale: f32, len: u32| {
        let at = ((out as f32 + 0.5) * scale - 0.5).max(0.0);
        let near = (at.floor() as u32).min(len - 1);
        let far = (near + 1).min(len - 1);
        (near, far, at - near as f32)
    };
    let columns: Vec<(u32, u32, f32)> = (0..width).map(|x| axis(x, scale_x, src_w)).collect();
    let mut out = RgbImage::new(width, height);
    for y in 0..height {
        let (y0, y1, wy) = axis(y, scale_y, src_h);
        for (x, &(x0, x1, wx)) in columns.iter().enumerate() {
            let px = |x, y| image.get_pixel(x, y).0;
            let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
            let mut rgb = [0u8; 3];
            for ch in 0..3 {
                let top = a[ch] as f32 + (b[ch] as f32 - a[ch] as f32) * wx;
                let bottom = c[ch] as f32 + (d[ch] as f32 - c[ch] as f32) * wx;
                rgb[ch] = (top + (bottom - top) * wy).round().clamp(0.0, 255.0) as u8;
            }
            out.put_pixel(x as u32, y, image::Rgb(rgb));
        }
    }
    out
}

/// A planar `3 x height x width` input reordered to `height x width x 3` (NHWC networks).
pub fn channels_last(planar: &[f32], width: u32, height: u32) -> Vec<f32> {
    let plane = (width * height) as usize;
    let mut out = vec![0.0; 3 * plane];
    for i in 0..plane {
        for c in 0..3 {
            out[3 * i + c] = planar[c * plane + i];
        }
    }
    out
}

/// The part of `image` inside `area`, widened by `margin` of its size on every side (a
/// classifier recognises an animal better with a little of its surroundings), within the
/// picture.
pub fn crop(image: &RgbImage, area: &BoundingBox, margin: f32) -> RgbImage {
    let (w, h) = image.dimensions();
    let grow_x = area.width * margin;
    let grow_y = area.height * margin;
    let x0 = (area.x - grow_x).floor().clamp(0.0, (w - 1) as f32) as u32;
    let y0 = (area.y - grow_y).floor().clamp(0.0, (h - 1) as f32) as u32;
    let x1 = (area.x + area.width + grow_x).ceil().clamp(1.0, w as f32) as u32;
    let y1 = (area.y + area.height + grow_y).ceil().clamp(1.0, h as f32) as u32;
    image::imageops::crop_imm(
        image,
        x0,
        y0,
        x1.saturating_sub(x0).max(1),
        y1.saturating_sub(y0).max(1),
    )
    .to_image()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, image::Rgb(rgb))
    }

    #[test]
    fn a_wide_picture_is_padded_above_and_below() {
        let (input, letterbox) = letterbox(&solid(200, 100, [255, 0, 0]), 100, 100);
        assert_eq!(input.len(), 3 * 100 * 100);
        assert_eq!(
            letterbox,
            Letterbox {
                scale: 0.5,
                pad_x: 0.0,
                pad_y: 25.0
            }
        );
        let plane = 100 * 100;
        // The top row is padding; the middle row is the picture's red.
        assert!((input[0] - PAD_GREY).abs() < 1e-6);
        assert!((input[50 * 100] - 1.0).abs() < 1e-6);
        assert!(input[plane + 50 * 100].abs() < 1e-6);
        assert!(input[2 * plane + 50 * 100].abs() < 1e-6);
    }

    #[test]
    fn a_tall_picture_is_padded_left_and_right() {
        let (_, letterbox) = letterbox(&solid(50, 100, [0, 0, 0]), 64, 64);
        assert_eq!(letterbox.scale, 0.64);
        assert_eq!(letterbox.pad_y, 0.0);
        assert_eq!(letterbox.pad_x, 16.0);
    }

    #[test]
    fn a_small_picture_is_scaled_up() {
        let (_, letterbox) = letterbox(&solid(10, 10, [0, 0, 0]), 40, 40);
        assert_eq!(letterbox.scale, 4.0);
        assert_eq!((letterbox.pad_x, letterbox.pad_y), (0.0, 0.0));
    }

    #[test]
    fn letterbox_coordinates_map_back_onto_the_picture() {
        let letterbox = Letterbox {
            scale: 0.5,
            pad_x: 0.0,
            pad_y: 25.0,
        };
        assert_eq!(letterbox.to_picture(50.0, 75.0), (100.0, 100.0));
    }

    #[test]
    fn normalised_input_subtracts_the_mean_and_divides_by_the_deviation() {
        let input = normalised(
            &solid(4, 4, [255, 0, 51]),
            2,
            2,
            [0.5, 0.5, 0.0],
            [0.5, 0.25, 0.2],
        );
        assert_eq!(input.len(), 3 * 4);
        assert!((input[0] - 1.0).abs() < 1e-5, "{}", input[0]);
        assert!((input[4] + 2.0).abs() < 1e-5, "{}", input[4]);
        assert!((input[8] - 1.0).abs() < 1e-5, "{}", input[8]);
    }

    #[test]
    fn bilinear_halving_averages_neighbours_without_a_wider_kernel() {
        // 4 x 1: 0, 100, 200, 250.  Halved, each output centre sits between two inputs.
        let mut image = RgbImage::new(4, 1);
        for (x, v) in [0u8, 100, 200, 250].into_iter().enumerate() {
            image.put_pixel(x as u32, 0, image::Rgb([v, v, v]));
        }
        let half = resize_bilinear(&image, 2, 1);
        assert_eq!(half.get_pixel(0, 0).0[0], 50);
        assert_eq!(half.get_pixel(1, 0).0[0], 225);
    }

    #[test]
    fn bilinear_upscaling_holds_the_edges() {
        let mut image = RgbImage::new(2, 1);
        image.put_pixel(0, 0, image::Rgb([0, 0, 0]));
        image.put_pixel(1, 0, image::Rgb([200, 200, 200]));
        let wide = resize_bilinear(&image, 4, 1);
        let row: Vec<u8> = (0..4).map(|x| wide.get_pixel(x, 0).0[0]).collect();
        // Output centres at source 0 (clamped), 0.25, 0.75 and 1.25 (clamped to 1).
        assert_eq!(row, vec![0, 50, 150, 200]);
    }

    #[test]
    fn bilinear_at_the_same_size_is_the_picture() {
        let image = RgbImage::from_pixel(3, 2, image::Rgb([7, 8, 9]));
        assert_eq!(resize_bilinear(&image, 3, 2), image);
    }

    #[test]
    fn channels_last_interleaves_the_planes() {
        // 2 x 1 pixels: R plane [1, 2], G plane [3, 4], B plane [5, 6].
        let planar = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(
            channels_last(&planar, 2, 1),
            vec![1.0, 3.0, 5.0, 2.0, 4.0, 6.0]
        );
    }

    #[test]
    fn a_crop_takes_the_box_and_its_margin() {
        let mut image = solid(100, 100, [0, 0, 0]);
        image.put_pixel(40, 40, image::Rgb([9, 9, 9]));
        let area = BoundingBox {
            x: 40.0,
            y: 40.0,
            width: 20.0,
            height: 10.0,
        };
        let cropped = crop(&image, &area, 0.5);
        assert_eq!(cropped.dimensions(), (40, 20));
        assert_eq!(cropped.get_pixel(10, 5).0, [9, 9, 9]);
    }

    #[test]
    fn a_crop_stays_inside_the_picture() {
        let image = solid(50, 30, [0, 0, 0]);
        let area = BoundingBox {
            x: -5.0,
            y: 20.0,
            width: 80.0,
            height: 30.0,
        };
        assert_eq!(crop(&image, &area, 0.1).dimensions(), (50, 13));
    }

    #[test]
    fn a_degenerate_box_still_crops_a_pixel() {
        let image = solid(10, 10, [0, 0, 0]);
        let area = BoundingBox {
            x: 12.0,
            y: 12.0,
            width: 0.0,
            height: 0.0,
        };
        assert_eq!(crop(&image, &area, 0.0).dimensions(), (1, 1));
    }
}
