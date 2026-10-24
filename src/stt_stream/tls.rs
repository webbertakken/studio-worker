//! The stream listener's TLS identity: one self-signed certificate, made on
//! the daemon's first start and kept next to the config, so every client
//! can pin it across restarts.
//!
//! The certificate is public: its SHA-256 (the pin) and PEM ride the stream
//! grant on the loopback API.  The private key never leaves its owner-only
//! file and is never logged.

use anyhow::Context as _;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The certificate, beside the config.  Public.
pub const CERT_FILE: &str = "stream-cert.pem";
/// Its private key, beside the config.  Owner-only; never logged or served.
pub const KEY_FILE: &str = "stream-key.pem";
/// The names the certificate carries: loopback clients check them; LAN
/// clients dial an address it cannot know, so they pin instead.
pub const CERT_NAMES: [&str; 4] = ["studio-worker", "localhost", "127.0.0.1", "::1"];
/// How long a made certificate is valid.  Rotation is deliberate (delete the
/// pair), never by expiry.  Safe range 1..=50 years.
pub const CERT_VALIDITY_YEARS: i32 = 25;
const TRACE_TARGET: &str = "studio_worker::stt_stream";

/// The listener's certificate and key, loaded or made.
pub struct StreamCertificate {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    sha256: String,
}

impl std::fmt::Debug for StreamCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCertificate")
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

impl StreamCertificate {
    /// The pair kept in `dir`, or a new one written there when it is missing,
    /// unreadable or mismatched (a warning names why).
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        if cert_path.exists() || key_path.exists() {
            match Self::load(&cert_path, &key_path) {
                Ok(cert) => {
                    tracing::info!(target: TRACE_TARGET, op = "tls", cert_sha256 = %cert.sha256, "stream certificate loaded");
                    return Ok(cert);
                }
                Err(e) => {
                    tracing::warn!(target: TRACE_TARGET, op = "tls", dir = %dir.display(), error = %e, "stream certificate unusable; making a new one");
                }
            }
        }
        let cert = Self::create(&cert_path, &key_path)?;
        tracing::info!(target: TRACE_TARGET, op = "tls", cert_sha256 = %cert.sha256, dir = %dir.display(), "stream certificate created");
        Ok(cert)
    }

    /// Lowercase hex SHA-256 of the certificate's DER: what a client pins.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The certificate as PEM, for a client that trusts it as its only root.
    pub fn pem(&self) -> &str {
        &self.cert_pem
    }

    /// The certificate's DER encoding.
    pub fn der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// A TLS server configuration presenting this certificate (ring, TLS 1.2
    /// and 1.3, no client certificates).
    pub fn server_config(&self) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("stream tls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(vec![self.cert_der.clone()], self.key_der.clone_key())
        .context("stream certificate and key do not form a usable pair")?;
        Ok(Arc::new(config))
    }

    fn from_parts(cert_pem: String, key_der: PrivateKeyDer<'static>) -> anyhow::Result<Self> {
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| anyhow::anyhow!("{CERT_FILE} holds no certificate: {e}"))?;
        let sha256 = hex(&Sha256::digest(cert_der.as_ref()));
        let cert = Self {
            cert_pem,
            cert_der,
            key_der,
            sha256,
        };
        cert.server_config()?;
        Ok(cert)
    }

    fn load(cert_path: &Path, key_path: &Path) -> anyhow::Result<Self> {
        let cert_pem = std::fs::read_to_string(cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key_pem =
            std::fs::read(key_path).with_context(|| format!("reading {}", key_path.display()))?;
        // The parser's error never quotes the key's bytes.
        let key_der = PrivateKeyDer::from_pem_slice(&key_pem)
            .map_err(|e| anyhow::anyhow!("{KEY_FILE} holds no private key: {e}"))?;
        Self::from_parts(cert_pem, key_der)
    }

    fn create(cert_path: &Path, key_path: &Path) -> anyhow::Result<Self> {
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .context("generating the stream key")?;
        let mut params = rcgen::CertificateParams::new(CERT_NAMES.map(String::from).to_vec())
            .context("stream certificate names")?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, CERT_NAMES[0]);
        use chrono::Datelike as _;
        let ymd =
            |d: chrono::NaiveDate| rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8);
        let (not_before, not_after) = validity(chrono::Utc::now().date_naive());
        params.not_before = ymd(not_before);
        params.not_after = ymd(not_after);
        let cert = params
            .self_signed(&key_pair)
            .context("signing the stream certificate")?;
        let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| anyhow::anyhow!("encoding the stream key: {e}"))?;
        // Key first: a crash between the two writes leaves a pair that fails
        // to load and is replaced, never a certificate without its key.
        crate::config::write_atomic(key_path, key_pair.serialize_pem().as_bytes())
            .with_context(|| format!("writing {}", key_path.display()))?;
        crate::config::write_atomic(cert_path, cert.pem().as_bytes())
            .with_context(|| format!("writing {}", cert_path.display()))?;
        Self::from_parts(cert.pem(), key_der)
    }
}

/// Where the pair lives for a config at `config_path`: its directory.
pub fn dir_for(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// From the day before `today` (a client's clock may lag) to
/// [`CERT_VALIDITY_YEARS`] after it.
fn validity(today: chrono::NaiveDate) -> (chrono::NaiveDate, chrono::NaiveDate) {
    use chrono::Datelike as _;
    let start = today.pred_opt().unwrap_or(today);
    // 29 February has no twin in most years: fall back to 28 February.
    let end = start
        .with_year(start.year() + CERT_VALIDITY_YEARS)
        .or_else(|| (start - chrono::Days::new(1)).with_year(start.year() + CERT_VALIDITY_YEARS))
        .unwrap_or(start);
    (start, end)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier as _;
    use rustls::pki_types::{ServerName, UnixTime};

    fn verify_name(cert: &StreamCertificate, name: &'static str) -> Result<(), rustls::Error> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        verifier
            .verify_server_cert(
                cert.der(),
                &[],
                &ServerName::try_from(name).unwrap(),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    #[test]
    fn a_first_start_makes_a_pair_and_later_starts_keep_it() {
        let dir = tempfile::tempdir().unwrap();
        let made = StreamCertificate::load_or_create(dir.path()).unwrap();
        assert!(dir.path().join(CERT_FILE).is_file());
        assert!(dir.path().join(KEY_FILE).is_file());
        let kept = StreamCertificate::load_or_create(dir.path()).unwrap();
        assert_eq!(made.sha256(), kept.sha256());
        assert_eq!(made.pem(), kept.pem());
    }

    #[test]
    fn the_pin_is_the_lowercase_hex_sha256_of_the_der() {
        let dir = tempfile::tempdir().unwrap();
        let cert = StreamCertificate::load_or_create(dir.path()).unwrap();
        let expected: String = Sha256::digest(cert.der().as_ref())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(cert.sha256(), expected);
        assert_eq!(cert.sha256().len(), 64);
        assert!(cert
            .sha256()
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    }

    #[test]
    fn the_pem_holds_the_pinned_certificate_and_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let cert = StreamCertificate::load_or_create(dir.path()).unwrap();
        let parsed = CertificateDer::from_pem_slice(cert.pem().as_bytes()).unwrap();
        assert_eq!(&parsed, cert.der());
        assert!(!cert.pem().contains("PRIVATE KEY"));
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        StreamCertificate::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "key mode {mode:o}");
    }

    #[test]
    fn an_unreadable_certificate_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let first = StreamCertificate::load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(CERT_FILE), "not a certificate").unwrap();
        let second = StreamCertificate::load_or_create(dir.path()).unwrap();
        assert_ne!(first.sha256(), second.sha256());
        assert_eq!(
            StreamCertificate::load_or_create(dir.path())
                .unwrap()
                .sha256(),
            second.sha256(),
            "the replacement is kept"
        );
    }

    #[test]
    fn a_missing_key_is_replaced_with_a_new_pair() {
        let dir = tempfile::tempdir().unwrap();
        let first = StreamCertificate::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        let second = StreamCertificate::load_or_create(dir.path()).unwrap();
        assert_ne!(first.sha256(), second.sha256());
    }

    #[test]
    fn a_key_that_is_not_the_certificates_is_replaced() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let first = StreamCertificate::load_or_create(a.path()).unwrap();
        StreamCertificate::load_or_create(b.path()).unwrap();
        std::fs::copy(b.path().join(KEY_FILE), a.path().join(KEY_FILE)).unwrap();
        let second = StreamCertificate::load_or_create(a.path()).unwrap();
        assert_ne!(first.sha256(), second.sha256());
        second.server_config().unwrap();
    }

    #[test]
    fn the_certificate_names_studio_worker_and_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let cert = StreamCertificate::load_or_create(dir.path()).unwrap();
        for name in CERT_NAMES {
            verify_name(&cert, name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert!(verify_name(&cert, "192.168.1.10").is_err());
    }

    #[test]
    fn validity_runs_from_yesterday_for_the_set_years() {
        let day = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
        let (from, to) = validity(day(2026, 3, 1));
        assert_eq!(from, day(2026, 2, 28));
        assert_eq!(to, day(2026 + CERT_VALIDITY_YEARS, 2, 28));
        let (from, to) = validity(day(2028, 3, 1));
        assert_eq!(from, day(2028, 2, 29));
        assert_eq!(
            to,
            day(2028 + CERT_VALIDITY_YEARS, 2, 28),
            "a leap day falls back"
        );
    }

    #[test]
    fn debug_shows_the_pin_and_never_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let cert = StreamCertificate::load_or_create(dir.path()).unwrap();
        let shown = format!("{cert:?}");
        assert!(shown.contains(cert.sha256()));
        assert!(!shown.contains("key"));
    }

    #[test]
    fn the_pair_lives_beside_the_config() {
        assert_eq!(
            dir_for(Path::new("/tmp/x/config.toml")),
            PathBuf::from("/tmp/x")
        );
        assert_eq!(dir_for(Path::new("config.toml")), PathBuf::from("."));
    }
}
