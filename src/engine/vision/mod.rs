//! Vision inference: detection (is there an animal, and where) and classification (which
//! species).  See `docs/engines/vision.md`.
//!
//! A vision model is a catalogue model of kind `vision` on the `onnx-vision` engine; its
//! `cliDefaults.vision` says whether it detects or classifies, and how.  The pre- and
//! post-processing here are pure and run over any [`VisionModel`]; only [`onnx`] (feature
//! `vision`) touches ONNX Runtime.

pub mod labels;
#[cfg(feature = "vision")]
pub mod onnx;
pub mod postprocess;
pub mod preprocess;

use std::io::Cursor;

use anyhow::{bail, Context, Result};
use image::{ImageDecoder, ImageReader, RgbImage};
use serde::Serialize;

use crate::catalog::CatalogModel;
use crate::types::{VisionDefaults, VisionTask};
use labels::{Label, Taxonomy};
use postprocess::RawTensor;

/// A detector's confidence floor when neither the model nor the request sets one.
pub const DEFAULT_MIN_CONFIDENCE: f32 = 0.25;
/// A detector's overlap limit when neither the model nor the request sets one.
pub const DEFAULT_IOU: f32 = 0.45;
/// Classes a classifier answers when neither the model nor the request says.
pub const DEFAULT_TOP_K: usize = 5;
/// Most classes one classification answers.  Safe range 1..=10 000.
pub const MAX_TOP_K: usize = 100;
/// How much of its surroundings an animal's crop keeps when the classifier does not say.
pub const DEFAULT_CROP_MARGIN: f32 = 0.1;

/// Everything about a loaded vision model the pipeline needs.
#[derive(Debug, Clone, PartialEq)]
pub struct VisionSpec {
    pub model: String,
    pub task: VisionTask,
    pub width: u32,
    pub height: u32,
    pub labels: Vec<Label>,
    /// Per class: whether it is an animal.
    pub animal: Vec<bool>,
    pub mean: [f32; 3],
    pub std: [f32; 3],
    pub probabilities: bool,
    pub min_confidence: f32,
    pub iou: f32,
    pub top_k: usize,
    /// The input is `1 x height x width x 3` rather than `1 x 3 x height x width`.
    pub channels_last: bool,
    /// Fraction of a box per side an animal's crop adds before this classifier ranks it.
    pub crop_margin: f32,
}

impl VisionSpec {
    /// The input tensor's shape.
    pub fn input_shape(&self) -> [usize; 4] {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.channels_last {
            [1, h, w, 3]
        } else {
            [1, 3, h, w]
        }
    }

    /// A planar input in the order the network takes.
    fn arrange(&self, planar: Vec<f32>) -> Vec<f32> {
        if self.channels_last {
            preprocess::channels_last(&planar, self.width, self.height)
        } else {
            planar
        }
    }
}

/// The vision knobs of `model`, refused when it is no vision model.
pub fn defaults_of(model: &CatalogModel) -> Result<&VisionDefaults> {
    model.source.cli_defaults.vision.as_ref().with_context(|| {
        format!(
            "model {} has no cliDefaults.vision: it is no vision model",
            model.id
        )
    })
}

impl VisionSpec {
    /// The spec of `model`, its class names from `labels_file` when its defaults list none.
    pub fn from_model(model: &CatalogModel, labels_file: Option<&str>) -> Result<Self> {
        let defaults = defaults_of(model)?;
        let (width, height) = (
            model.source.cli_defaults.width,
            model.source.cli_defaults.height,
        );
        if width == 0 || height == 0 {
            bail!(
                "model {}: cliDefaults.width and height (its input size) must be set",
                model.id
            );
        }
        let labels = if !defaults.labels.is_empty() {
            defaults.labels.iter().map(Label::plain).collect()
        } else if let Some(text) = labels_file {
            labels::parse(text).with_context(|| format!("reading the labels of {}", model.id))?
        } else {
            bail!(
                "model {}: no labels (set cliDefaults.vision.labels or add a `labels` file)",
                model.id
            );
        };
        let animal = match &defaults.animal_labels {
            None => vec![true; labels.len()],
            Some(animals) => {
                if let Some(unknown) = animals
                    .iter()
                    .find(|a| !labels.iter().any(|l| &l.name == *a))
                {
                    bail!(
                        "model {}: animal label {unknown:?} is none of its labels",
                        model.id
                    );
                }
                labels.iter().map(|l| animals.contains(&l.name)).collect()
            }
        };
        let std = defaults.std.unwrap_or([1.0; 3]);
        if std.iter().any(|s| *s <= 0.0) {
            bail!("model {}: every std must be above 0", model.id);
        }
        let crop_margin = defaults.crop_margin.unwrap_or(DEFAULT_CROP_MARGIN);
        if !(0.0..=1.0).contains(&crop_margin) {
            bail!("model {}: cropMargin must be between 0 and 1", model.id);
        }
        Ok(Self {
            model: model.id.clone(),
            task: defaults.task,
            width,
            height,
            animal,
            labels,
            mean: defaults.mean.unwrap_or([0.0; 3]),
            std,
            probabilities: defaults.probabilities.unwrap_or(false),
            min_confidence: defaults.min_confidence.unwrap_or(match defaults.task {
                VisionTask::Detect => DEFAULT_MIN_CONFIDENCE,
                VisionTask::Classify => 0.0,
            }),
            iou: defaults.iou.unwrap_or(DEFAULT_IOU),
            top_k: defaults.top_k.unwrap_or(DEFAULT_TOP_K).clamp(1, MAX_TOP_K),
            channels_last: defaults.channels_last.unwrap_or(false),
            crop_margin,
        })
    }
}

/// Whether a declared tensor shape fits `want`; a dynamic dimension (below 0) fits any size.
pub fn shape_fits(have: &[i64], want: &[usize]) -> bool {
    have.len() == want.len()
        && have
            .iter()
            .zip(want)
            .all(|(have, want)| *have < 0 || *have as usize == *want)
}

/// Whether a classifier's declared output holds one score per label: its last dimension is the
/// label count (or dynamic).
pub fn scores_fit(output: &[i64], labels: usize) -> bool {
    output
        .last()
        .is_some_and(|last| *last < 0 || *last as usize == labels)
}

/// A vision network in memory.
pub trait VisionModel: Send + Sync {
    fn spec(&self) -> &VisionSpec;

    /// Where it runs: `cuda` or `cpu`.
    fn device(&self) -> &'static str;

    /// Run the network on one input of [`VisionSpec::input_shape`]; its first output.
    fn run(&self, input: Vec<f32>) -> Result<RawTensor>;
}

/// A rectangle in pixels (or, normalised, in fractions of the picture).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One thing a detector found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub label: String,
    pub class_id: usize,
    pub confidence: f32,
    /// Whether its class is an animal.
    pub animal: bool,
    /// Where, in the picture's pixels.
    #[serde(rename = "box")]
    pub area: BoundingBox,
    /// Where, as fractions of the picture's width and height.
    pub box_normalised: BoundingBox,
}

/// One class a classifier answered.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prediction {
    pub label: String,
    pub class_id: usize,
    pub confidence: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taxonomy: Option<Taxonomy>,
}

/// What a request may change about a detection.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DetectOptions {
    pub min_confidence: Option<f32>,
    pub iou: Option<f32>,
}

/// What a request may change about a classification.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClassifyOptions {
    pub min_confidence: Option<f32>,
    pub top_k: Option<usize>,
}

fn require(spec: &VisionSpec, task: VisionTask) -> Result<()> {
    if spec.task != task {
        bail!(
            "model {} is a {} model, not {}",
            spec.model,
            spec.task.as_str(),
            task.as_str()
        );
    }
    Ok(())
}

/// What `model` finds in `picture`, strongest first.
pub fn detect(
    model: &dyn VisionModel,
    picture: &RgbImage,
    options: &DetectOptions,
) -> Result<Vec<Detection>> {
    let spec = model.spec();
    require(spec, VisionTask::Detect)?;
    let (input, letterbox) = preprocess::letterbox(picture, spec.width, spec.height);
    let raw = model.run(spec.arrange(input))?;
    let candidates = postprocess::yolo_candidates(
        &raw,
        spec.labels.len(),
        options.min_confidence.unwrap_or(spec.min_confidence),
    )?;
    let kept = postprocess::suppress(candidates, options.iou.unwrap_or(spec.iou));
    let (w, h) = (picture.width() as f32, picture.height() as f32);
    Ok(kept
        .into_iter()
        .map(|c| {
            let (x0, y0) = letterbox.to_picture(c.x0, c.y0);
            let (x1, y1) = letterbox.to_picture(c.x1, c.y1);
            let (x0, y0) = (x0.clamp(0.0, w), y0.clamp(0.0, h));
            let (x1, y1) = (x1.clamp(0.0, w), y1.clamp(0.0, h));
            let area = BoundingBox {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            };
            Detection {
                label: spec.labels[c.class].name.clone(),
                class_id: c.class,
                confidence: c.score,
                animal: spec.animal[c.class],
                area,
                box_normalised: BoundingBox {
                    x: area.x / w,
                    y: area.y / h,
                    width: area.width / w,
                    height: area.height / h,
                },
            }
        })
        .collect())
}

/// The classes `model` sees in `picture`, most likely first.
pub fn classify(
    model: &dyn VisionModel,
    picture: &RgbImage,
    options: &ClassifyOptions,
) -> Result<Vec<Prediction>> {
    let spec = model.spec();
    require(spec, VisionTask::Classify)?;
    let input = preprocess::normalised(picture, spec.width, spec.height, spec.mean, spec.std);
    let raw = model.run(spec.arrange(input))?;
    if raw.data.len() != spec.labels.len() {
        bail!(
            "classifier output holds {} scores (shape {:?}); model {} has {} labels",
            raw.data.len(),
            raw.shape,
            spec.model,
            spec.labels.len()
        );
    }
    let scores = if spec.probabilities {
        raw.data
    } else {
        postprocess::softmax(&raw.data)
    };
    let k = options.top_k.unwrap_or(spec.top_k).clamp(1, MAX_TOP_K);
    let floor = options.min_confidence.unwrap_or(spec.min_confidence);
    Ok(postprocess::top_k(&scores, k, floor)
        .into_iter()
        .map(|(class, confidence)| Prediction {
            label: spec.labels[class].name.clone(),
            class_id: class,
            confidence,
            taxonomy: spec.labels[class].taxonomy.clone(),
        })
        .collect())
}

/// Decode a PNG, JPEG or WebP picture to RGB, turned upright as its EXIF orientation says
/// (camera pictures are often stored on their side).
pub fn decode_picture(bytes: &[u8]) -> Result<RgbImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("reading the picture")?
        .into_decoder()
        .context("the picture is no PNG, JPEG or WebP")?;
    let orientation = decoder
        .orientation()
        .context("reading the picture's orientation")?;
    let mut picture = image::DynamicImage::from_decoder(decoder).context("decoding the picture")?;
    picture.apply_orientation(orientation);
    Ok(picture.to_rgb8())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{vision_defaults as defaults, vision_model, CannedVision as Canned};

    fn detector_spec() -> VisionSpec {
        let mut d = defaults(VisionTask::Detect, &["animal", "person", "vehicle"]);
        d.animal_labels = Some(vec!["animal".into()]);
        d.min_confidence = Some(0.2);
        VisionSpec::from_model(&vision_model("md", 100, d), None).unwrap()
    }

    #[test]
    fn a_spec_takes_the_model_defaults() {
        let spec = detector_spec();
        assert_eq!(spec.task, VisionTask::Detect);
        assert_eq!((spec.width, spec.height), (100, 100));
        assert_eq!(spec.animal, vec![true, false, false]);
        assert_eq!(spec.min_confidence, 0.2);
        assert_eq!(spec.iou, DEFAULT_IOU);
        assert_eq!(spec.std, [1.0; 3]);
    }

    #[test]
    fn a_spec_reads_its_labels_file_when_the_defaults_list_none() {
        let model = vision_model("birds", 8, defaults(VisionTask::Classify, &[]));
        let spec = VisionSpec::from_model(&model, Some("robin\nwren")).unwrap();
        assert_eq!(spec.labels[1].name, "wren");
        assert_eq!(spec.animal, vec![true, true]);
        assert_eq!(spec.min_confidence, 0.0);
        assert_eq!(spec.top_k, DEFAULT_TOP_K);
    }

    #[test]
    fn a_spec_without_labels_is_refused() {
        let model = vision_model("birds", 8, defaults(VisionTask::Classify, &[]));
        let err = VisionSpec::from_model(&model, None).unwrap_err();
        assert!(err.to_string().contains("no labels"), "{err}");
    }

    #[test]
    fn a_broken_labels_file_names_the_model() {
        let model = vision_model("birds", 8, defaults(VisionTask::Classify, &[]));
        let err = VisionSpec::from_model(&model, Some("{}")).unwrap_err();
        assert!(format!("{err:#}").contains("labels of birds"), "{err:#}");
    }

    #[test]
    fn an_unknown_animal_label_is_refused() {
        let mut d = defaults(VisionTask::Detect, &["animal"]);
        d.animal_labels = Some(vec!["aminal".into()]);
        let err = VisionSpec::from_model(&vision_model("md", 8, d), None).unwrap_err();
        assert!(err.to_string().contains("\"aminal\""), "{err}");
    }

    #[test]
    fn a_spec_without_an_input_size_is_refused() {
        let model = vision_model("md", 0, defaults(VisionTask::Detect, &["a"]));
        let err = VisionSpec::from_model(&model, None).unwrap_err();
        assert!(err.to_string().contains("input size"), "{err}");
    }

    #[test]
    fn a_crop_margin_outside_a_fraction_is_refused() {
        let mut d = defaults(VisionTask::Classify, &["a"]);
        d.crop_margin = Some(1.5);
        let err = VisionSpec::from_model(&vision_model("c", 8, d), None).unwrap_err();
        assert!(err.to_string().contains("cropMargin"), "{err}");
    }

    #[test]
    fn a_channels_last_network_gets_interleaved_input() {
        let mut d = defaults(VisionTask::Classify, &["a", "b"]);
        d.channels_last = Some(true);
        d.crop_margin = Some(0.0);
        let spec = VisionSpec::from_model(&vision_model("sn", 2, d), None).unwrap();
        assert_eq!(spec.input_shape(), [1, 2, 2, 3]);
        assert_eq!(spec.crop_margin, 0.0);
        let model = Canned::new(spec, &[1, 2], vec![0.0, 1.0]);
        let red = RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));
        classify(&model, &red, &ClassifyOptions::default()).unwrap();
        let seen = model.seen.lock().clone().unwrap();
        assert_eq!(&seen[..6], &[1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn declared_shapes_fit_with_dynamic_dimensions() {
        assert!(shape_fits(&[-1, 480, 480, 3], &[1, 480, 480, 3]));
        assert!(shape_fits(&[1, 3, 1280, 1280], &[1, 3, 1280, 1280]));
        assert!(!shape_fits(&[1, 3, 224, 224], &[1, 3, 260, 260]));
        assert!(!shape_fits(&[1, 3, 260], &[1, 3, 260, 260]));
    }

    #[test]
    fn a_classifier_output_must_hold_a_score_per_label() {
        assert!(scores_fit(&[1, 525], 525));
        assert!(scores_fit(&[-1, -1], 2498));
        assert!(!scores_fit(&[1, 1000], 525));
        assert!(!scores_fit(&[], 3));
    }

    #[test]
    fn a_planar_network_keeps_its_input_shape() {
        assert_eq!(detector_spec().input_shape(), [1, 3, 100, 100]);
    }

    #[test]
    fn a_zero_deviation_is_refused() {
        let mut d = defaults(VisionTask::Classify, &["a"]);
        d.std = Some([1.0, 0.0, 1.0]);
        let err = VisionSpec::from_model(&vision_model("c", 8, d), None).unwrap_err();
        assert!(err.to_string().contains("std"), "{err}");
    }

    #[test]
    fn a_model_without_vision_defaults_is_no_vision_model() {
        let mut model = vision_model("x", 8, defaults(VisionTask::Detect, &["a"]));
        model.source.cli_defaults.vision = None;
        let err = VisionSpec::from_model(&model, None).unwrap_err();
        assert!(err.to_string().contains("no vision model"), "{err}");
    }

    #[test]
    fn a_detection_lands_on_the_picture_in_pixels_and_fractions() {
        // A 200x100 picture letterboxed into 100x100: scale 0.5, 25 px grey above.
        let picture = RgbImage::new(200, 100);
        let model = Canned::new(
            detector_spec(),
            &[1, 2, 8],
            vec![
                50.0, 50.0, 20.0, 10.0, 0.9, 1.0, 0.0, 0.0, // an animal in the middle
                10.0, 30.0, 4.0, 4.0, 0.5, 0.0, 0.8, 0.0, // a person
            ],
        );
        let found = detect(&model, &picture, &DetectOptions::default()).unwrap();
        assert_eq!(found.len(), 2);
        let animal = &found[0];
        assert_eq!(animal.label, "animal");
        assert!(animal.animal);
        assert!((animal.confidence - 0.9).abs() < 1e-6);
        assert_eq!(
            animal.area,
            BoundingBox {
                x: 80.0,
                y: 40.0,
                width: 40.0,
                height: 20.0
            }
        );
        assert_eq!(animal.box_normalised.x, 0.4);
        assert_eq!(animal.box_normalised.height, 0.2);
        assert_eq!(found[1].label, "person");
        assert!(!found[1].animal);
        assert_eq!(
            model.seen.lock().as_ref().map(Vec::len),
            Some(3 * 100 * 100)
        );
    }

    #[test]
    fn a_box_past_the_picture_is_clamped_to_it() {
        let picture = RgbImage::new(100, 100);
        let model = Canned::new(
            detector_spec(),
            &[1, 1, 8],
            vec![5.0, 5.0, 20.0, 20.0, 0.9, 1.0, 0.0, 0.0],
        );
        let found = detect(&model, &picture, &DetectOptions::default()).unwrap();
        assert_eq!(found[0].area.x, 0.0);
        assert_eq!(found[0].area.width, 15.0);
    }

    #[test]
    fn a_request_threshold_overrides_the_model() {
        let picture = RgbImage::new(100, 100);
        let model = Canned::new(
            detector_spec(),
            &[1, 1, 8],
            vec![50.0, 50.0, 10.0, 10.0, 0.5, 1.0, 0.0, 0.0],
        );
        let options = DetectOptions {
            min_confidence: Some(0.6),
            iou: None,
        };
        assert!(detect(&model, &picture, &options).unwrap().is_empty());
    }

    fn classifier_spec(probabilities: bool) -> VisionSpec {
        let mut d = defaults(VisionTask::Classify, &[]);
        d.probabilities = Some(probabilities);
        d.mean = Some([0.5; 3]);
        d.std = Some([0.5; 3]);
        d.top_k = Some(2);
        let model = vision_model("birds", 4, d);
        VisionSpec::from_model(
            &model,
            Some("u;aves;passeriformes;turdidae;turdus;merula;blackbird\nrobin\nwren"),
        )
        .unwrap()
    }

    #[test]
    fn a_classification_ranks_softmaxed_logits_with_taxonomy() {
        let model = Canned::new(classifier_spec(false), &[1, 3], vec![5.0, 1.0, 3.0]);
        let ranked = classify(&model, &RgbImage::new(9, 9), &ClassifyOptions::default()).unwrap();
        let names: Vec<&str> = ranked.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(names, ["blackbird", "wren"]);
        assert!(ranked[0].confidence > 0.8);
        assert_eq!(
            ranked[0]
                .taxonomy
                .as_ref()
                .and_then(|t| t.scientific_name.as_deref()),
            Some("Turdus merula")
        );
        assert!(ranked[1].taxonomy.is_none());
        // Black pixels normalised with mean 0.5, std 0.5.
        assert_eq!(model.seen.lock().as_ref().unwrap()[0], -1.0);
    }

    #[test]
    fn probabilities_are_ranked_as_they_are() {
        let model = Canned::new(classifier_spec(true), &[1, 3], vec![0.2, 0.7, 0.1]);
        let options = ClassifyOptions {
            min_confidence: Some(0.15),
            top_k: Some(10),
        };
        let ranked = classify(&model, &RgbImage::new(4, 4), &options).unwrap();
        let summary: Vec<(usize, f32)> =
            ranked.iter().map(|p| (p.class_id, p.confidence)).collect();
        assert_eq!(summary, vec![(1, 0.7), (0, 0.2)]);
    }

    #[test]
    fn a_classifier_output_of_the_wrong_size_is_refused() {
        let model = Canned::new(classifier_spec(false), &[1, 2], vec![1.0, 2.0]);
        let err = classify(&model, &RgbImage::new(4, 4), &ClassifyOptions::default()).unwrap_err();
        assert!(err.to_string().contains("has 3 labels"), "{err}");
    }

    #[test]
    fn a_model_answers_only_its_own_task() {
        let classifier = Canned::new(classifier_spec(false), &[1, 3], vec![0.0; 3]);
        let err = detect(&classifier, &RgbImage::new(4, 4), &DetectOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("is a classify model, not detect"),
            "{err}"
        );
        let detector = Canned::new(detector_spec(), &[1, 0, 8], vec![]);
        assert!(classify(&detector, &RgbImage::new(4, 4), &ClassifyOptions::default()).is_err());
    }

    fn encoded(picture: &RgbImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        picture.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn png_and_jpeg_pictures_decode() {
        let picture = RgbImage::from_pixel(3, 2, image::Rgb([200, 10, 10]));
        for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg] {
            let decoded = decode_picture(&encoded(&picture, format)).unwrap();
            assert_eq!(decoded.dimensions(), (3, 2), "{format:?}");
        }
    }

    #[test]
    fn a_picture_on_its_side_is_turned_upright() {
        // A 4x2 JPEG with EXIF orientation 6 (rotate 90 degrees clockwise) shows as 2x4.
        let jpeg = encoded(&RgbImage::new(4, 2), image::ImageFormat::Jpeg);
        let tagged = with_exif_orientation(&jpeg, 6);
        assert_eq!(decode_picture(&tagged).unwrap().dimensions(), (2, 4));
    }

    /// `jpeg` with an APP1 EXIF segment holding only an orientation tag.
    fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"MM\x00\x2a\x00\x00\x00\x08"); // big endian, IFD at 8
        tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
        tiff.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
        tiff.extend_from_slice(&3u16.to_be_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_be_bytes()); // count
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
        let mut segment = b"Exif\x00\x00".to_vec();
        segment.extend_from_slice(&tiff);
        let mut out = jpeg[..2].to_vec(); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((segment.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&segment);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn bytes_that_are_no_picture_are_refused() {
        assert!(decode_picture(b"definitely not a picture").is_err());
    }
}
