//! Takes arriving over the LAN listener (`docs/engines/wake-word.md`, "Uploads: the LAN listener"):
//! the listener reads each request's head itself; `PUT /voice-samples/...` is answered here as
//! plain HTTP/1.1, everything for `/transcribe` goes on to the WebSocket handshake.

use std::io::{Read, Write};

use chrono::Utc;
use serde_json::json;

use super::take::{parse_path, MAX_TAKE_BYTES};
use super::{StoreError, VoiceSamples, PATH, TRACE_TARGET};
use crate::stt_stream::tokens::StreamTokens;

/// The longest request head read.  Safe range 4 to 64 KiB.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// A request's head: its method, path and headers (names lower-cased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The token in `Authorization: Bearer <token>` (scheme in any case).
    pub fn bearer(&self) -> Option<&str> {
        let (scheme, token) = self.header("authorization")?.split_once(' ')?;
        let token = token.trim();
        (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
    }
}

/// Read `bytes` (a whole head, without its blank line) into a [`Head`].
pub fn parse_head(bytes: &[u8]) -> Result<Head, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "the request head is not UTF-8".to_string())?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("not a request line: {line:?}"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(format!("not HTTP/1.x: {version:?}"));
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query.to_string())),
        None => (target, None),
    };
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("not a header: {line:?}"));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Head {
        method: method.to_string(),
        path: path.to_string(),
        query,
        headers,
    })
}

/// A head read from a stream: the head, every byte read, and where its body starts in them.
#[derive(Debug)]
pub struct ReadHead {
    pub head: Head,
    pub consumed: Vec<u8>,
    pub body_start: usize,
}

impl ReadHead {
    /// Whatever of the body came with the head.
    pub fn body(&self) -> Vec<u8> {
        self.consumed[self.body_start..].to_vec()
    }
}

/// Read a request head from `stream`.
pub fn read_head(stream: &mut impl Read) -> std::io::Result<ReadHead> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = parse_head(&buffer[..end])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(ReadHead {
                head,
                consumed: buffer,
                body_start: end + 4,
            });
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the request head is too long",
            ));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// A stream whose first bytes are `prefix` (read already), then `inner`'s.
pub struct Prefixed<S> {
    prefix: std::io::Cursor<Vec<u8>>,
    pub inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: std::io::Cursor::new(prefix),
            inner,
        }
    }
}

impl<S: Read> Read for Prefixed<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let read = self.prefix.read(out)?;
        if read > 0 {
            return Ok(read);
        }
        self.inner.read(out)
    }
}

impl<S: Write> Write for Prefixed<S> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Whether a request is for the upload routes (anything under `/voice-samples`).
pub fn is_upload_path(path: &str) -> bool {
    path == PATH || path.starts_with("/voice-samples/")
}

fn refusal(status: u16, error: &str, message: impl Into<String>) -> (u16, serde_json::Value) {
    (status, json!({"error": error, "message": message.into()}))
}

/// Everything about a request but its body: the answer when it is refused before the body.
fn precheck(
    head: &Head,
    tokens: &StreamTokens,
    store: Option<&VoiceSamples>,
) -> Result<u64, (u16, serde_json::Value)> {
    if head.method != "PUT" {
        return Err(refusal(405, "method_not_allowed", "upload a take with PUT"));
    }
    match head.bearer().map(|t| tokens.check(t, Utc::now())) {
        Some(Ok(_)) => {}
        Some(Err(rejection)) => {
            return Err(refusal(
                401,
                "unauthorized",
                rejection.to_string().replace("stream", "recording"),
            ))
        }
        None => {
            return Err(refusal(
                401,
                "unauthorized",
                "missing recording token; send Authorization: Bearer <token>",
            ))
        }
    }
    if store.is_none() {
        return Err(refusal(
            503,
            "voice_samples_unavailable",
            "this worker keeps no takes",
        ));
    }
    parse_path(&head.path).map_err(|e| refusal(400, "bad_name", e.0))?;
    if head
        .header("transfer-encoding")
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        return Err(refusal(
            411,
            "length_required",
            "send Content-Length; chunked bodies are refused",
        ));
    }
    let Some(length) = head
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return Err(refusal(411, "length_required", "send Content-Length"));
    };
    if length > MAX_TAKE_BYTES {
        return Err(refusal(
            413,
            "too_large",
            format!("{length} bytes is over the {MAX_TAKE_BYTES} a take may be"),
        ));
    }
    Ok(length)
}

/// The answer to an upload whose body is `body`.
fn store_take(head: &Head, store: &VoiceSamples, body: &[u8]) -> (u16, serde_json::Value) {
    let (set, take) = parse_path(&head.path).expect("checked before the body was read");
    match store.put(&set, &take, body) {
        Ok(stored) => (
            if stored.created { 201 } else { 200 },
            json!({
                "set": set,
                "kind": take.kind,
                "take": take.stem(),
                "number": take.number,
                "phrase": take.phrase,
                "seconds": stored.seconds,
                "bytes": stored.bytes,
                "created": stored.created,
            }),
        ),
        Err(StoreError::BadWav(why)) => refusal(400, "bad_wav", why),
        Err(StoreError::InTraining(set)) => refusal(
            409,
            "set_in_training",
            format!("a training holds set {set}; upload once it ends"),
        ),
        Err(error) => refusal(500, "not_stored", error.to_string()),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// Write an HTTP/1.1 response with a JSON body, and close.
pub fn respond(
    stream: &mut impl Write,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()
}

/// Answer one upload request read from `stream`, whose head and first body bytes are `head` and
/// `already`.  Returns the status answered.
pub fn serve(
    stream: &mut (impl Read + Write),
    head: &Head,
    already: Vec<u8>,
    tokens: &StreamTokens,
    store: Option<&VoiceSamples>,
) -> std::io::Result<u16> {
    let (status, body) = match precheck(head, tokens, store) {
        Err(answer) => {
            // Read a body of known, take-sized length before refusing: closing with it unread
            // resets the connection, and the client may lose the answer.
            if head.header("expect").is_none() {
                discard_body(stream, head, already.len())?;
            }
            answer
        }
        Ok(length) => {
            if head
                .header("expect")
                .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
            {
                write!(stream, "HTTP/1.1 100 Continue\r\n\r\n")?;
                stream.flush()?;
            }
            let mut body = already;
            body.truncate(length as usize);
            let missing = length as usize - body.len();
            let mut rest = vec![0u8; missing];
            stream.read_exact(&mut rest)?;
            body.extend(rest);
            store_take(head, store.expect("checked by precheck"), &body)
        }
    };
    respond(stream, status, &body)?;
    Ok(status)
}

/// Read and drop the rest of a refused request's body, when its length is known and take-sized.
fn discard_body(stream: &mut impl Read, head: &Head, already: usize) -> std::io::Result<()> {
    let Some(length) = head
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|l| *l <= MAX_TAKE_BYTES)
    else {
        return Ok(());
    };
    let rest = (length as usize).saturating_sub(already) as u64;
    std::io::copy(&mut stream.take(rest), &mut std::io::sink())?;
    Ok(())
}

/// [`serve`], logged.
pub fn serve_logged(
    stream: &mut (impl Read + Write),
    head: &Head,
    already: Vec<u8>,
    tokens: &StreamTokens,
    store: Option<&VoiceSamples>,
    peer: std::net::SocketAddr,
) {
    match serve(stream, head, already, tokens, store) {
        Ok(status) if status < 300 => {
            tracing::info!(target: TRACE_TARGET, op = "upload", %peer, path = %head.path, status, "take stored")
        }
        Ok(status) => {
            tracing::warn!(target: TRACE_TARGET, op = "upload", %peer, path = %head.path, status, "take refused")
        }
        Err(e) => {
            tracing::warn!(target: TRACE_TARGET, op = "upload", %peer, path = %head.path, error = %e, "upload broke off")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_samples::take::tests::wav;

    /// A duplex in-memory stream: reads `input`, collects what is written.
    struct Duplex {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(out)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: VoiceSamples,
        tokens: StreamTokens,
        token: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = VoiceSamples::new(dir.path().join("voice-samples"));
        let tokens = StreamTokens::default();
        let token = tokens
            .mint("voice-samples", chrono::Duration::minutes(5), Utc::now())
            .token;
        Fixture {
            _dir: dir,
            store,
            tokens,
            token,
        }
    }

    /// Send one raw request; the status and the JSON answered.
    fn send(f: &Fixture, request: Vec<u8>, store: bool) -> (u16, serde_json::Value, String) {
        let mut stream = Duplex {
            input: std::io::Cursor::new(request),
            output: Vec::new(),
        };
        let read = read_head(&mut stream).unwrap();
        let status = serve(
            &mut stream,
            &read.head,
            read.body(),
            &f.tokens,
            store.then_some(&f.store),
        )
        .unwrap();
        let text = String::from_utf8(stream.output).unwrap();
        let body = text.split("\r\n\r\n").last().unwrap();
        (status, serde_json::from_str(body).unwrap(), text)
    }

    fn put(path: &str, token: Option<&str>, body: &[u8], extra: &str) -> Vec<u8> {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let mut request = format!(
            "PUT {path} HTTP/1.1\r\nHost: 192.168.1.2:4798\r\n{auth}Content-Type: audio/wav\r\nContent-Length: {}\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    #[test]
    fn a_take_is_stored_and_its_redo_replaces_it() {
        let f = fixture();
        let path = "/voice-samples/s1/near_miss/hey_luna-002.wav";
        let (status, body, text) = send(&f, put(path, Some(&f.token), &wav(2.0), ""), true);
        assert_eq!(status, 201);
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"), "{text}");
        assert_eq!(body["take"], "hey_luna-002");
        assert_eq!(body["phrase"], "hey_luna");
        assert_eq!(
            (body["kind"].as_str(), body["seconds"].as_f64()),
            (Some("near_miss"), Some(2.0))
        );
        let (status, body, _) = send(&f, put(path, Some(&f.token), &wav(1.0), ""), true);
        assert_eq!((status, body["created"].as_bool()), (200, Some(false)));
        assert_eq!(f.store.list().unwrap()[0].seconds, 1.0);
    }

    #[test]
    fn a_client_that_asks_to_continue_is_told_to() {
        let f = fixture();
        let request = put(
            "/voice-samples/s1/positive/001.wav",
            Some(&f.token),
            &wav(2.0),
            "Expect: 100-continue\r\n",
        );
        let (status, _, text) = send(&f, request, true);
        assert_eq!(status, 201);
        assert!(
            text.starts_with("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201"),
            "{text}"
        );
    }

    #[test]
    fn every_refusal_is_named() {
        let f = fixture();
        let take = "/voice-samples/s1/positive/001.wav";
        let code = |request: Vec<u8>, store: bool| {
            let (status, body, _) = send(&f, request, store);
            (status, body["error"].as_str().unwrap().to_string())
        };
        assert_eq!(
            code(put(take, None, &wav(2.0), ""), true),
            (401, "unauthorized".into())
        );
        assert_eq!(
            code(put(take, Some("nope"), &wav(2.0), ""), true),
            (401, "unauthorized".into())
        );
        assert_eq!(
            code(put(take, Some(&f.token), &wav(2.0), ""), false),
            (503, "voice_samples_unavailable".into())
        );
        assert_eq!(
            code(
                put(
                    "/voice-samples/../x/positive/1.wav",
                    Some(&f.token),
                    &wav(2.0),
                    ""
                ),
                true
            ),
            (400, "bad_name".into())
        );
        assert_eq!(
            code(put(take, Some(&f.token), b"RIFFnot a wav", ""), true),
            (400, "bad_wav".into())
        );
        let get = format!(
            "GET {take} HTTP/1.1\r\nAuthorization: Bearer {}\r\n\r\n",
            f.token
        )
        .into_bytes();
        assert_eq!(code(get, true), (405, "method_not_allowed".into()));
        let chunked = format!(
            "PUT {take} HTTP/1.1\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\n\r\n",
            f.token
        )
        .into_bytes();
        assert_eq!(code(chunked, true), (411, "length_required".into()));
        let no_length = format!(
            "PUT {take} HTTP/1.1\r\nAuthorization: Bearer {}\r\n\r\n",
            f.token
        )
        .into_bytes();
        assert_eq!(code(no_length, true), (411, "length_required".into()));
        let huge = format!(
            "PUT {take} HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
            f.token,
            MAX_TAKE_BYTES + 1
        )
        .into_bytes();
        assert_eq!(code(huge, true), (413, "too_large".into()));
        let _held = f
            .store
            .put(
                "s2",
                &crate::voice_samples::Take {
                    kind: crate::voice_samples::Kind::Positive,
                    number: 1,
                    phrase: None,
                },
                &wav(1.0),
            )
            .unwrap();
        let hold = f.store.hold(&["s2".into()]).unwrap();
        assert_eq!(
            code(
                put(
                    "/voice-samples/s2/positive/002.wav",
                    Some(&f.token),
                    &wav(2.0),
                    ""
                ),
                true
            ),
            (409, "set_in_training".into())
        );
        drop(hold);
    }

    #[test]
    fn heads_read_as_sent() {
        let head = parse_head(b"PUT /voice-samples/s1/positive/001.wav?x=1 HTTP/1.1\r\nAuthorization: bearer abc \r\nContent-Length: 5").unwrap();
        assert_eq!(
            (
                head.method.as_str(),
                head.path.as_str(),
                head.query.as_deref()
            ),
            ("PUT", "/voice-samples/s1/positive/001.wav", Some("x=1"))
        );
        assert_eq!(head.bearer(), Some("abc"));
        assert_eq!(head.header("content-length"), Some("5"));
        assert!(parse_head(b"PUT /x").is_err());
        assert!(parse_head(b"PUT /x SPDY/3").is_err());
        assert!(parse_head(b"PUT /x HTTP/1.1\r\nno colon").is_err());
        assert!(parse_head(&[0xff, 0xfe]).is_err());
        assert!(
            is_upload_path("/voice-samples/a")
                && is_upload_path("/voice-samples")
                && !is_upload_path("/transcribe")
        );
    }

    #[test]
    fn a_head_too_long_or_cut_short_is_an_error() {
        let mut long = Duplex {
            input: std::io::Cursor::new(vec![b'a'; MAX_HEAD_BYTES * 2]),
            output: vec![],
        };
        assert_eq!(
            read_head(&mut long).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        let mut short = Duplex {
            input: std::io::Cursor::new(b"PUT / HTTP/1.1\r\n".to_vec()),
            output: vec![],
        };
        assert_eq!(
            read_head(&mut short).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_prefixed_stream_replays_its_prefix_first() {
        let mut stream = Prefixed::new(
            b"abc".to_vec(),
            Duplex {
                input: std::io::Cursor::new(b"def".to_vec()),
                output: vec![],
            },
        );
        let mut all = String::new();
        stream.read_to_string(&mut all).unwrap();
        assert_eq!(all, "abcdef");
        stream.write_all(b"out").unwrap();
        stream.flush().unwrap();
        assert_eq!(stream.inner.output, b"out");
    }
}
