//! `/wake-words`: train wake word models (`docs/engines/wake-word.md`, "Local API").

use serde_json::json;
use tiny_http::{Method, Request};

use super::{read_body, respond_json, BodyOutcome, LocalApi};
use crate::wake_word::{request, CancelRefusal, Refusal};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Whether `path` is one of the wake word routes.
pub(super) fn is_route(path: &str) -> bool {
    path == "/wake-words" || path.starts_with("/wake-words/")
}

/// `/wake-words/<id>/files/<file>` -> `Some((id, file))`, each non-empty and without slashes.
fn output_route(path: &str) -> Option<(&str, &str)> {
    let (id, file) = path.strip_prefix("/wake-words/")?.split_once("/files/")?;
    let plain = |s: &str| !s.is_empty() && !s.contains('/');
    (plain(id) && plain(file)).then_some((id, file))
}

impl LocalApi {
    fn serve_output_file(
        &self,
        request: Request,
        service: &crate::wake_word::WakeWords,
        id: &str,
        file: &str,
    ) -> std::io::Result<()> {
        use crate::wake_word::files::FileRefusal;
        match service.output_file(id, file) {
            Ok(found) => match std::fs::read(&found.path) {
                Ok(bytes) => super::respond(request, 200, found.content_type, &bytes),
                Err(e) => respond_json(request, 500, &error("not_read", format!("reading {file}: {e}"))),
            },
            Err(FileRefusal::UnknownTraining) => respond_json(
                request,
                404,
                &error("unknown_training", format!("no completed training {id} has output files")),
            ),
            Err(FileRefusal::UnknownFile) => respond_json(
                request,
                404,
                &error(
                    "unknown_file",
                    format!("{file} is not an output of training {id}: <name>.tflite, <name>.json, report.json, report.md"),
                ),
            ),
        }
    }
}

/// `/wake-words/<id>` -> `Some(id)` for a non-empty id without slashes.
fn training_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/wake-words/")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn error(code: &str, message: impl Into<String>) -> serde_json::Value {
    json!({"error": code, "message": message.into()})
}

fn refusal(refusal: Refusal) -> (u16, serde_json::Value) {
    match refusal {
        Refusal::Busy => (
            409,
            error(
                "busy",
                "another job holds the worker; a training needs it to itself",
            ),
        ),
        Refusal::UnsupportedPlatform(reason) => (
            503,
            error(
                "unsupported_platform",
                format!("wake word training needs Linux x86_64 with an NVIDIA GPU: {reason}"),
            ),
        ),
        Refusal::InsufficientDisk {
            needed_bytes,
            free_bytes,
        } => {
            let needed_gib = needed_bytes as f64 / GIB;
            let free_gib = free_bytes as f64 / GIB;
            let mut body = error(
                "insufficient_disk",
                format!(
                    "the training needs about {needed_gib:.1} GiB on the models filesystem; {free_gib:.1} GiB are free"
                ),
            );
            body["neededGib"] = json!((needed_gib * 10.0).round() / 10.0);
            body["freeGib"] = json!((free_gib * 10.0).round() / 10.0);
            (507, body)
        }
        Refusal::Unavailable(message) => (500, error("unavailable", message)),
        Refusal::InvalidVoiceSamples(message) => (400, error("invalid_voice_samples", message)),
        Refusal::VoiceSamplesUnavailable => (
            503,
            error(
                "voice_samples_unavailable",
                "the request names recording sets; this worker keeps none",
            ),
        ),
    }
}

impl LocalApi {
    pub(super) fn handle_wake_words(
        &self,
        mut request: Request,
        method: &Method,
        path: &str,
    ) -> std::io::Result<()> {
        let Some(service) = self.wake_words.clone() else {
            return respond_json(
                request,
                503,
                &error(
                    "wake_words_unavailable",
                    "this worker does not train wake words",
                ),
            );
        };
        fn as_json(value: impl serde::Serialize) -> serde_json::Value {
            serde_json::to_value(value).expect("a training serialises")
        }
        match (method, path) {
            (Method::Post, "/wake-words") => {
                let body = match read_body(&mut request)? {
                    BodyOutcome::Ok(body) => body,
                    BodyOutcome::TooLarge => {
                        return respond_json(
                            request,
                            413,
                            &error("too_large", "the body is over 1 MiB"),
                        )
                    }
                };
                let spec = match request::parse(body.as_bytes()) {
                    Ok(spec) => spec,
                    Err(invalid) => {
                        return respond_json(request, 400, &error("invalid_request", invalid.0))
                    }
                };
                match service.start(spec) {
                    Ok(training) => respond_json(request, 202, &as_json(training)),
                    Err(refused) => {
                        let (status, body) = refusal(refused);
                        respond_json(request, status, &body)
                    }
                }
            }
            (Method::Get, "/wake-words") => respond_json(request, 200, &as_json(service.list())),
            (Method::Get, path) if output_route(path).is_some() => {
                let (id, file) = output_route(path).expect("matched just above");
                self.serve_output_file(request, &service, id, file)
            }
            (method, path) => {
                let Some(id) = training_id(path) else {
                    return respond_json(
                        request,
                        404,
                        &error("not_found", format!("no route {path}")),
                    );
                };
                match method {
                    Method::Get => match service.get(id) {
                        Some(training) => respond_json(request, 200, &as_json(training)),
                        None => respond_json(
                            request,
                            404,
                            &error("unknown_training", format!("no training {id}")),
                        ),
                    },
                    Method::Delete => match service.cancel(id) {
                        Ok(training) => respond_json(request, 202, &as_json(training)),
                        Err(CancelRefusal::Unknown) => respond_json(
                            request,
                            404,
                            &error("unknown_training", format!("no training {id}")),
                        ),
                        Err(CancelRefusal::NotRunning) => respond_json(
                            request,
                            409,
                            &error("not_running", format!("training {id} has already ended")),
                        ),
                    },
                    _ => respond_json(
                        request,
                        405,
                        &error(
                            "method_not_allowed",
                            format!("{method} {path} is not a route"),
                        ),
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_routes_parse() {
        assert_eq!(
            output_route("/wake-words/local-1-1/files/report.md"),
            Some(("local-1-1", "report.md"))
        );
        assert_eq!(output_route("/wake-words/local-1-1/files/"), None);
        assert_eq!(output_route("/wake-words/local-1-1/files/a/b"), None);
        assert_eq!(output_route("/wake-words//files/x"), None);
        assert_eq!(output_route("/wake-words/local-1-1"), None);
    }

    #[test]
    fn routes_and_ids_parse() {
        assert!(is_route("/wake-words"));
        assert!(is_route("/wake-words/local-1"));
        assert!(!is_route("/wake-wordsx"));
        assert_eq!(training_id("/wake-words/local-1"), Some("local-1"));
        assert_eq!(training_id("/wake-words/"), None);
        assert_eq!(training_id("/wake-words/a/b"), None);
    }

    #[test]
    fn every_refusal_has_its_status_and_code() {
        let code = |r| {
            let (status, body) = refusal(r);
            (status, body["error"].as_str().unwrap().to_string())
        };
        assert_eq!(code(Refusal::Busy), (409, "busy".into()));
        assert_eq!(
            code(Refusal::UnsupportedPlatform("x".into())),
            (503, "unsupported_platform".into())
        );
        assert_eq!(
            code(Refusal::Unavailable("x".into())),
            (500, "unavailable".into())
        );
        assert_eq!(
            code(Refusal::InvalidVoiceSamples("x".into())),
            (400, "invalid_voice_samples".into())
        );
        assert_eq!(
            code(Refusal::VoiceSamplesUnavailable),
            (503, "voice_samples_unavailable".into())
        );
        let (status, body) = refusal(Refusal::InsufficientDisk {
            needed_bytes: 30 * 1024 * 1024 * 1024,
            free_bytes: 1024 * 1024 * 1024,
        });
        assert_eq!(status, 507);
        assert_eq!(body["neededGib"], 30.0);
        assert_eq!(body["freeGib"], 1.0);
        assert!(body["message"].as_str().unwrap().contains("about 30.0 GiB"));
    }
}
