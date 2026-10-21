//! A picture in a local API request: the raw body (`Content-Type: image/*` or
//! `application/octet-stream`), the `image` file of a `multipart/form-data` form, or `image` in a
//! JSON body (a `data:image/...;base64,` URL or bare base64).  Options come from the query string
//! and, overriding it, the form fields or JSON keys.  Shared by the vision and mesh routes.

use std::collections::BTreeMap;

use base64::Engine as _;
use tiny_http::{Header, Request, Response};

use super::respond_json;

/// A request the route can run: its picture and its options.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PictureRequest {
    pub picture: Vec<u8>,
    pub options: BTreeMap<String, String>,
}

/// Why a request was refused before any model ran: an HTTP status, a code, a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub(super) fn bad(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message: message.into(),
        }
    }
}

/// The picture and options of a request body.
pub(super) fn parse(
    allowed: &[&str],
    content_type: Option<&str>,
    query: Option<&str>,
    body: Vec<u8>,
) -> Result<PictureRequest, Refusal> {
    let mut options: BTreeMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let content_type = content_type.unwrap_or("").trim();
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let picture = if media == "application/json" {
        from_json(&body, &mut options)?
    } else if media == "multipart/form-data" {
        from_form(content_type, &body, &mut options)?
    } else if media.is_empty() || media.starts_with("image/") || media == "application/octet-stream"
    {
        body
    } else {
        return Err(Refusal {
            status: 415,
            code: "unsupported_media_type",
            message: format!(
                "Content-Type {media} is not served; send image/*, application/octet-stream, \
                 multipart/form-data or application/json"
            ),
        });
    };
    if picture.is_empty() {
        return Err(Refusal::bad(
            "no picture: the request carries no image bytes",
        ));
    }
    if picture.len() as u64 > crate::vision::MAX_IMAGE_BYTES {
        return Err(Refusal {
            status: 413,
            code: "picture_too_large",
            message: format!(
                "the picture is {} bytes; at most {}",
                picture.len(),
                crate::vision::MAX_IMAGE_BYTES
            ),
        });
    }
    if let Some(unknown) = options.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Refusal::bad(format!(
            "unknown option {unknown:?}; this route reads {}",
            allowed.join(", ")
        )));
    }
    Ok(PictureRequest { picture, options })
}

fn from_json(body: &[u8], options: &mut BTreeMap<String, String>) -> Result<Vec<u8>, Refusal> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Refusal::bad(format!("bad json: {e}")))?;
    let Some(object) = json.as_object() else {
        return Err(Refusal::bad("the JSON body must be an object"));
    };
    let mut picture = None;
    for (key, value) in object {
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Null => continue,
            other => {
                return Err(Refusal::bad(format!(
                    "{key} must be a string or a number, not {other}"
                )))
            }
        };
        if key == "image" {
            picture = Some(decode_inline(&text)?);
        } else {
            options.insert(key.clone(), text);
        }
    }
    picture.ok_or_else(|| Refusal::bad("the JSON body has no `image`"))
}

/// An inline picture: a `data:image/...;base64,` URL or bare base64.
fn decode_inline(text: &str) -> Result<Vec<u8>, Refusal> {
    if text.starts_with("data:") {
        return crate::engine::download::decode_image_data_url(text)
            .map_err(|e| Refusal::bad(format!("image: {e:#}")));
    }
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| Refusal::bad(format!("image is neither a data: URL nor base64: {e}")))
}

fn from_form(
    content_type: &str,
    body: &[u8],
    options: &mut BTreeMap<String, String>,
) -> Result<Vec<u8>, Refusal> {
    let boundary = crate::multipart::boundary(content_type)
        .ok_or_else(|| Refusal::bad("multipart/form-data without a boundary"))?;
    let parts = crate::multipart::parse(body, &boundary)
        .map_err(|e| Refusal::bad(format!("multipart body: {e}")))?;
    let mut picture = None;
    for part in parts {
        if part.name == "image" {
            picture = Some(part.data);
        } else {
            let text = String::from_utf8(part.data)
                .map_err(|_| Refusal::bad(format!("form field {} is not UTF-8", part.name)))?;
            options.insert(part.name, text);
        }
    }
    picture.ok_or_else(|| Refusal::bad("the form has no `image` file"))
}

pub(super) fn number<T: std::str::FromStr>(
    options: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<T>, Refusal> {
    options
        .get(key)
        .map(|v| {
            v.trim()
                .parse()
                .map_err(|_| Refusal::bad(format!("{key} {v:?} is not a number")))
        })
        .transpose()
}

/// Answer a refusal as JSON (`503` with `Retry-After`), logged under `op`.
pub(super) fn refuse(request: Request, op: &'static str, refusal: Refusal) -> std::io::Result<()> {
    tracing::warn!(
        target: super::TRACE_TARGET,
        op,
        status = refusal.status,
        code = refusal.code,
        message = %refusal.message,
        "request refused"
    );
    let body = serde_json::json!({ "error": refusal.code, "message": refusal.message });
    if refusal.status == 503 {
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        let response = Response::from_data(bytes)
            .with_status_code(503)
            .with_header(Header::from_bytes("Retry-After", "2").expect("static header"))
            .with_header(
                Header::from_bytes("Content-Type", "application/json").expect("static header"),
            );
        return request.respond(response);
    }
    respond_json(request, refusal.status, &body)
}
