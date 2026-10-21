//! `POST /vision/detect`, `/vision/classify` and `/vision/species`: a picture in, JSON out.
//!
//! The picture and its options arrive in any of the forms [`super::picture`] reads.

use std::collections::BTreeMap;

use tiny_http::Request;

use super::picture::{self, number, PictureRequest, Refusal};
use super::{respond_json, LocalApi, MAX_IMAGE_BODY_BYTES};
use crate::engine::vision::{self, ClassifyOptions, DetectOptions, MAX_TOP_K};
use crate::local_vision::{self, VisionContext, VisionError};

/// Which vision route a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    Detect,
    Classify,
    Species,
}

impl Route {
    pub(super) fn of(path: &str) -> Option<Self> {
        match path {
            "/vision/detect" => Some(Self::Detect),
            "/vision/classify" => Some(Self::Classify),
            "/vision/species" => Some(Self::Species),
            _ => None,
        }
    }

    /// The options a route reads; any other is refused by name.
    fn options(self) -> &'static [&'static str] {
        match self {
            Route::Detect => &["model", "minConfidence", "iou"],
            Route::Classify => &["model", "minConfidence", "topK"],
            Route::Species => &["detector", "classifier", "minConfidence", "iou", "topK"],
        }
    }
}

/// The picture and options of a request body.
pub(super) fn parse_request(
    route: Route,
    content_type: Option<&str>,
    query: Option<&str>,
    body: Vec<u8>,
) -> Result<PictureRequest, Refusal> {
    picture::parse(route.options(), content_type, query, body)
}

fn fraction(options: &BTreeMap<String, String>, key: &str) -> Result<Option<f32>, Refusal> {
    match number::<f32>(options, key)? {
        Some(v) if !(0.0..=1.0).contains(&v) => Err(Refusal::bad(format!(
            "{key} must be between 0 and 1, not {v}"
        ))),
        v => Ok(v),
    }
}

pub(super) fn detect_options(options: &BTreeMap<String, String>) -> Result<DetectOptions, Refusal> {
    Ok(DetectOptions {
        min_confidence: fraction(options, "minConfidence")?,
        iou: fraction(options, "iou")?,
    })
}

pub(super) fn classify_options(
    options: &BTreeMap<String, String>,
    with_threshold: bool,
) -> Result<ClassifyOptions, Refusal> {
    let top_k = match number::<usize>(options, "topK")? {
        Some(k) if !(1..=MAX_TOP_K).contains(&k) => {
            return Err(Refusal::bad(format!(
                "topK must be between 1 and {MAX_TOP_K}, not {k}"
            )))
        }
        k => k,
    };
    Ok(ClassifyOptions {
        min_confidence: if with_threshold {
            fraction(options, "minConfidence")?
        } else {
            None
        },
        top_k,
    })
}

/// The status and code a [`VisionError`] answers with.
pub(super) fn status_of(err: &VisionError) -> (u16, &'static str) {
    match err {
        VisionError::UnknownModel(_) => (404, "unknown_model"),
        VisionError::NoModel(_) => (404, "no_model"),
        VisionError::WrongModel { .. } => (400, "wrong_model"),
        VisionError::Disabled(_) => (400, "model_disabled"),
        VisionError::Busy => (503, "busy"),
        VisionError::InsufficientMemory(_) => (409, "insufficient_memory"),
        VisionError::NotServed(_) => (501, "not_served"),
        VisionError::Failed(_) => (500, "inference_failed"),
    }
}

fn refuse(request: Request, refusal: Refusal) -> std::io::Result<()> {
    picture::refuse(request, "vision", refusal)
}

impl LocalApi {
    pub(super) fn handle_vision(&self, mut request: Request, route: Route) -> std::io::Result<()> {
        let body = match super::read_bytes_capped(&mut request, MAX_IMAGE_BODY_BYTES)? {
            Some(body) => body,
            None => return super::respond_too_large_capped(request, MAX_IMAGE_BODY_BYTES),
        };
        let header = |name: &'static str| {
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv(name))
                .map(|h| h.value.as_str().to_string())
        };
        let content_type = header("content-type");
        let query = request.url().split_once('?').map(|(_, q)| q.to_string());
        let parsed = match parse_request(route, content_type.as_deref(), query.as_deref(), body) {
            Ok(parsed) => parsed,
            Err(refusal) => return refuse(request, refusal),
        };
        let picture = match vision::decode_picture(&parsed.picture) {
            Ok(picture) => picture,
            Err(err) => return refuse(request, Refusal::bad(format!("{err:#}"))),
        };
        let o = &parsed.options;
        let options = match route {
            Route::Detect => detect_options(o).map(|d| (d, ClassifyOptions::default())),
            Route::Classify => classify_options(o, true).map(|c| (DetectOptions::default(), c)),
            Route::Species => detect_options(o).and_then(|d| Ok((d, classify_options(o, false)?))),
        };
        let (detect_options, classify_options) = match options {
            Ok(options) => options,
            Err(refusal) => return refuse(request, refusal),
        };
        let catalog = self.catalog.lock().clone();
        let ctx = VisionContext {
            host: &self.services.host,
            catalog: &catalog,
            observers: &self.observers,
            gate: &self.gate,
        };
        let model = o.get("model").map(String::as_str);
        let answer = match route {
            Route::Detect => local_vision::detect(&ctx, model, &picture, &detect_options)
                .map(serde_json::to_value),
            Route::Classify => local_vision::classify(&ctx, model, &picture, &classify_options)
                .map(serde_json::to_value),
            Route::Species => local_vision::species(
                &ctx,
                o.get("detector").map(String::as_str),
                o.get("classifier").map(String::as_str),
                &picture,
                &detect_options,
                &classify_options,
            )
            .map(serde_json::to_value),
        };
        match answer {
            Ok(Ok(json)) => respond_json(request, 200, &json),
            Ok(Err(err)) => refuse(
                request,
                Refusal {
                    status: 500,
                    code: "serialisation_failed",
                    message: err.to_string(),
                },
            ),
            Err(err) => {
                let (status, code) = status_of(&err);
                refuse(
                    request,
                    Refusal {
                        status,
                        code,
                        message: err.to_string(),
                    },
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn routes_are_found_by_path() {
        assert_eq!(Route::of("/vision/detect"), Some(Route::Detect));
        assert_eq!(Route::of("/vision/classify"), Some(Route::Classify));
        assert_eq!(Route::of("/vision/species"), Some(Route::Species));
        assert_eq!(Route::of("/vision/other"), None);
    }

    #[test]
    fn a_raw_body_is_the_picture_and_the_query_its_options() {
        let parsed = parse_request(
            Route::Detect,
            Some("image/jpeg"),
            Some("model=md%2Dv5&minConfidence=0.3"),
            b"jpeg".to_vec(),
        )
        .unwrap();
        assert_eq!(parsed.picture, b"jpeg");
        assert_eq!(
            parsed.options,
            options(&[("model", "md-v5"), ("minConfidence", "0.3")])
        );
        for content_type in [None, Some("application/octet-stream")] {
            assert!(parse_request(Route::Detect, content_type, None, b"x".to_vec()).is_ok());
        }
    }

    #[test]
    fn a_json_body_carries_the_picture_inline_and_overrides_the_query() {
        let body = serde_json::json!({
            "image": "data:image/png;base64,iVBORw0KGgo=",
            "topK": 3,
            "model": "birds",
            "minConfidence": null,
        });
        let parsed = parse_request(
            Route::Classify,
            Some("application/json; charset=utf-8"),
            Some("topK=9"),
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.picture, b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            parsed.options,
            options(&[("model", "birds"), ("topK", "3")])
        );
    }

    #[test]
    fn bare_base64_is_a_picture_too() {
        let body = br#"{"image": "aGVsbG8="}"#.to_vec();
        let parsed = parse_request(Route::Detect, Some("application/json"), None, body).unwrap();
        assert_eq!(parsed.picture, b"hello");
    }

    #[test]
    fn broken_json_bodies_are_refused() {
        for (body, says) in [
            (&b"{"[..], "bad json"),
            (b"[1]", "must be an object"),
            (br#"{"model": "x"}"#, "no `image`"),
            (br#"{"image": "!!"}"#, "neither a data: URL nor base64"),
            (br#"{"image": "data:text/plain;base64,aGk="}"#, "image:"),
            (
                br#"{"image": "aGk=", "topK": [1]}"#,
                "must be a string or a number",
            ),
        ] {
            let refusal = parse_request(
                Route::Classify,
                Some("application/json"),
                None,
                body.to_vec(),
            )
            .unwrap_err();
            assert_eq!(refusal.status, 400);
            assert!(
                refusal.message.contains(says),
                "{}: {}",
                says,
                refusal.message
            );
        }
    }

    #[test]
    fn a_form_carries_the_picture_as_its_image_file() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"x.jpg\"\r\n\r\nJPEG\r\n\
--b\r\nContent-Disposition: form-data; name=\"detector\"\r\n\r\nmd\r\n--b--\r\n"
            .to_vec();
        let parsed = parse_request(
            Route::Species,
            Some("multipart/form-data; boundary=b"),
            None,
            body,
        )
        .unwrap();
        assert_eq!(parsed.picture, b"JPEG");
        assert_eq!(parsed.options, options(&[("detector", "md")]));
    }

    #[test]
    fn broken_forms_are_refused() {
        let no_image =
            b"--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1\r\n--b--".to_vec();
        let not_utf8 =
            b"--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n\xff\r\n--b--".to_vec();
        for (content_type, body, says) in [
            ("multipart/form-data", b"".to_vec(), "without a boundary"),
            (
                "multipart/form-data; boundary=b",
                b"junk".to_vec(),
                "multipart body",
            ),
            (
                "multipart/form-data; boundary=b",
                no_image,
                "no `image` file",
            ),
            ("multipart/form-data; boundary=b", not_utf8, "not UTF-8"),
        ] {
            let refusal = parse_request(Route::Detect, Some(content_type), None, body).unwrap_err();
            assert!(
                refusal.message.contains(says),
                "{says}: {}",
                refusal.message
            );
        }
    }

    #[test]
    fn requests_without_a_usable_picture_are_refused() {
        let refusal =
            parse_request(Route::Detect, Some("text/plain"), None, b"x".to_vec()).unwrap_err();
        assert_eq!(
            (refusal.status, refusal.code),
            (415, "unsupported_media_type")
        );
        let refusal = parse_request(Route::Detect, None, None, Vec::new()).unwrap_err();
        assert!(
            refusal.message.contains("no picture"),
            "{}",
            refusal.message
        );
        let huge = vec![0; crate::vision::MAX_IMAGE_BYTES as usize + 1];
        let refusal = parse_request(Route::Detect, Some("image/png"), None, huge).unwrap_err();
        assert_eq!((refusal.status, refusal.code), (413, "picture_too_large"));
    }

    #[test]
    fn an_option_the_route_does_not_read_is_refused_by_name() {
        let refusal =
            parse_request(Route::Detect, None, Some("topK=3"), b"x".to_vec()).unwrap_err();
        assert!(
            refusal.message.contains("unknown option \"topK\""),
            "{}",
            refusal.message
        );
        assert!(
            refusal.message.contains("model, minConfidence, iou"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn options_are_validated() {
        assert_eq!(
            detect_options(&options(&[("minConfidence", "0.5"), ("iou", " 0.3 ")])).unwrap(),
            DetectOptions {
                min_confidence: Some(0.5),
                iou: Some(0.3)
            }
        );
        assert!(detect_options(&options(&[("iou", "1.5")])).is_err());
        assert!(detect_options(&options(&[("minConfidence", "high")])).is_err());
        assert_eq!(
            classify_options(&options(&[("topK", "3"), ("minConfidence", "0.1")]), true).unwrap(),
            ClassifyOptions {
                min_confidence: Some(0.1),
                top_k: Some(3)
            }
        );
        assert_eq!(
            classify_options(&options(&[("minConfidence", "0.1")]), false)
                .unwrap()
                .min_confidence,
            None,
            "species applies the threshold to the detector only"
        );
        assert!(classify_options(&options(&[("topK", "0")]), true).is_err());
        assert!(classify_options(&options(&[("topK", "101")]), true).is_err());
    }

    #[test]
    fn every_error_has_its_status() {
        let refused = crate::admission::Refused {
            memory: crate::admission::MemoryKind::Device,
            needed_gib: 2.0,
            free_gib: 1.0,
            margin_gib: 0.5,
        };
        let cases = [
            (VisionError::UnknownModel("m".into()), 404),
            (VisionError::NoModel(crate::types::VisionTask::Detect), 404),
            (
                VisionError::WrongModel {
                    id: "m".into(),
                    want: crate::types::VisionTask::Detect,
                    got: "x".into(),
                },
                400,
            ),
            (VisionError::Disabled("m".into()), 400),
            (VisionError::Busy, 503),
            (VisionError::InsufficientMemory(refused), 409),
            (VisionError::NotServed("m".into()), 501),
            (VisionError::Failed("x".into()), 500),
        ];
        for (err, status) in cases {
            assert_eq!(status_of(&err).0, status, "{err}");
        }
    }
}
