//! Blocking client for the daemon's local API, used by the tray UI.
//!
//! The daemon publishes its URL and bearer token in the discovery file
//! (`<config dir>/local-api.json`); [`DaemonClient::discover`] reads it
//! afresh, so a restarted daemon on a new port is found again.

use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::daemon_api::{DaemonStatus, EditableConfig, ErrorBody, LogsPage, ModelEntry};
use crate::job_log::JobLog;

/// Per-request timeout.  Every route the UI calls answers at once; a
/// daemon that takes longer is treated as unreachable.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a call to the daemon failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// No discovery file, or the daemon did not answer.
    #[error("daemon not reachable: {0}")]
    Unreachable(String),
    /// The daemon answered with an error.
    #[error("{message} ({code}, HTTP {status})")]
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    /// The daemon answered something this UI cannot read.
    #[error("unexpected answer from the daemon: {0}")]
    Unreadable(String),
}

#[derive(Deserialize)]
struct Discovery {
    url: String,
    token: String,
}

/// A client bound to one daemon's URL and token.
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::blocking::Client,
    url: String,
    token: String,
}

impl DaemonClient {
    pub fn new(url: &str, token: &str) -> Result<Self, ClientError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        Ok(Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// A client for the daemon serving the config at `config_path`, from its
    /// discovery file.
    pub fn discover(config_path: &Path) -> Result<Self, ClientError> {
        let path = crate::config::local_api_discovery_path_for(config_path)
            .ok_or_else(|| ClientError::Unreachable("no discovery path".into()))?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            ClientError::Unreachable(format!("no discovery file at {}: {e}", path.display()))
        })?;
        let discovery: Discovery = serde_json::from_str(&text).map_err(|e| {
            ClientError::Unreachable(format!("unreadable discovery file {}: {e}", path.display()))
        })?;
        Self::new(&discovery.url, &discovery.token)
    }

    /// The daemon's base URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn send(&self, request: reqwest::blocking::RequestBuilder) -> Result<Vec<u8>, ClientError> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?
            .to_vec();
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let (code, message) = match serde_json::from_slice::<ErrorBody>(&body) {
            Ok(err) => (err.error, err.message.unwrap_or_default()),
            Err(_) => (
                "http_error".to_string(),
                String::from_utf8_lossy(&body).trim().to_string(),
            ),
        };
        Err(ClientError::Refused {
            status,
            code,
            message,
        })
    }

    fn json<T: DeserializeOwned>(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<T, ClientError> {
        let body = self.send(request)?;
        serde_json::from_slice(&body).map_err(|e| ClientError::Unreadable(e.to_string()))
    }

    fn get(&self, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http.get(format!("{}{path}", self.url))
    }

    fn post(&self, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http.post(format!("{}{path}", self.url))
    }

    pub fn status(&self) -> Result<DaemonStatus, ClientError> {
        self.json(self.get("/daemon/status"))
    }

    pub fn logs(&self, after: u64) -> Result<LogsPage, ClientError> {
        self.json(self.get(&format!("/daemon/logs?after={after}")))
    }

    pub fn models(&self) -> Result<Vec<ModelEntry>, ClientError> {
        self.json(self.get("/models"))
    }

    /// The job's log; `None` when the daemon captured none.
    pub fn job_log(&self, job_id: &str) -> Result<Option<JobLog>, ClientError> {
        not_found_is_none(self.json(self.get(&format!("/jobs/{job_id}/log"))))
    }

    /// The job's PNG thumbnail; `None` when it has none.
    pub fn thumbnail(&self, job_id: &str) -> Result<Option<Vec<u8>>, ClientError> {
        not_found_is_none(self.send(self.get(&format!("/jobs/{job_id}/thumbnail"))))
    }

    /// Pause (`true`) or resume claiming studio jobs.
    pub fn set_paused(&self, paused: bool) -> Result<(), ClientError> {
        let path = if paused {
            "/daemon/pause"
        } else {
            "/daemon/resume"
        };
        self.send(self.post(path)).map(drop)
    }

    pub fn put_config(&self, edit: &EditableConfig) -> Result<EditableConfig, ClientError> {
        self.json(
            self.http
                .put(format!("{}/daemon/config", self.url))
                .json(edit),
        )
    }

    /// Load a model; answers its lifecycle state.
    pub fn load_model(&self, id: &str) -> Result<String, ClientError> {
        self.lifecycle(id, "load")
    }

    /// Unload a model; answers its lifecycle state.
    pub fn unload_model(&self, id: &str) -> Result<String, ClientError> {
        self.lifecycle(id, "unload")
    }

    fn lifecycle(&self, id: &str, verb: &str) -> Result<String, ClientError> {
        #[derive(Deserialize)]
        struct State {
            state: String,
        }
        let state: State = self.json(self.post(&format!("/models/{id}/{verb}")))?;
        Ok(state.state)
    }

    pub fn reset_registration(&self) -> Result<(), ClientError> {
        self.send(self.post("/daemon/registration/reset")).map(drop)
    }

    pub fn shutdown(&self) -> Result<(), ClientError> {
        self.send(self.post("/daemon/shutdown")).map(drop)
    }
}

fn not_found_is_none<T>(result: Result<T, ClientError>) -> Result<Option<T>, ClientError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ClientError::Refused { status: 404, .. }) => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::DaemonHarness;

    #[test]
    fn discovery_finds_the_daemon_and_reads_its_status() {
        let daemon = DaemonHarness::start();
        let client = DaemonClient::discover(&daemon.config_path).unwrap();
        assert_eq!(client.url(), daemon.url);
        let status = client.status().unwrap();
        assert_eq!(status.version, crate::AGENT_VERSION);
    }

    #[test]
    fn a_missing_discovery_file_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let err = DaemonClient::discover(&dir.path().join("config.toml"))
            .err()
            .unwrap();
        assert!(matches!(err, ClientError::Unreachable(_)), "{err}");
    }

    #[test]
    fn an_unreadable_discovery_file_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("local-api.json"), "{").unwrap();
        let err = DaemonClient::discover(&dir.path().join("config.toml"))
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("unreadable discovery file"),
            "{err}"
        );
    }

    #[test]
    fn a_closed_port_is_unreachable() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = DaemonClient::new(&format!("http://127.0.0.1:{port}"), "t").unwrap();
        assert!(matches!(client.status(), Err(ClientError::Unreachable(_))));
    }

    #[test]
    fn a_wrong_token_is_refused_with_the_status() {
        let daemon = DaemonHarness::start();
        let client = DaemonClient::new(&daemon.url, "wrong").unwrap();
        let err = client.status().unwrap_err();
        assert!(
            matches!(err, ClientError::Refused { status: 401, ref code, .. } if code == "http_error"),
            "{err}"
        );
    }

    #[test]
    fn pause_and_resume_reach_the_daemon() {
        let daemon = DaemonHarness::start();
        let client = daemon.client();
        client.set_paused(true).unwrap();
        assert!(client.status().unwrap().paused);
        client.set_paused(false).unwrap();
        assert!(!client.status().unwrap().paused);
    }

    #[test]
    fn a_config_update_round_trips_and_a_bad_one_is_refused() {
        let daemon = DaemonHarness::start();
        let client = daemon.client();
        let mut edit = client.status().unwrap().config;
        edit.vram_threshold_gb = 9.0;
        assert_eq!(client.put_config(&edit).unwrap().vram_threshold_gb, 9.0);

        edit.auto_update_interval_secs = 1;
        let err = client.put_config(&edit).unwrap_err();
        assert!(
            matches!(err, ClientError::Refused { status: 400, ref code, .. } if code == "invalid_config"),
            "{err}"
        );
    }

    #[test]
    fn models_load_and_unload() {
        let daemon = DaemonHarness::start();
        let client = daemon.client();
        let models = client.models().unwrap();
        assert_eq!(models[0].id, "chat");
        assert_eq!(models[0].state, "unloaded");
        client.load_model("chat").unwrap();
        daemon.wait_state("chat", "loaded");
        assert!(client.models().unwrap()[0].resident);
        client.unload_model("chat").unwrap();
        daemon.wait_state("chat", "unloaded");
        let err = client.load_model("nope").unwrap_err();
        assert!(
            matches!(err, ClientError::Refused { status: 404, ref code, .. } if code == "unknown_model"),
            "{err}"
        );
    }

    #[test]
    fn a_job_log_and_thumbnail_are_fetched_or_absent() {
        crate::test_support::install_job_log_capture();
        let daemon = DaemonHarness::start();
        let client = daemon.client();
        let job_id = daemon.run_image_job();
        let log = client.job_log(&job_id).unwrap().expect("a log");
        assert!(log
            .lines
            .iter()
            .any(|l| l.message.starts_with("job finished")));
        let png = client.thumbnail(&job_id).unwrap().expect("a thumbnail");
        assert!(image::load_from_memory(&png).is_ok());
        assert_eq!(client.job_log("no-such-job").unwrap(), None);
        assert_eq!(client.thumbnail("no-such-job").unwrap(), None);
    }

    #[test]
    fn logs_page_after_a_sequence_number() {
        let daemon = DaemonHarness::start();
        let client = daemon.client();
        daemon.push_log("first");
        daemon.push_log("second");
        let page = client.logs(0).unwrap();
        assert_eq!(page.seq, 2);
        assert_eq!(page.entries.len(), 2);
        let page = client.logs(1).unwrap();
        assert_eq!(page.entries[0].message, "second");
    }

    #[test]
    fn a_registration_reset_is_refused_unless_rejected_and_shutdown_stops() {
        let daemon = DaemonHarness::start();
        let client = daemon.client();
        let err = client.reset_registration().unwrap_err();
        assert!(
            matches!(err, ClientError::Refused { status: 409, .. }),
            "{err}"
        );
        *daemon.control.registration.lock() =
            crate::auto_register::RegistrationState::Rejected { reason: "x".into() };
        client.reset_registration().unwrap();
        client.shutdown().unwrap();
        assert!(daemon
            .control
            .stop
            .load(std::sync::atomic::Ordering::SeqCst));
    }
}
