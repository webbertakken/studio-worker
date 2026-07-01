//! Shared transport-level guards for everything the worker downloads.
//!
//! Model files, `sd-cli` archives, ONNX runtimes, and auto-update
//! installers all arrive over HTTP and are then either loaded into an
//! engine or executed — so a plaintext-`http` fetch is a
//! man-in-the-middle away from model poisoning or remote code
//! execution.  Every downloader routes its URL through
//! [`validate_download_url`] before the first byte is requested.

use anyhow::{bail, Context, Result};

/// Refuse any download URL that is not `https`.  Loopback `http` is
/// allowed so test suites (wiremock) and air-gapped local mirrors keep
/// working; everything else — remote `http`, `file`, `ftp`, garbage —
/// is rejected before any request is made.  `what` names the artefact
/// class (e.g. `"model file"`, `"installer"`) so the error tells the
/// operator which config/registry entry to fix.
pub fn validate_download_url(raw: &str, what: &str) -> Result<()> {
    let url = url::Url::parse(raw).with_context(|| format!("invalid {what} URL {raw:?}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() == "http" {
        // Typed hosts so bracketed IPv6 (`[::1]`) is recognised too —
        // `host_str()` keeps the brackets and defeats `IpAddr::parse`.
        match url.host() {
            Some(url::Host::Domain(d)) if d.eq_ignore_ascii_case("localhost") => return Ok(()),
            Some(url::Host::Ipv4(ip)) if ip.is_loopback() => return Ok(()),
            Some(url::Host::Ipv6(ip)) if ip.is_loopback() => return Ok(()),
            _ => {}
        }
    }
    bail!("{what} URL must use https (loopback http is allowed for tests/mirrors): {raw}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_urls_pass() {
        validate_download_url(
            "https://huggingface.co/x/y/resolve/main/m.gguf",
            "model file",
        )
        .unwrap();
        validate_download_url(
            "https://github.com/o/r/releases/download/v1/i.sh",
            "installer",
        )
        .unwrap();
    }

    #[test]
    fn loopback_http_passes_for_tests_and_mirrors() {
        validate_download_url("http://127.0.0.1:1234/m.gguf", "model file").unwrap();
        validate_download_url("http://localhost:1234/m.gguf", "model file").unwrap();
        validate_download_url("http://[::1]:1234/m.gguf", "model file").unwrap();
    }

    #[test]
    fn remote_http_is_rejected() {
        let err = validate_download_url("http://example.com/m.gguf", "model file")
            .unwrap_err()
            .to_string();
        assert!(err.contains("https"), "got: {err}");
        assert!(
            err.contains("model file"),
            "must name the artefact class: {err}"
        );
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/m.gguf",
            "javascript:alert(1)",
        ] {
            let err = validate_download_url(raw, "model file")
                .unwrap_err()
                .to_string();
            assert!(err.contains("https"), "{raw} must be rejected: {err}");
        }
    }

    #[test]
    fn malformed_urls_error_with_parse_context() {
        let err = validate_download_url("not a url", "model file")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid model file URL"), "got: {err}");
    }
}
