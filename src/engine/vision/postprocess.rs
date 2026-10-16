//! A network's output into answers: boxes from a detector, ranked classes from a classifier.

use anyhow::{bail, Result};

/// Most boxes one detection keeps, strongest first.  Safe range 1..=10 000.
pub const MAX_DETECTIONS: usize = 300;

/// One output tensor: its shape and its values, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct RawTensor {
    pub shape: Vec<i64>,
    pub data: Vec<f32>,
}

/// How a YOLO detector lays out its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YoloLayout {
    /// YOLOv5: `[1, boxes, 5 + classes]`, rows of `cx, cy, w, h, objectness, class scores`.
    WithObjectness,
    /// YOLOv8 and later: `[1, 4 + classes, boxes]`, columns of `cx, cy, w, h, class scores`.
    Transposed,
    /// `[1, boxes, 4 + classes]`: YOLOv8 rows exported untransposed.
    Rows,
}

/// Which layout `shape` is for a detector of `classes` classes.
pub fn yolo_layout(shape: &[i64], classes: usize) -> Result<(YoloLayout, usize)> {
    let c = classes as i64;
    match *shape {
        [1, n, k] if k == 5 + c => Ok((YoloLayout::WithObjectness, n as usize)),
        [1, k, n] if k == 4 + c => Ok((YoloLayout::Transposed, n as usize)),
        [1, n, k] if k == 4 + c => Ok((YoloLayout::Rows, n as usize)),
        _ => bail!(
            "detector output shape {shape:?} is no YOLO layout for {classes} classes \
             (expected [1, N, {}] or [1, {}, N])",
            5 + classes,
            4 + classes
        ),
    }
}

/// A box in the network's input coordinates, before suppression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub score: f32,
    pub class: usize,
}

impl Candidate {
    fn area(&self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    /// Intersection over union with `other`.
    pub fn iou(&self, other: &Candidate) -> f32 {
        let w = (self.x1.min(other.x1) - self.x0.max(other.x0)).max(0.0);
        let h = (self.y1.min(other.y1) - self.y0.max(other.y0)).max(0.0);
        let inter = w * h;
        let union = self.area() + other.area() - inter;
        if union <= 0.0 {
            0.0
        } else {
            inter / union
        }
    }
}

/// Every box at or above `min_confidence`, with its best class.
pub fn yolo_candidates(
    raw: &RawTensor,
    classes: usize,
    min_confidence: f32,
) -> Result<Vec<Candidate>> {
    let (layout, boxes) = yolo_layout(&raw.shape, classes)?;
    let stride = match layout {
        YoloLayout::WithObjectness => 5 + classes,
        YoloLayout::Transposed | YoloLayout::Rows => 4 + classes,
    };
    if raw.data.len() < boxes * stride {
        bail!(
            "detector output holds {} values; its shape {:?} needs {}",
            raw.data.len(),
            raw.shape,
            boxes * stride
        );
    }
    let at = |b: usize, field: usize| match layout {
        YoloLayout::Transposed => raw.data[field * boxes + b],
        YoloLayout::WithObjectness | YoloLayout::Rows => raw.data[b * stride + field],
    };
    let first_class = stride - classes;
    let mut out = Vec::new();
    for b in 0..boxes {
        let objectness = match layout {
            YoloLayout::WithObjectness => at(b, 4),
            YoloLayout::Transposed | YoloLayout::Rows => 1.0,
        };
        if objectness < min_confidence {
            continue;
        }
        let (class, class_score) =
            (0..classes)
                .map(|c| (c, at(b, first_class + c)))
                .fold(
                    (0, f32::MIN),
                    |best, next| {
                        if next.1 > best.1 {
                            next
                        } else {
                            best
                        }
                    },
                );
        let score = objectness * class_score;
        if score < min_confidence {
            continue;
        }
        let (cx, cy, w, h) = (at(b, 0), at(b, 1), at(b, 2), at(b, 3));
        out.push(Candidate {
            x0: cx - w / 2.0,
            y0: cy - h / 2.0,
            x1: cx + w / 2.0,
            y1: cy + h / 2.0,
            score,
            class,
        });
    }
    Ok(out)
}

/// Non-maximum suppression per class: the strongest boxes, without a weaker box of the same
/// class overlapping one by more than `iou`; at most [`MAX_DETECTIONS`], strongest first.
pub fn suppress(mut candidates: Vec<Candidate>, iou: f32) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Candidate> = Vec::new();
    for c in candidates {
        if kept.len() == MAX_DETECTIONS {
            break;
        }
        if kept.iter().all(|k| k.class != c.class || k.iou(&c) <= iou) {
            kept.push(c);
        }
    }
    kept
}

/// Probabilities from logits (numerically stable).
pub fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

/// The `k` highest of `scores` at or above `min_confidence` as `(index, score)`, highest first.
pub fn top_k(scores: &[f32], k: usize, min_confidence: f32) -> Vec<(usize, f32)> {
    let mut ranked: Vec<(usize, f32)> = scores
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, s)| *s >= min_confidence)
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(k);
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(shape: &[i64], data: Vec<f32>) -> RawTensor {
        RawTensor {
            shape: shape.to_vec(),
            data,
        }
    }

    #[test]
    fn layouts_are_told_apart_by_the_class_count() {
        assert_eq!(
            yolo_layout(&[1, 102_000, 8], 3).unwrap(),
            (YoloLayout::WithObjectness, 102_000)
        );
        assert_eq!(
            yolo_layout(&[1, 84, 8400], 80).unwrap(),
            (YoloLayout::Transposed, 8400)
        );
        assert_eq!(
            yolo_layout(&[1, 8400, 84], 80).unwrap(),
            (YoloLayout::Rows, 8400)
        );
    }

    #[test]
    fn a_shape_that_fits_no_layout_is_refused_with_both_expectations() {
        let err = yolo_layout(&[1, 100, 9], 3).unwrap_err().to_string();
        assert!(err.contains("[1, N, 8] or [1, 7, N]"), "{err}");
        assert!(yolo_layout(&[100, 8], 3).is_err());
    }

    #[test]
    fn yolov5_rows_multiply_objectness_by_the_best_class() {
        // Two boxes, three classes: the first is a confident class 1, the second too weak.
        let out = raw(
            &[1, 2, 8],
            vec![
                50.0, 40.0, 20.0, 10.0, 0.9, 0.1, 0.8, 0.1, //
                10.0, 10.0, 4.0, 4.0, 0.1, 0.9, 0.0, 0.0,
            ],
        );
        let candidates = yolo_candidates(&out, 3, 0.2).unwrap();
        assert_eq!(candidates.len(), 1);
        let c = candidates[0];
        assert_eq!(c.class, 1);
        assert!((c.score - 0.72).abs() < 1e-6);
        assert_eq!((c.x0, c.y0, c.x1, c.y1), (40.0, 35.0, 60.0, 45.0));
    }

    #[test]
    fn a_box_with_high_objectness_but_weak_classes_is_dropped() {
        let out = raw(&[1, 1, 7], vec![5.0, 5.0, 2.0, 2.0, 0.9, 0.1, 0.1]);
        assert!(yolo_candidates(&out, 2, 0.2).unwrap().is_empty());
    }

    #[test]
    fn transposed_columns_are_read_per_box() {
        // Two boxes, two classes: columns are boxes.
        let out = raw(
            &[1, 6, 2],
            vec![
                10.0, 30.0, // cx
                10.0, 30.0, // cy
                4.0, 2.0, // w
                4.0, 2.0, // h
                0.1, 0.7, // class 0
                0.95, 0.2, // class 1
            ],
        );
        let candidates = yolo_candidates(&out, 2, 0.5).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!((candidates[0].class, candidates[0].score), (1, 0.95));
        assert_eq!((candidates[1].class, candidates[1].score), (0, 0.7));
        assert_eq!(candidates[1].x0, 29.0);
    }

    #[test]
    fn untransposed_rows_have_no_objectness() {
        let out = raw(&[1, 1, 6], vec![10.0, 10.0, 2.0, 2.0, 0.3, 0.6]);
        let candidates = yolo_candidates(&out, 2, 0.5).unwrap();
        assert_eq!((candidates[0].class, candidates[0].score), (1, 0.6));
    }

    #[test]
    fn an_output_shorter_than_its_shape_is_refused() {
        let err = yolo_candidates(&raw(&[1, 2, 8], vec![0.0; 8]), 3, 0.2).unwrap_err();
        assert!(err.to_string().contains("needs 16"), "{err}");
    }

    fn cand(x0: f32, score: f32, class: usize) -> Candidate {
        Candidate {
            x0,
            y0: 0.0,
            x1: x0 + 10.0,
            y1: 10.0,
            score,
            class,
        }
    }

    #[test]
    fn iou_of_identical_boxes_is_one_and_of_apart_boxes_zero() {
        assert!((cand(0.0, 1.0, 0).iou(&cand(0.0, 1.0, 0)) - 1.0).abs() < 1e-6);
        assert_eq!(cand(0.0, 1.0, 0).iou(&cand(20.0, 1.0, 0)), 0.0);
        let half = cand(0.0, 1.0, 0).iou(&cand(5.0, 1.0, 0));
        assert!((half - 1.0 / 3.0).abs() < 1e-6, "{half}");
    }

    #[test]
    fn empty_boxes_overlap_nothing() {
        let empty = Candidate {
            x0: 1.0,
            y0: 1.0,
            x1: 1.0,
            y1: 1.0,
            score: 1.0,
            class: 0,
        };
        assert_eq!(empty.iou(&empty), 0.0);
    }

    #[test]
    fn suppression_keeps_the_strongest_of_overlapping_boxes_per_class() {
        let kept = suppress(
            vec![
                cand(1.0, 0.6, 0),
                cand(0.0, 0.9, 0),
                cand(0.0, 0.5, 1),
                cand(50.0, 0.4, 0),
            ],
            0.45,
        );
        let summary: Vec<(f32, usize)> = kept.iter().map(|c| (c.score, c.class)).collect();
        assert_eq!(summary, vec![(0.9, 0), (0.5, 1), (0.4, 0)]);
    }

    #[test]
    fn suppression_keeps_at_most_the_cap() {
        let many: Vec<Candidate> = (0..MAX_DETECTIONS + 5)
            .map(|i| cand(i as f32 * 20.0, 0.5, 0))
            .collect();
        assert_eq!(suppress(many, 0.45).len(), MAX_DETECTIONS);
    }

    #[test]
    fn softmax_sums_to_one_and_keeps_the_order() {
        let p = softmax(&[1.0, 3.0, 2.0]);
        assert!((p.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!(p[1] > p[2] && p[2] > p[0]);
    }

    #[test]
    fn softmax_of_huge_logits_does_not_overflow() {
        let p = softmax(&[1.0, 1000.0, 999.0]);
        assert!(p.iter().all(|v| v.is_finite()));
        assert!((p.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!(p[1] > p[2]);
    }

    #[test]
    fn top_k_ranks_above_the_threshold() {
        assert_eq!(
            top_k(&[0.1, 0.5, 0.05, 0.3], 2, 0.0),
            vec![(1, 0.5), (3, 0.3)]
        );
        assert_eq!(
            top_k(&[0.1, 0.5, 0.05, 0.3], 5, 0.2),
            vec![(1, 0.5), (3, 0.3)]
        );
        assert!(top_k(&[0.1], 0, 0.0).is_empty());
    }
}
