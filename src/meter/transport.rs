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
use std::time::Duration;

use crate::domain::failure::{FailureClass, HttpStatusClass};
use crate::domain::time::{Clock, MonotonicDuration, MonotonicInstant};

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

/// An outgoing HTTP request definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeouts: RequestTimeoutConfig,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>, timeouts: RequestTimeoutConfig) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            timeouts,
        }
    }

    pub fn post(url: impl Into<String>, body: Vec<u8>, timeouts: RequestTimeoutConfig) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: Some(body),
            timeouts,
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

/// Executes a single HTTP request respecting the command-wide budget.
pub fn execute_single(
    request: &HttpRequest,
    budget: &CommandBudget,
    clock: &impl Clock,
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

    if let Some(total) = effective_timeouts.total_timeout {
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
            if budget.is_expired(clock) {
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
        if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
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

fn map_transport_error(err: &ureq::Transport) -> FailureClass {
    let msg = err.to_string().to_lowercase();
    match err.kind() {
        ureq::ErrorKind::Dns => FailureClass::DnsFailure,
        ureq::ErrorKind::ConnectionFailed => {
            if msg.contains("timeout") || msg.contains("timed out") {
                FailureClass::ConnectTimeout
            } else if msg.contains("refused") {
                FailureClass::HttpStatus(HttpStatusClass::ClientError)
            } else {
                FailureClass::ConnectTimeout
            }
        }
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
        _ => {
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
            match resp.result {
                Err(FailureClass::TotalBudgetExpired) | Err(FailureClass::ReadTimeout) => {}
                other => panic!(
                    "expected TotalBudgetExpired or ReadTimeout within budget, got {other:?}"
                ),
            }
        }

        // Must complete within budget plus shutdown tolerance
        let max_allowed = budget_dur.as_nanos() + SHUTDOWN_TOLERANCE.as_nanos();
        assert!(
            elapsed.as_nanos() <= max_allowed,
            "elapsed {elapsed:?} exceeded budget + tolerance {max_allowed}ns"
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
}
