//! Blocking HTTP transport wrapper with monotonic timeouts and command-wide elapsed-time budget.
//!
//! May not depend on:
//! - SQLite
//! - presentation
//! - provider semantics (adapter logic lives in provider adapters)
//! - calibration
//!
//! Every HTTP request is executed with monotonic timeouts (connect, read, total).
//! A command-wide budget clips all deadlines so that no thread can block indefinitely.
//! On budget expiry, all unfinished requests return [`FailureClass::TotalBudgetExpired`].

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use crate::domain::failure::{FailureClass, HttpStatusClass};
use crate::domain::time::{Clock, MonotonicDuration, MonotonicInstant};
use crate::meter::adapter::HttpTransport;

/// Documented, tested shutdown tolerance for scoped-thread joins after budget expiry.
pub const SHUTDOWN_TOLERANCE: MonotonicDuration = MonotonicDuration::from_millis(250);

/// HTTP methods supported by the transport layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// Request-level timeout configuration using monotonic durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTimeoutConfig {
    pub connect_timeout: MonotonicDuration,
    pub read_timeout: MonotonicDuration,
    pub total_timeout: Option<MonotonicDuration>,
}

impl RequestTimeoutConfig {
    pub const fn new(
        connect_timeout: MonotonicDuration,
        read_timeout: MonotonicDuration,
        total_timeout: Option<MonotonicDuration>,
    ) -> Self {
        Self {
            connect_timeout,
            read_timeout,
            total_timeout,
        }
    }

    /// Clips the request timeouts so none exceeds the remaining command budget.
    pub fn clip_to_budget(&self, remaining_budget: MonotonicDuration) -> Self {
        let connect_timeout = MonotonicDuration::from_nanos(
            self.connect_timeout
                .as_nanos()
                .min(remaining_budget.as_nanos()),
        );
        let read_timeout = MonotonicDuration::from_nanos(
            self.read_timeout
                .as_nanos()
                .min(remaining_budget.as_nanos()),
        );
        let total_timeout = match self.total_timeout {
            Some(total) => Some(MonotonicDuration::from_nanos(
                total.as_nanos().min(remaining_budget.as_nanos()),
            )),
            None => Some(remaining_budget),
        };
        Self {
            connect_timeout,
            read_timeout,
            total_timeout,
        }
    }
}

/// The local-file source variant on the transport request (`aub-cg6k`).
///
/// A file-backed provider meter has no HTTP request to make, but its adapter
/// still owns no filesystem: its bytes cross the same [`HttpTransport`] port
/// an HTTP request takes, so evidence capture and the test transport seam
/// keep working unchanged. The real transport serves a local-file request
/// from disk; the synthetic transport in tests serves a fixture. On a
/// request carrying `local_file`, `url` and `method` are inert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    /// The file to read; or, when [`LocalFile::newest_glob`] is set, the
    /// directory whose matching files are searched.
    pub path: PathBuf,
    /// When set: read the newest file matching this glob under `path`,
    /// recursively, by modification time, instead of `path` itself. The glob
    /// matches file names with `*` and `?`, never directory names. This is
    /// the one generic file-source primitive the transport owns; which
    /// pattern and which directory a provider needs stays with its adapter.
    pub newest_glob: Option<String>,
}

impl LocalFile {
    /// Reads exactly the named file.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            newest_glob: None,
        }
    }

    /// Reads the newest file matching `pattern` under `path`, recursively,
    /// by modification time. Ties on modification time resolve to the
    /// lexicographically greatest path, so the selection is deterministic.
    pub fn with_newest_glob(path: impl Into<PathBuf>, pattern: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            newest_glob: Some(pattern.into()),
        }
    }
}

/// Response headers through which the local-file arm reports which file it
/// served and when the provider last wrote it. They ride the ordinary header
/// list because [`HttpResponse`] is shared with the HTTP arms; the evidence
/// capsule deliberately excludes headers, so an adapter that needs these two
/// facts inside its capsule copies them into the body it captures.
pub const LOCAL_FILE_PATH_HEADER: &str = "x-aub-local-file-path";
pub const LOCAL_FILE_MTIME_HEADER: &str = "x-aub-local-file-mtime-nanos";

/// An outgoing HTTP request definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeouts: RequestTimeoutConfig,
    /// The local file this request reads instead of the URL (`aub-cg6k`).
    /// `None` on every request the HTTP constructors build; `Some` makes
    /// `url` and `method` inert and the local-file arm serves the bytes.
    pub local_file: Option<LocalFile>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>, timeouts: RequestTimeoutConfig) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            timeouts,
            local_file: None,
        }
    }

    pub fn post(url: impl Into<String>, body: Vec<u8>, timeouts: RequestTimeoutConfig) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: Some(body),
            timeouts,
            local_file: None,
        }
    }

    /// A request whose bytes the real transport reads from disk and a
    /// synthetic transport serves from a fixture, through the same port an
    /// HTTP request takes (`aub-cg6k`).
    pub fn local_file(path: impl Into<PathBuf>, timeouts: RequestTimeoutConfig) -> Self {
        Self {
            url: String::new(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            timeouts,
            local_file: Some(LocalFile::new(path)),
        }
    }

    /// The newest-matching form of the local-file read: the transport
    /// resolves the newest file matching `pattern` under `path`, recursively,
    /// by modification time, and serves that file's bytes (`aub-cg6k`).
    pub fn newest_local_file(
        path: impl Into<PathBuf>,
        pattern: impl Into<String>,
        timeouts: RequestTimeoutConfig,
    ) -> Self {
        Self {
            url: String::new(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            timeouts,
            local_file: Some(LocalFile::with_newest_glob(path, pattern)),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// An incoming HTTP response received by the transport layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn body_as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    pub fn http_status_class(&self) -> Option<HttpStatusClass> {
        match self.status {
            400..=499 => Some(HttpStatusClass::ClientError),
            500..=599 => Some(HttpStatusClass::ServerError),
            _ => None,
        }
    }
}

/// Command-wide elapsed-time budget measured with a monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct CommandBudget {
    budget: MonotonicDuration,
    started_at: MonotonicInstant,
}

impl CommandBudget {
    pub fn new(budget: MonotonicDuration, clock: &impl Clock) -> Self {
        Self {
            budget,
            started_at: clock.monotonic_now(),
        }
    }

    pub fn remaining(&self, clock: &impl Clock) -> Option<MonotonicDuration> {
        let elapsed = clock.monotonic_now().duration_since(self.started_at);
        if elapsed.as_nanos() >= self.budget.as_nanos() {
            None
        } else {
            Some(MonotonicDuration::from_nanos(
                self.budget.as_nanos() - elapsed.as_nanos(),
            ))
        }
    }

    pub fn is_expired(&self, clock: &impl Clock) -> bool {
        self.remaining(clock).is_none()
    }
}

/// A correlated request pairing a caller-supplied key with an HTTP request.
#[derive(Debug, Clone)]
pub struct CorrelatedRequest<K> {
    pub key: K,
    pub request: HttpRequest,
}

impl<K> CorrelatedRequest<K> {
    pub fn new(key: K, request: HttpRequest) -> Self {
        Self { key, request }
    }
}

/// A correlated response pairing the caller-supplied key with the transport result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelatedResponse<K> {
    pub key: K,
    pub result: Result<HttpResponse, FailureClass>,
}

/// The production HTTP transport port: every request goes through
/// [`execute_single`], the one place the blocking driver is referenced (rule
/// `12`). The caller hands it a [`CommandBudget`], so the same request shape
/// serves both the single-request path and the sampler's batch workers: the
/// budget the caller passes is the one whose expiry clips every timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockingTransport;

impl HttpTransport for BlockingTransport {
    fn send(
        &self,
        request: &HttpRequest,
        budget: &CommandBudget,
        clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        if let Some(local) = &request.local_file {
            return crate::local_source::serve_local_file(local, budget, clock);
        }
        execute_single(request, budget, clock)
    }
}

/// Executes a single HTTP request respecting the command-wide budget.
pub fn execute_single(
    request: &HttpRequest,
    budget: &CommandBudget,
    clock: &impl Clock,
) -> Result<HttpResponse, FailureClass> {
    execute_single_with_resolver(request, budget, clock, None)
}

/// A hostname resolver in std types, so no ureq type leaves this module.
type HostResolver = fn(&str) -> std::io::Result<Vec<std::net::SocketAddr>>;

/// Executes a single HTTP request, resolving hostnames through `resolver`
/// instead of the system resolver when one is given.
///
/// `None` resolves through the system resolver: the production path, and the
/// only path production uses. Tests pass a resolver that fails (or answers)
/// deterministically, so a test asserting a DNS mapping never races the
/// machine's real resolver against the command budget (aub-1ijb).
fn execute_single_with_resolver(
    request: &HttpRequest,
    budget: &CommandBudget,
    clock: &impl Clock,
    resolver: Option<HostResolver>,
) -> Result<HttpResponse, FailureClass> {
    let Some(remaining) = budget.remaining(clock) else {
        return Err(FailureClass::TotalBudgetExpired);
    };

    let effective_timeouts = request.timeouts.clip_to_budget(remaining);
    let connect_dur = Duration::from_nanos(effective_timeouts.connect_timeout.as_nanos());
    let read_dur = Duration::from_nanos(effective_timeouts.read_timeout.as_nanos());

    let mut agent_builder = ureq::AgentBuilder::new()
        .timeout_connect(connect_dur)
        .timeout_read(read_dur);

    if let Some(resolve) = resolver {
        agent_builder = agent_builder.resolver(resolve);
    }

    // ureq derives the socket read timeout from the overall deadline whenever one is
    // set, discarding timeout_read entirely - so a budget-derived total would silently
    // disable the per-read timeout. Only a total the request itself declared becomes
    // ureq's deadline; the budget is enforced by the is_expired checks around the call,
    // and connect/read are already clipped to the remaining budget above.
    if request.timeouts.total_timeout.is_some()
        && let Some(total) = effective_timeouts.total_timeout
    {
        agent_builder = agent_builder.timeout(Duration::from_nanos(total.as_nanos()));
    }

    let agent: ureq::Agent = agent_builder.build();

    let method_str = match request.method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
    };

    let mut req = agent.request(method_str, &request.url);
    for (k, v) in &request.headers {
        req = req.set(k, v);
    }

    let ureq_response_res = match &request.body {
        Some(bytes) => req.send_bytes(bytes),
        None => req.call(),
    };

    if budget.is_expired(clock) {
        return Err(FailureClass::TotalBudgetExpired);
    }

    match ureq_response_res {
        Ok(res) => to_http_response(res, budget, clock),
        Err(ureq::Error::Status(_status, res)) => to_http_response(res, budget, clock),
        Err(ureq::Error::Transport(err)) => {
            if budget.is_expired(clock)
                || (effective_timeouts.read_timeout < request.timeouts.read_timeout
                    && (err.kind() == ureq::ErrorKind::Io
                        || err.kind() == ureq::ErrorKind::ConnectionFailed)
                    && (err.to_string().to_lowercase().contains("timeout")
                        || err.to_string().to_lowercase().contains("timed out")))
            {
                Err(FailureClass::TotalBudgetExpired)
            } else {
                Err(map_transport_error(&err))
            }
        }
    }
}

fn to_http_response(
    res: ureq::Response,
    budget: &CommandBudget,
    clock: &impl Clock,
) -> Result<HttpResponse, FailureClass> {
    let status = res.status();
    let mut headers = Vec::new();
    for name in res.headers_names() {
        if let Some(val) = res.header(&name) {
            headers.push((name, val.to_string()));
        }
    }
    let mut reader = res.into_reader();
    let mut body = Vec::new();
    if let Err(e) = reader.read_to_end(&mut body) {
        if budget.is_expired(clock) {
            return Err(FailureClass::TotalBudgetExpired);
        }
        if is_timeout_flavored_io_error(&e) {
            return Err(FailureClass::ReadTimeout);
        }
        return Err(FailureClass::MalformedBody);
    }

    if budget.is_expired(clock) {
        return Err(FailureClass::TotalBudgetExpired);
    }

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// True when an io error from the body reader is a timeout in disguise.
///
/// The kind check alone misses ureq's overall-deadline expiry: when a request
/// declares a total timeout, ureq discards the per-read timeout and derives
/// the socket deadline from the total, and the deadline's expiry surfaces
/// through the body reader with a non-timeout kind and a deadline-flavoured
/// message. The message check mirrors `map_transport_error`'s convention so
/// the two classifier paths agree on what a timeout is, and a provider hang is
/// never persisted as a malformed body (PLAN.md 14.5: timeouts are retried,
/// malformed responses are not).
fn is_timeout_flavored_io_error(e: &std::io::Error) -> bool {
    let message = e.to_string().to_lowercase();
    e.kind() == std::io::ErrorKind::TimedOut
        || e.kind() == std::io::ErrorKind::WouldBlock
        || message.contains("timeout")
        || message.contains("timed out")
        || message.contains("deadline")
}

fn map_transport_error(err: &ureq::Transport) -> FailureClass {
    let msg = err.to_string().to_lowercase();
    match err.kind() {
        ureq::ErrorKind::Dns => FailureClass::DnsFailure,
        // Refused and timed-out connects share a class on purpose: FailureClass has no
        // refused variant, and callers only distinguish connect-phase from read-phase.
        ureq::ErrorKind::ConnectionFailed => FailureClass::ConnectTimeout,
        ureq::ErrorKind::Io => {
            if msg.contains("connect") && (msg.contains("timeout") || msg.contains("timed out")) {
                FailureClass::ConnectTimeout
            } else if msg.contains("timeout")
                || msg.contains("timed out")
                || msg.contains("deadline")
            {
                FailureClass::ReadTimeout
            } else {
                FailureClass::ConnectTimeout
            }
        }
        // Enumerated rather than left to a wildcard: ureq::ErrorKind is not
        // #[non_exhaustive], so naming every variant makes a future ureq
        // release fail to compile here instead of silently classifying a new
        // failure mode as a connect timeout.
        ureq::ErrorKind::InvalidUrl
        | ureq::ErrorKind::UnknownScheme
        | ureq::ErrorKind::InsecureRequestHttpsOnly
        | ureq::ErrorKind::TooManyRedirects
        | ureq::ErrorKind::BadStatus
        | ureq::ErrorKind::BadHeader
        | ureq::ErrorKind::InvalidProxyUrl
        | ureq::ErrorKind::ProxyConnect
        | ureq::ErrorKind::ProxyUnauthorized
        | ureq::ErrorKind::HTTP => {
            if msg.contains("timeout") || msg.contains("timed out") {
                FailureClass::ReadTimeout
            } else {
                FailureClass::ConnectTimeout
            }
        }
    }
}

/// Executes a collection of correlated requests concurrently in scoped threads,
/// respecting the command-wide budget.
///
/// If the budget expires before or during execution, any unfinished request returns
/// `Err(FailureClass::TotalBudgetExpired)`, preserving key and ordinal correlation.
pub fn execute_batch<K: Send + Clone>(
    requests: Vec<CorrelatedRequest<K>>,
    budget: &CommandBudget,
    clock: &(impl Clock + Sync),
) -> Vec<CorrelatedResponse<K>> {
    if requests.is_empty() {
        return Vec::new();
    }

    if budget.is_expired(clock) {
        return requests
            .into_iter()
            .map(|req| CorrelatedResponse {
                key: req.key,
                result: Err(FailureClass::TotalBudgetExpired),
            })
            .collect();
    }

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(requests.len());

        for req in requests {
            let handle = s.spawn(move || {
                let res = execute_single(&req.request, budget, clock);
                CorrelatedResponse {
                    key: req.key,
                    result: res,
                }
            });
            handles.push(handle);
        }

        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| panic!("transport worker thread panicked"))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, RealClock, UtcTimestamp};
    use std::io::Write;
    use std::net::TcpListener;

    fn timeouts(connect_ms: u64, read_ms: u64, total_ms: Option<u64>) -> RequestTimeoutConfig {
        RequestTimeoutConfig::new(
            MonotonicDuration::from_millis(connect_ms),
            MonotonicDuration::from_millis(read_ms),
            total_ms.map(MonotonicDuration::from_millis),
        )
    }

    /// A resolver that fails every hostname without touching the network.
    /// Returning an error (rather than an empty address list) exercises the
    /// same `ErrorKind::Dns` path a real NXDOMAIN takes through ureq.
    fn failing_dns_resolver(_netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "test resolver refuses every hostname",
        ))
    }

    #[test]
    fn slow_headers_and_connection_refused_produce_distinct_failure_classes() {
        let clock = RealClock::new();

        // 1. Slow headers: accepts connection and sleeps
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_millis(300));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });

        let slow_req = HttpRequest::get(
            format!("http://127.0.0.1:{port}"),
            timeouts(50, 50, Some(50)),
        );
        let budget_slow = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let slow_result = execute_single(&slow_req, &budget_slow, &clock).unwrap_err();

        // 2. Connection refused: port where nothing listens
        let unused_port = {
            let temp = TcpListener::bind("127.0.0.1:0").unwrap();
            temp.local_addr().unwrap().port()
        };
        let refused_req = HttpRequest::get(
            format!("http://127.0.0.1:{unused_port}"),
            timeouts(50, 50, Some(50)),
        );
        let budget_refused = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let refused_result = execute_single(&refused_req, &budget_refused, &clock).unwrap_err();

        assert_ne!(
            slow_result, refused_result,
            "slow headers and connection refused must produce distinct failure classes: {slow_result:?} vs {refused_result:?}"
        );
    }

    #[test]
    fn wedged_endpoint_returns_total_budget_expired_within_budget() {
        let clock = RealClock::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server accepts connection and never responds
        std::thread::spawn(move || {
            while let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_millis(500));
            }
        });

        let req1 = CorrelatedRequest::new(
            "account-a",
            HttpRequest::get(
                format!("http://127.0.0.1:{port}/a"),
                timeouts(5000, 5000, Some(5000)),
            ),
        );
        let req2 = CorrelatedRequest::new(
            "account-b",
            HttpRequest::get(
                format!("http://127.0.0.1:{port}/b"),
                timeouts(5000, 5000, Some(5000)),
            ),
        );

        let budget_dur = MonotonicDuration::from_millis(80);
        let budget = CommandBudget::new(budget_dur, &clock);

        let start = clock.monotonic_now();
        let responses = execute_batch(vec![req1, req2], &budget, &clock);
        let elapsed = clock.monotonic_now().duration_since(start);

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].key, "account-a");
        assert_eq!(responses[1].key, "account-b");

        for resp in responses {
            assert_eq!(
                resp.result,
                Err(FailureClass::TotalBudgetExpired),
                "expected TotalBudgetExpired for wedged endpoint, got {:?}",
                resp.result
            );
        }

        // Must complete within budget plus shutdown tolerance
        let max_allowed = budget_dur.as_nanos() + SHUTDOWN_TOLERANCE.as_nanos();
        assert!(
            elapsed.as_nanos() <= max_allowed,
            "elapsed {elapsed:?} exceeded budget + tolerance {max_allowed}ns"
        );
    }

    #[test]
    fn connection_refused_unreachable_and_dns_failure_map_to_transport_variants() {
        let clock = RealClock::new();

        // 1. Connection refused: port where nothing listens -> ConnectTimeout (transport, not HttpStatus)
        let unused_port = {
            let temp = TcpListener::bind("127.0.0.1:0").unwrap();
            temp.local_addr().unwrap().port()
        };
        let refused_req = HttpRequest::get(
            format!("http://127.0.0.1:{unused_port}"),
            timeouts(50, 50, Some(50)),
        );
        let budget = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let refused_res = execute_single(&refused_req, &budget, &clock);
        assert_eq!(
            refused_res,
            Err(FailureClass::ConnectTimeout),
            "connection refused must map to transport FailureClass::ConnectTimeout"
        );

        // 2. DNS failure -> DnsFailure. Deterministic: the resolver below
        // fails every hostname without touching the network, so the outcome
        // cannot depend on the machine's resolver latency; the budget is
        // generous for the same reason, so only the resolver can fail this.
        let dns_req = HttpRequest::get(
            "http://nonexistent.invalid.domain.for.transport.test:80",
            timeouts(50, 50, Some(50)),
        );
        let dns_budget = CommandBudget::new(MonotonicDuration::from_millis(5000), &clock);
        let dns_res =
            execute_single_with_resolver(&dns_req, &dns_budget, &clock, Some(failing_dns_resolver));
        assert_eq!(
            dns_res,
            Err(FailureClass::DnsFailure),
            "dns resolution failure must map to FailureClass::DnsFailure"
        );

        // 3. Planted negative: real 4xx response from server -> HttpStatus(ClientError)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let client_err_req = HttpRequest::get(
            format!("http://127.0.0.1:{port}"),
            timeouts(200, 200, Some(200)),
        );
        let client_err_res = execute_single(&client_err_req, &budget, &clock).unwrap();
        assert_eq!(
            client_err_res.http_status_class(),
            Some(HttpStatusClass::ClientError),
            "real 4xx HTTP response must produce HttpStatusClass::ClientError"
        );
    }

    /// Each timeout fires rather than waiting for a wedged server. The elapsed
    /// bounds are deliberately loose on top: the lower bound proves the call
    /// waited for the timeout instead of returning instantly, and the upper
    /// bound proves it returned long before the wedged server acted. A tight
    /// upper bound around the configured value flakes under load, because a
    /// thread that is ready at 60ms may not be scheduled until later, and no
    /// assertion can tell that scheduling delay apart from a late timeout
    /// (aub-1ijb). The servers therefore sleep far beyond the upper bound, so
    /// an implementation that ignored the timeout would still be caught.
    #[test]
    fn each_timeout_fires_before_a_wedged_server_responds() {
        let clock = RealClock::new();

        // 1. Read timeout: server accepts connection and never sends data
        let listener_read = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_read = listener_read.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener_read.accept() {
                std::thread::sleep(Duration::from_millis(2000));
            }
        });

        let read_req = HttpRequest::get(
            format!("http://127.0.0.1:{port_read}"),
            timeouts(5000, 60, None),
        );
        let budget_large = CommandBudget::new(MonotonicDuration::from_millis(5000), &clock);
        let start_read = clock.monotonic_now();
        let read_res = execute_single(&read_req, &budget_large, &clock);
        let elapsed_read = clock.monotonic_now().duration_since(start_read);

        assert_eq!(
            read_res,
            Err(FailureClass::ReadTimeout),
            "read timeout must produce FailureClass::ReadTimeout"
        );
        let min_read = 40_000_000u128; // 40ms
        let max_read = 1_000_000_000u128; // 1s, far below the wedged server's 2s sleep
        assert!(
            u128::from(elapsed_read.as_nanos()) >= min_read
                && u128::from(elapsed_read.as_nanos()) <= max_read,
            "read timeout elapsed {elapsed_read:?} out of expected range {min_read}..={max_read}ns"
        );

        // 2. Total command budget timeout: server accepts connection and sleeps
        let listener_budget = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_budget = listener_budget.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener_budget.accept() {
                std::thread::sleep(Duration::from_millis(2000));
            }
        });

        let budget_req = HttpRequest::get(
            format!("http://127.0.0.1:{port_budget}"),
            timeouts(5000, 5000, None),
        );
        let budget_short = CommandBudget::new(MonotonicDuration::from_millis(60), &clock);
        let start_budget = clock.monotonic_now();
        let budget_res = execute_single(&budget_req, &budget_short, &clock);
        let elapsed_budget = clock.monotonic_now().duration_since(start_budget);

        assert_eq!(
            budget_res,
            Err(FailureClass::TotalBudgetExpired),
            "expired command budget must produce FailureClass::TotalBudgetExpired"
        );
        let min_budget = 40_000_000u128; // 40ms
        let max_budget = 1_000_000_000u128; // 1s, far below the wedged server's 2s sleep
        assert!(
            u128::from(elapsed_budget.as_nanos()) >= min_budget
                && u128::from(elapsed_budget.as_nanos()) <= max_budget,
            "budget timeout elapsed {elapsed_budget:?} out of expected range {min_budget}..={max_budget}ns"
        );
    }

    #[test]
    fn total_budget_expiry_returns_typed_correlated_outcomes() {
        let mut clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
        let budget = CommandBudget::new(MonotonicDuration::from_millis(100), &clock);

        // Advance fake clock past budget
        clock.advance(MonotonicDuration::from_millis(150));

        let reqs = vec![
            CorrelatedRequest::new(
                "acc-1",
                HttpRequest::get("http://localhost:1/1", timeouts(10, 10, None)),
            ),
            CorrelatedRequest::new(
                "acc-2",
                HttpRequest::get("http://localhost:1/2", timeouts(10, 10, None)),
            ),
            CorrelatedRequest::new(
                "acc-3",
                HttpRequest::get("http://localhost:1/3", timeouts(10, 10, None)),
            ),
        ];

        let results = execute_batch(reqs, &budget, &clock);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].key, "acc-1");
        assert_eq!(results[0].result, Err(FailureClass::TotalBudgetExpired));
        assert_eq!(results[1].key, "acc-2");
        assert_eq!(results[1].result, Err(FailureClass::TotalBudgetExpired));
        assert_eq!(results[2].key, "acc-3");
        assert_eq!(results[2].result, Err(FailureClass::TotalBudgetExpired));
    }

    #[test]
    fn timeouts_are_clipped_to_remaining_budget() {
        let req_timeouts = timeouts(5000, 3000, Some(10000));
        let clipped = req_timeouts.clip_to_budget(MonotonicDuration::from_millis(500));

        assert_eq!(clipped.connect_timeout, MonotonicDuration::from_millis(500));
        assert_eq!(clipped.read_timeout, MonotonicDuration::from_millis(500));
        assert_eq!(
            clipped.total_timeout,
            Some(MonotonicDuration::from_millis(500))
        );
    }

    /// The production port drives the same executor the free function is: a
    /// request through [`BlockingTransport`] against a port nothing listens
    /// on comes back classified, proving the impl is wired to the real
    /// driver and not to silence.
    #[test]
    fn the_blocking_transport_port_drives_the_real_executor() {
        let clock = RealClock::new();
        let unused_port = {
            let temp = TcpListener::bind("127.0.0.1:0").unwrap();
            temp.local_addr().unwrap().port()
        };
        let request = HttpRequest::get(
            format!("http://127.0.0.1:{unused_port}"),
            timeouts(50, 50, Some(50)),
        );
        let budget = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let result = BlockingTransport.send(&request, &budget, &clock);
        assert_eq!(
            result,
            Err(FailureClass::ConnectTimeout),
            "the port must classify a refused connection through execute_single"
        );
    }

    /// The real local-file arm serves the named file's bytes from disk and
    /// reports the resolved path and the file's modification time through the
    /// response headers, the facts the evidence capsule records.
    #[test]
    fn the_local_file_arm_reads_the_named_file_from_disk() {
        let clock = RealClock::new();
        let scratch = test_support::StateDir::new();
        let file = scratch.path().join("rollout-example.jsonl");
        test_support::scratch_files::write(&file, b"{\"payload\":{\"rate_limits\":{}}}");
        test_support::scratch_files::pin_mtime(&file, 1_788_646_100);

        let request = HttpRequest::local_file(&file, timeouts(50, 50, Some(50)));
        let budget = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let response = BlockingTransport
            .send(&request, &budget, &clock)
            .expect("an existing file reads through the local-file arm");

        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), b"{\"payload\":{\"rate_limits\":{}}}");
        assert_eq!(
            response.header(LOCAL_FILE_PATH_HEADER),
            Some(file.to_str().unwrap())
        );
        assert_eq!(
            response.header(LOCAL_FILE_MTIME_HEADER),
            Some("1788646100000000000")
        );
    }

    /// A source that names nothing to read is the no-evidence class, never a
    /// fabricated empty answer: the same class an empty provider body takes.
    #[test]
    fn a_local_file_that_names_nothing_is_malformed_body() {
        let clock = RealClock::new();
        let scratch = test_support::StateDir::new();
        let request = HttpRequest::local_file(
            scratch.path().join("absent.jsonl"),
            timeouts(50, 50, Some(50)),
        );
        let budget = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let result = BlockingTransport.send(&request, &budget, &clock);
        assert_eq!(
            result,
            Err(FailureClass::MalformedBody),
            "a missing file must come back as the no-evidence class"
        );
    }

    /// The newest-glob arm resolves the newest file under the directory by
    /// modification time, across nested dated subdirectories, with each
    /// file's mtime pinned explicitly so the answer cannot depend on file
    /// creation order or filesystem timestamp granularity.
    #[test]
    fn the_newest_glob_arm_resolves_the_newest_file_across_dated_subdirectories() {
        let clock = RealClock::new();
        let scratch = test_support::StateDir::new();
        let sessions = scratch.path().join("sessions");
        let set_mtime = |path: &std::path::Path, seconds: u64| {
            test_support::scratch_files::pin_mtime(path, seconds);
        };
        for (subdir, marker) in [
            ("2026/07/04", "oldest"),
            ("2026/08/20", "middle"),
            ("2026/09/05", "newest"),
        ] {
            let dir = sessions.join(subdir);
            test_support::scratch_files::create_dir_all(&dir);
            test_support::scratch_files::write(&dir.join("rollout-session.jsonl"), marker);
        }
        set_mtime(
            &sessions.join("2026/07/04/rollout-session.jsonl"),
            1_700_000_000,
        );
        set_mtime(
            &sessions.join("2026/08/20/rollout-session.jsonl"),
            1_800_000_000,
        );
        set_mtime(
            &sessions.join("2026/09/05/rollout-session.jsonl"),
            1_900_000_000,
        );

        let request = HttpRequest::newest_local_file(
            &sessions,
            "rollout-*.jsonl",
            timeouts(50, 50, Some(50)),
        );
        let budget = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let response = BlockingTransport
            .send(&request, &budget, &clock)
            .expect("a tree with matches resolves the newest");

        assert_eq!(response.body(), b"newest");
        assert!(
            response
                .header(LOCAL_FILE_PATH_HEADER)
                .unwrap()
                .contains("2026/09/05")
        );
        assert_eq!(
            response.header(LOCAL_FILE_MTIME_HEADER),
            Some("1900000000000000000")
        );
    }

    /// The glob matches file names only, and a tree with no readable match
    /// leaves the caller its no-evidence class rather than an empty answer.
    #[test]
    fn the_newest_glob_arm_skips_non_matching_names_and_reports_no_evidence_when_none_match() {
        let clock = RealClock::new();
        let scratch = test_support::StateDir::new();
        let sessions = scratch.path().join("sessions");
        test_support::scratch_files::create_dir_all(&sessions.join("2026/09/05"));
        test_support::scratch_files::write(
            &sessions.join("2026/09/05/thoughts.log"),
            "not a rollout",
        );

        let request = HttpRequest::newest_local_file(
            &sessions,
            "rollout-*.jsonl",
            timeouts(50, 50, Some(50)),
        );
        let budget = CommandBudget::new(MonotonicDuration::from_millis(200), &clock);
        let result = BlockingTransport.send(&request, &budget, &clock);
        assert_eq!(
            result,
            Err(FailureClass::MalformedBody),
            "a tree with no matching file must come back as the no-evidence class"
        );
    }

    /// A local-file read clipped to an expired command budget is refused
    /// before the disk is touched, like every other request the port takes.
    #[test]
    fn the_local_file_arm_honours_an_expired_command_budget() {
        let mut clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
        let scratch = test_support::StateDir::new();
        let file = scratch.path().join("rollout-example.jsonl");
        test_support::scratch_files::write(&file, "bytes");
        let budget = CommandBudget::new(MonotonicDuration::from_millis(100), &clock);
        clock.advance(MonotonicDuration::from_millis(150));
        let request = HttpRequest::local_file(&file, timeouts(50, 50, Some(50)));
        let result = BlockingTransport.send(&request, &budget, &clock);
        assert_eq!(
            result,
            Err(FailureClass::TotalBudgetExpired),
            "an expired budget must refuse the read before the disk is touched"
        );
    }
}
