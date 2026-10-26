//! The operator's recordings on the loopback API (`docs/engines/wake-word.md`, "Loopback routes"):
//! recording tokens, and listing and deleting sets.  Takes themselves arrive over the LAN listener.

use serde::Deserialize;
use serde_json::json;
use tiny_http::{Method, Request};

use super::{read_body, respond_json, BodyOutcome, LocalApi, TRACE_TARGET};
use crate::voice_samples::{take, StoreError, DEFAULT_TOKEN_TTL_SECS, PATH};

/// What a recording token opens, for the log.
const SCOPE: &str = "voice-samples";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenBody {
    #[serde(default)]
    ttl_secs: Option<i64>,
}

fn error(code: &str, message: impl Into<String>) -> serde_json::Value {
    json!({"error": code, "message": message.into()})
}

/// Whether `path` is one of the loopback recording routes.
pub(super) fn is_route(path: &str) -> bool {
    path == "/voice-sample-tokens" || path == PATH || path.starts_with("/voice-samples/")
}

impl LocalApi {
    pub(super) fn handle_voice_samples(
        &self,
        mut request: Request,
        method: &Method,
        path: &str,
    ) -> std::io::Result<()> {
        if path == "/voice-sample-tokens" {
            if *method != Method::Post {
                return respond_json(
                    request,
                    405,
                    &error("method_not_allowed", "POST a recording token"),
                );
            }
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
            let body = if body.trim().is_empty() {
                "{}"
            } else {
                body.as_str()
            };
            return match serde_json::from_str::<TokenBody>(body) {
                Ok(parsed) => self.mint_recording_token(request, parsed),
                Err(e) => respond_json(request, 400, &error("bad_request", e.to_string())),
            };
        }
        let Some(store) = self.services.voice_samples.clone() else {
            return respond_json(
                request,
                503,
                &error("voice_samples_unavailable", "this worker keeps no takes"),
            );
        };
        let set = path.strip_prefix("/voice-samples/");
        match (method, set) {
            (Method::Get, None) => match store.list() {
                Ok(sets) => respond_json(request, 200, &json!({ "sets": sets })),
                Err(e) => respond_json(request, 500, &error("not_listed", e.to_string())),
            },
            (Method::Delete, set) => {
                if let Some(set) = set {
                    if let Err(bad) = take::check_set(set) {
                        return respond_json(request, 400, &error("bad_name", bad.0));
                    }
                }
                match store.delete(set) {
                    Ok(deleted) => {
                        tracing::info!(target: TRACE_TARGET, op = "voice_samples", ?deleted, "sets deleted");
                        respond_json(request, 200, &json!({ "deleted": deleted }))
                    }
                    Err(StoreError::UnknownSet(set)) => respond_json(
                        request,
                        404,
                        &error("unknown_set", format!("no set named {set}")),
                    ),
                    Err(StoreError::InTraining(set)) => respond_json(
                        request,
                        409,
                        &error(
                            "set_in_training",
                            format!("a training holds set {set}; delete it once the training ends"),
                        ),
                    ),
                    Err(e) => respond_json(request, 500, &error("not_deleted", e.to_string())),
                }
            }
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

    fn mint_recording_token(&self, request: Request, body: TokenBody) -> std::io::Result<()> {
        if self.services.voice_samples.is_none() {
            return respond_json(
                request,
                503,
                &error("voice_samples_unavailable", "this worker keeps no takes"),
            );
        }
        let endpoint = self.services.stream_endpoint.read().clone();
        let Some(endpoint) = endpoint else {
            return respond_json(
                request,
                503,
                &error("stream_listener_down", "the LAN listener is not up"),
            );
        };
        let ttl = chrono::Duration::seconds(body.ttl_secs.unwrap_or(DEFAULT_TOKEN_TTL_SECS));
        let grant = self
            .services
            .voice_tokens
            .mint(SCOPE, ttl, chrono::Utc::now());
        let host = match (self.services.lan_address)() {
            Ok(address) => Some(address.to_string()),
            Err(why) => {
                tracing::warn!(target: TRACE_TARGET, op = "recording_token", error = %why,
                    "recording grant has no LAN address; only a loopback client can use it");
                None
            }
        };
        tracing::info!(
            target: TRACE_TARGET,
            op = "recording_token",
            expires_at = %grant.expires_at,
            host = host.as_deref().unwrap_or(""),
            "recording token minted"
        );
        respond_json(
            request,
            200,
            &json!({
                "token": grant.token,
                "expiresAt": grant.expires_at.to_rfc3339(),
                "scheme": "https",
                "host": host,
                "port": endpoint.port,
                "path": PATH,
                "certSha256": endpoint.cert_sha256,
                "certPem": endpoint.cert_pem,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_routes_are_recognised() {
        assert!(is_route("/voice-sample-tokens"));
        assert!(is_route("/voice-samples"));
        assert!(is_route("/voice-samples/s1"));
        assert!(!is_route("/voice-samplesx"));
    }
}
