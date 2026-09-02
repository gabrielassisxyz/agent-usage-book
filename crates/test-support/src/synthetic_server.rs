//! The synthetic provider HTTP server (`aub-71j.3`), test-only.
//!
//! A loopback TCP server bound to an ephemeral port, programmable to produce
//! every response and failure shape the adapter contract covers. It exists so
//! end-to-end tests can exercise the real `aub` binary over a real socket,
//! including the shapes the in-process synthetic adapter cannot reach:
//! transport-level refusal, headers-then-stall, mid-body close, and the
//! credential-recording claim that proves two logical accounts each carry
//! their own credential to the wire.
//!
//! The server is `test-support`-resident and the crate is a `dev-dependency`
//! of the main package, so the release binary never links it. The manifest
//! check in `tests/test_support.rs` enforces that property.
//!
//! May not depend on:
//! - SQLite (rule `03`)
//! - the credential or configuration modules (rule `07`)
//! - the ureq transport driver (rule `12`, which confines it to the transport
//!   module; the server does not speak the transport's request shape, only the
//!   wire-level HTTP shape).
//!
//! Threading model: each accepted connection is handled on its own thread, so
//! a script with a slow `HeadersThenStall` does not stall the listener from
//! accepting the next request, so the test can dispatch several concurrent
//! requests and observe the budget actually bounding them. The thread count
//! is bounded by the number of script entries; no test in this repository
//! asks for more than a handful.

pub mod script;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use script::{ScriptedOutcome, ScriptedResponseBody};

/// A recorded inbound request the synthetic server received.
///
/// One per accepted connection. The server is HTTP/1.1 with keep-alive
/// disabled: every connection carries exactly one request, which is what the
/// `aub` binary's HTTP client (ureq, in `src/meter/transport.rs`) expects
/// for its batched credential loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// Returns the value of the `Authorization` header, if the request
    /// carried one. This is the credential the server actually saw on the
    /// wire, used by the named-account isolation test
    /// (`aub-71j.4`/`aub-eun.11`).
    pub fn authorization(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str())
    }
}

/// Errors the server can report back to the test, separate from the outcomes
/// the server programs the wire to produce.
#[derive(Debug)]
pub enum SyntheticServerError {
    /// The OS refused to bind the loopback socket. Carries the underlying
    /// io::Error for diagnostics.
    BindFailed(std::io::Error),
    /// The acceptor thread panicked before any request was served.
    AcceptorCrashed,
    /// The script ran out of entries; the server keeps the listener open but
    /// reports the exhaustion on every subsequent request.
    ScriptExhausted,
    /// An I/O error occurred on a single connection while writing the
    /// programmed response. Carries the underlying error.
    WriteFailed(std::io::Error),
    /// An I/O error occurred while reading the inbound request.
    ReadFailed(std::io::Error),
}

impl std::fmt::Display for SyntheticServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyntheticServerError::BindFailed(e) => write!(f, "bind failed: {e}"),
            SyntheticServerError::AcceptorCrashed => f.write_str("acceptor thread panicked"),
            SyntheticServerError::ScriptExhausted => f.write_str("script exhausted"),
            SyntheticServerError::WriteFailed(e) => write!(f, "response write failed: {e}"),
            SyntheticServerError::ReadFailed(e) => write!(f, "request read failed: {e}"),
        }
    }
}

impl std::error::Error for SyntheticServerError {}

/// The synthetic provider HTTP server.
///
/// Constructed via [`SyntheticServer::start`], which binds an ephemeral port
/// on the loopback interface, spawns the acceptor thread, and returns the
/// server. The URL to point a test client at is [`SyntheticServer::url`].
///
/// The server keeps accepting until [`SyntheticServer::stop`] is called, even
/// after the script runs out; a test that scripts fewer responses than
/// requests will get an [`SyntheticServerError::ScriptExhausted`] indicator on the
/// next request. The listener does not close on its own.
pub struct SyntheticServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    script: Arc<Mutex<Vec<ScriptedOutcome>>>,
    script_exhausted: Arc<Mutex<bool>>,
    shutdown: Arc<Mutex<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl SyntheticServer {
    /// Binds an ephemeral loopback port, spawns the acceptor thread, and
    /// returns a server ready to receive requests.
    ///
    /// The script is the programmed sequence of responses the server will
    /// produce. One entry per expected request, in order.
    pub fn start(script: Vec<ScriptedOutcome>) -> Result<Self, SyntheticServerError> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(SyntheticServerError::BindFailed)?;
        let address = listener
            .local_addr()
            .expect("local_addr on a bound socket is infallible");
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script));
        let script_exhausted = Arc::new(Mutex::new(false));
        let shutdown = Arc::new(Mutex::new(false));

        let thread = spawn_acceptor(
            listener,
            Arc::clone(&requests),
            Arc::clone(&script),
            Arc::clone(&script_exhausted),
            Arc::clone(&shutdown),
        );

        Ok(Self {
            address,
            requests,
            script,
            script_exhausted,
            shutdown,
            thread: Some(thread),
        })
    }

    /// The loopback URL of the form `http://127.0.0.1:<port>`. Tests point
    /// their client at this.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The TCP port the server bound to. Provided for tests that build the
    /// URL themselves.
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// Every request the server has received so far, in receive order. The
    /// caller reads this after issuing all the requests it planned and
    /// before asserting on the recorded credential.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .expect("requests mutex poisoned")
            .clone()
    }

    /// How many requests the server has received.
    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests mutex poisoned").len()
    }

    /// `true` if the script ran out of entries. A request received after
    /// exhaustion produces a default 500 response so the client does not hang;
    /// tests should script at least as many responses as they issue.
    pub fn script_exhausted(&self) -> bool {
        *self
            .script_exhausted
            .lock()
            .expect("script_exhausted mutex poisoned")
    }

    /// Appends one scripted outcome. Useful for tests that build the script
    /// incrementally as the request count grows.
    pub fn push_outcome(&self, outcome: ScriptedOutcome) {
        self.script
            .lock()
            .expect("script mutex poisoned")
            .push(outcome);
    }

    /// Closes the listener and joins the acceptor thread. Idempotent: a
    /// second call is a no-op. After this, [`Self::url`] still returns the
    /// bound address but the server will not accept any further connections.
    pub fn stop(&mut self) {
        *self.shutdown.lock().expect("shutdown mutex poisoned") = true;
        // Closing the listener from this thread unblocks `accept()` in the
        // acceptor thread, which then sees the shutdown flag and exits.
        // We open a throwaway connection so the listener socket is woken up
        // even on platforms where closing the listener from the owning thread
        // is racy.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(50));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SyntheticServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns the acceptor thread that owns the listener. Each accepted
/// connection is handled on its own short-lived thread.
fn spawn_acceptor(
    listener: TcpListener,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    script: Arc<Mutex<Vec<ScriptedOutcome>>>,
    script_exhausted: Arc<Mutex<bool>>,
    shutdown: Arc<Mutex<bool>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let listener = listener;
        loop {
            if *shutdown.lock().expect("shutdown mutex poisoned") {
                return;
            }

            let (stream, _peer) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) => {
                    // A non-fatal accept error: the listener is shutting down
                    // or the kernel briefly refused a connection. We loop and
                    // try again; the shutdown flag will catch the rest case.
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
            };

            let reqs = Arc::clone(&requests);
            let sc = Arc::clone(&script);
            let exhausted = Arc::clone(&script_exhausted);
            thread::spawn(move || {
                handle_connection(stream, reqs, sc, exhausted);
            });
        }
    })
}

/// Handles one inbound connection: read one HTTP request, write one
/// programmed response, then close the connection.
fn handle_connection(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    script: Arc<Mutex<Vec<ScriptedOutcome>>>,
    script_exhausted: Arc<Mutex<bool>>,
) {
    let request = match read_request(&mut stream) {
        Ok(req) => req,
        Err(_) => {
            // A truncated read or unsupported method. We cannot produce a
            // meaningful response: close and move on. The client will see a
            // connection-reset error and translate it through the transport's
            // vocabulary.
            return;
        }
    };
    requests
        .lock()
        .expect("requests mutex poisoned")
        .push(request.clone());

    let outcome = {
        let mut script = script.lock().expect("script mutex poisoned");
        if script.is_empty() {
            *script_exhausted
                .lock()
                .expect("script_exhausted mutex poisoned") = true;
            ScriptedOutcome::InternalServerError500
        } else {
            script.remove(0)
        }
    };

    if let Err(e) = write_outcome(&mut stream, &outcome) {
        // A write failure is the test client's read timeout firing before we
        // finished; nothing for the test to assert on.
        eprintln!("[synthetic_server] connection write returned error: {e}");
    }
}

/// Reads one HTTP/1.1 request from the stream. We support the method-URL
/// forms the `aub` client issues (`GET /path HTTP/1.1`, `POST /path HTTP/1.1`)
/// and the headers up to the blank line, then the body if `Content-Length`
/// was set. The body is read with a 250 ms ceiling so a client that hangs
/// after sending headers does not block this thread.
fn read_request(stream: &mut TcpStream) -> Result<RecordedRequest, SyntheticServerError> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    let read_start = Instant::now();
    // Read until we have the end-of-headers marker or we time out.
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if read_start.elapsed() > Duration::from_millis(250) {
            return Err(SyntheticServerError::ReadFailed(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request header read timed out",
            )));
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(SyntheticServerError::ReadFailed(e)),
        }
    }

    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            SyntheticServerError::ReadFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no end-of-headers marker",
            ))
        })?;
    let header_text = std::str::from_utf8(&buf[..header_end]).map_err(|e| {
        SyntheticServerError::ReadFailed(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })?;
    let (start_line, rest_headers) = header_text.split_once("\r\n").ok_or_else(|| {
        SyntheticServerError::ReadFailed(std::io::Error::other("no request line"))
    })?;
    let mut header_iter = rest_headers.split("\r\n");
    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    for line in header_iter.by_ref() {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim().to_string();
            if key.eq_ignore_ascii_case("content-length") {
                if let Ok(n) = value.parse::<usize>() {
                    content_length = n;
                }
            }
            headers.push((key, value));
        }
    }

    let mut parts = start_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| SyntheticServerError::ReadFailed(std::io::Error::other("missing method")))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| SyntheticServerError::ReadFailed(std::io::Error::other("missing path")))?
        .to_string();
    // We do not validate the HTTP version; clients the production binary
    // produces use 1.1.

    let mut body = Vec::with_capacity(content_length);
    let already_in_buf = buf.len() - (header_end + 4);
    if already_in_buf > 0 {
        body.extend_from_slice(&buf[header_end + 4..]);
    }
    while body.len() < content_length {
        if read_start.elapsed() > Duration::from_millis(250) {
            return Err(SyntheticServerError::ReadFailed(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request body read timed out",
            )));
        }
        let need = content_length - body.len();
        let cap = chunk.len().min(need);
        let n = stream
            .read(&mut chunk[..cap])
            .map_err(SyntheticServerError::ReadFailed)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    Ok(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Writes the programmed outcome to the stream. Every variant produces a
/// single best-effort write; failure closes the socket with the partial
/// buffer flushed.
fn write_outcome(
    stream: &mut TcpStream,
    outcome: &ScriptedOutcome,
) -> Result<(), SyntheticServerError> {
    match outcome {
        ScriptedOutcome::Success(body) => write_response(stream, body),
        ScriptedOutcome::Response {
            status,
            headers,
            body,
        } => write_response(
            stream,
            &ScriptedResponseBody {
                status: *status,
                headers: headers.clone(),
                body: body.clone(),
            },
        ),
        ScriptedOutcome::Unauthorized401 => {
            write_response(stream, &ScriptedResponseBody::with_status(401, Vec::new()))
        }
        ScriptedOutcome::Forbidden403 => {
            write_response(stream, &ScriptedResponseBody::with_status(403, Vec::new()))
        }
        ScriptedOutcome::TooManyRequests429 {
            retry_after_seconds,
        } => {
            let mut body = ScriptedResponseBody::with_status(429, Vec::new());
            if let Some(secs) = retry_after_seconds {
                body.headers
                    .push(("Retry-After".to_string(), secs.to_string()));
            }
            write_response(stream, &body)
        }
        ScriptedOutcome::InternalServerError500 => {
            write_response(stream, &ScriptedResponseBody::with_status(500, Vec::new()))
        }
        ScriptedOutcome::MalformedJson {
            status,
            headers,
            body,
        } => write_response(
            stream,
            &ScriptedResponseBody {
                status: *status,
                headers: headers.clone(),
                body: body.clone(),
            },
        ),
        ScriptedOutcome::MissingRequiredField {
            status,
            headers,
            body,
        } => write_response(
            stream,
            &ScriptedResponseBody {
                status: *status,
                headers: headers.clone(),
                body: body.clone(),
            },
        ),
        ScriptedOutcome::UnknownAdditionalField {
            status,
            headers,
            body,
        } => write_response(
            stream,
            &ScriptedResponseBody {
                status: *status,
                headers: headers.clone(),
                body: body.clone(),
            },
        ),
        ScriptedOutcome::StaleServerTimestamp {
            status,
            headers,
            body,
        } => write_response(
            stream,
            &ScriptedResponseBody {
                status: *status,
                headers: headers.clone(),
                body: body.clone(),
            },
        ),
        ScriptedOutcome::HeadersThenStall { status, headers } => {
            // Write the status line and headers, then sleep long enough for
            // the client to give up reading. The sleep is bounded so a test
            // does not hang the test runner indefinitely, and it must exceed
            // the longest client deadline any test drives against this shape
            // (the contract suite's 15s total): a stall shorter than the
            // client's deadline races it, and when the server's close wins
            // the client sees a clean EOF and an empty body instead of the
            // timeout the case is pinning (aub-deadline-body-read-timeout-u3db).
            let head = format!(
                "HTTP/1.1 {} {}\r\n{}\r\n",
                status,
                status_text(*status),
                headers_to_header_block(headers)
            );
            stream
                .write_all(head.as_bytes())
                .map_err(SyntheticServerError::WriteFailed)?;
            stream.flush().map_err(SyntheticServerError::WriteFailed)?;
            thread::sleep(Duration::from_secs(20));
            Ok(())
        }
        ScriptedOutcome::AcceptThenNeverRespond => {
            // Sleep without writing anything. The client's connect completes
            // because the TCP handshake already happened on accept, but its
            // read times out because we never send a byte.
            thread::sleep(Duration::from_secs(20));
            Ok(())
        }
        ScriptedOutcome::CloseMidBody {
            status,
            headers,
            partial_body,
        } => {
            let head = format!(
                "HTTP/1.1 {} {}\r\n{}\r\n",
                status,
                status_text(*status),
                headers_to_header_block(headers)
            );
            stream
                .write_all(head.as_bytes())
                .map_err(SyntheticServerError::WriteFailed)?;
            stream
                .write_all(partial_body)
                .map_err(SyntheticServerError::WriteFailed)?;
            stream.flush().map_err(SyntheticServerError::WriteFailed)?;
            // Drop the stream to close the socket. The OS will reset the
            // connection from the client's perspective.
            Ok(())
        }
    }
}

fn write_response(
    stream: &mut TcpStream,
    response: &ScriptedResponseBody,
) -> Result<(), SyntheticServerError> {
    let head = format!(
        "HTTP/1.1 {} {}\r\n{}\r\n",
        response.status,
        status_text(response.status),
        headers_to_header_block(&response.headers)
    );
    stream
        .write_all(head.as_bytes())
        .map_err(SyntheticServerError::WriteFailed)?;
    stream
        .write_all(&response.body)
        .map_err(SyntheticServerError::WriteFailed)?;
    stream.flush().map_err(SyntheticServerError::WriteFailed)?;
    Ok(())
}

fn headers_to_header_block(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    out
}

/// The shared body of a status response, used by [`write_response`].
fn status_text(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn fetch(port: u16, path: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-token\r\n\r\n"
        );
        use std::io::Write;
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let status_line = text.lines().next().unwrap_or("").to_string();
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, text)
    }

    #[test]
    fn binds_ephemeral_loopback_port_and_reports_url() {
        let server = SyntheticServer::start(vec![]).unwrap();
        let url = server.url();
        assert!(url.starts_with("http://127.0.0.1:"), "url was {url}");
        assert!(server.port() > 0);
    }

    #[test]
    fn success_outcome_returns_status_and_body() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
            ScriptedResponseBody::json_ok(b"{\"hello\":1}".to_vec()),
        )])
        .unwrap();
        let (status, text) = fetch(server.port(), "/usage");
        assert_eq!(status, 200);
        assert!(text.contains("{\"hello\":1}"));
        assert_eq!(server.request_count(), 1);
        assert_eq!(
            server.requests()[0].authorization(),
            Some("Bearer test-token")
        );
    }

    #[test]
    fn unauthorized_outcome_sends_401() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
        let (status, _) = fetch(server.port(), "/usage");
        assert_eq!(status, 401);
    }

    #[test]
    fn ambiguous_403_outcome_sends_403() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::Forbidden403]).unwrap();
        let (status, _) = fetch(server.port(), "/usage");
        assert_eq!(status, 403);
    }

    #[test]
    fn too_many_requests_carries_retry_after() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::TooManyRequests429 {
            retry_after_seconds: Some(30),
        }])
        .unwrap();
        let (_, text) = fetch(server.port(), "/usage");
        assert!(text.contains("Retry-After: 30"));
    }

    #[test]
    fn malformed_body_outcome_sends_non_json_bytes() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::MalformedJson {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: b"<<not json>>".to_vec(),
        }])
        .unwrap();
        let (_, text) = fetch(server.port(), "/usage");
        assert!(text.contains("<<not json>>"));
    }

    #[test]
    fn close_mid_body_writes_partial_then_drops() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::CloseMidBody {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            partial_body: b"{\"incomp".to_vec(),
        }])
        .unwrap();
        let (_, text) = fetch(server.port(), "/usage");
        assert!(text.contains("{\"incomp"));
    }

    #[test]
    fn accepts_multiple_requests_in_script_order() {
        let server = SyntheticServer::start(vec![
            ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{\"i\":0}".to_vec())),
            ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{\"i\":1}".to_vec())),
            ScriptedOutcome::Success(ScriptedResponseBody::json_ok(b"{\"i\":2}".to_vec())),
        ])
        .unwrap();
        for _ in 0..3 {
            let _ = fetch(server.port(), "/usage");
        }
        assert_eq!(server.request_count(), 3);
    }

    #[test]
    fn script_exhaustion_reports_a_default_500() {
        let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();
        let _ = fetch(server.port(), "/a"); // exhausts the one-outcome script
        let (status, _) = fetch(server.port(), "/b");
        assert!(server.script_exhausted());
        assert_eq!(status, 500);
    }

    #[test]
    fn status_text_returns_known_reason_phrases() {
        assert_eq!(status_text(200), "OK");
        assert_eq!(status_text(401), "Unauthorized");
        assert_eq!(status_text(404), "Not Found");
        assert_eq!(status_text(429), "Too Many Requests");
        assert_eq!(status_text(500), "Internal Server Error");
        // An unknown status falls back to OK rather than crashing; the
        // response still parses because the body and headers are unaffected.
        assert_eq!(status_text(799), "OK");
    }
}
