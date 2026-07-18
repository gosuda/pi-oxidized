//! Axum-backed HTTP recorder for provider conformance tests.

use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header::HOST},
    response::Response,
    routing::any,
};
use futures::stream;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify, oneshot, watch},
    task::JoinHandle,
};

/// One response body chunk and the delay applied immediately before it is sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseChunk {
    /// Exact bytes sent for this chunk.
    pub bytes: Vec<u8>,
    /// Optional delay before sending `bytes`.
    pub delay: Option<Duration>,
}

impl ResponseChunk {
    /// Creates an immediately delivered chunk.
    pub fn immediate(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            delay: None,
        }
    }

    /// Creates a chunk delivered after `delay`.
    pub fn delayed(bytes: impl Into<Vec<u8>>, delay: Duration) -> Self {
        Self {
            bytes: bytes.into(),
            delay: Some(delay),
        }
    }
}

/// A queued HTTP response served once, in request order.
#[derive(Clone, Debug)]
pub struct ResponseSpec {
    /// HTTP response status.
    pub status: StatusCode,
    /// Exact response headers, including repeated values.
    pub headers: HeaderMap,
    /// Ordered response body chunks.
    pub chunks: Vec<ResponseChunk>,
    /// Keeps the response body open after its final chunk until server shutdown.
    pub keep_open: bool,
}

impl ResponseSpec {
    /// Creates an empty response with `status`.
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            chunks: Vec::new(),
            keep_open: false,
        }
    }

    /// Creates a response with one immediate body chunk.
    pub fn bytes(status: StatusCode, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            chunks: vec![ResponseChunk::immediate(bytes)],
            keep_open: false,
        }
    }
}

/// An exact request observed by the loopback server.
#[derive(Clone, Debug)]
pub struct CapturedRequest {
    /// Arrival order assigned before the request body is read.
    pub sequence: u64,
    /// HTTP method.
    pub method: Method,
    /// URI path, without the query string.
    pub path: String,
    /// URI query, without the leading `?`.
    pub query: Option<String>,
    /// Exact request headers, including repeated values.
    pub headers: HeaderMap,
    /// Exact request body bytes.
    pub body: Vec<u8>,
}

/// Failures produced by the loopback harness.
#[derive(Debug, Error)]
pub enum HttpHarnessError {
    /// The operating system refused the loopback listener.
    #[error("failed to bind provider test server to 127.0.0.1:0: {0}")]
    Bind(#[source] std::io::Error),
    /// A response attempted to redirect away from this server.
    #[error("response redirect {value:?} is not local to {authority}: {reason}")]
    ExternalRedirect {
        /// Rejected Location value.
        value: String,
        /// Required loopback authority.
        authority: String,
        /// Validation detail.
        reason: String,
    },
    /// Axum failed while serving the listener.
    #[error("provider test server failed: {0}")]
    Serve(#[source] std::io::Error),
    /// The server task could not be joined.
    #[error("provider test server task failed: {0}")]
    Join(#[source] tokio::task::JoinError),
    /// The requested number of captures did not arrive in time.
    #[error("timed out after {timeout:?} waiting for {expected} requests; captured {actual}")]
    CaptureTimeout {
        /// Requested capture count.
        expected: usize,
        /// Capture count at timeout.
        actual: usize,
        /// Time allowed.
        timeout: Duration,
    },
}

#[derive(Debug)]
struct ServerState {
    authority: String,
    inner: Mutex<ServerInner>,
    captured: Notify,
    closing: watch::Sender<bool>,
}

#[derive(Debug)]
struct ServerInner {
    next_sequence: u64,
    responses: VecDeque<ResponseSpec>,
    requests: BTreeMap<u64, CapturedRequest>,
}

/// A loopback-only Axum server with queued responses and request capture.
#[derive(Debug)]
pub struct LocalHttpServer {
    address: SocketAddr,
    state: Arc<ServerState>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

impl LocalHttpServer {
    /// Binds `127.0.0.1:0`, validates every redirect, and starts serving.
    pub async fn start(
        responses: impl IntoIterator<Item = ResponseSpec>,
    ) -> Result<Self, HttpHarnessError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .map_err(HttpHarnessError::Bind)?;
        let address = listener.local_addr().map_err(HttpHarnessError::Bind)?;
        if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(HttpHarnessError::Bind(std::io::Error::other(format!(
                "listener unexpectedly bound to {address}"
            ))));
        }

        let authority = address.to_string();
        let responses = responses.into_iter().collect::<VecDeque<_>>();
        validate_redirects(&responses, &authority)?;

        let (closing, _) = watch::channel(false);
        let state = Arc::new(ServerState {
            authority,
            inner: Mutex::new(ServerInner {
                next_sequence: 0,
                responses,
                requests: BTreeMap::new(),
            }),
            captured: Notify::new(),
            closing,
        });
        let app = Router::new()
            .fallback(any(handle_request))
            .with_state(Arc::clone(&state));
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _result = shutdown_rx.await;
                })
                .await
        });

        Ok(Self {
            address,
            state,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    /// Returns the bound loopback socket.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Returns an HTTP base URL without a trailing slash.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Removes and returns all completed captures in deterministic arrival order.
    pub async fn take_requests(&self) -> Vec<CapturedRequest> {
        let mut inner = self.state.inner.lock().await;
        std::mem::take(&mut inner.requests).into_values().collect()
    }

    /// Waits until at least `expected` requests have been fully captured.
    pub async fn wait_for_requests(
        &self,
        expected: usize,
        timeout: Duration,
    ) -> Result<(), HttpHarnessError> {
        let wait = async {
            loop {
                let notified = self.state.captured.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let actual = self.state.inner.lock().await.requests.len();
                if actual >= expected {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(timeout, wait).await.is_err() {
            let actual = self.state.inner.lock().await.requests.len();
            return Err(HttpHarnessError::CaptureTimeout {
                expected,
                actual,
                timeout,
            });
        }
        Ok(())
    }

    /// Signals shutdown, closes kept-open bodies, waits for Axum, and returns captures.
    pub async fn shutdown(mut self) -> Result<Vec<CapturedRequest>, HttpHarnessError> {
        self.signal_shutdown();
        if let Some(task) = self.task.take() {
            task.await
                .map_err(HttpHarnessError::Join)?
                .map_err(HttpHarnessError::Serve)?;
        }
        Ok(self.take_requests().await)
    }

    fn signal_shutdown(&mut self) {
        let _changed = self.state.closing.send(true);
        if let Some(shutdown) = self.shutdown.take() {
            let _sent = shutdown.send(());
        }
    }
}

impl Drop for LocalHttpServer {
    fn drop(&mut self) {
        self.signal_shutdown();
        let _detached = self.task.take();
    }
}

async fn handle_request(State(state): State<Arc<ServerState>>, request: Request) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
    let headers = request.headers().clone();
    let host_is_local = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host == state.authority);

    let (sequence, response_spec) = {
        let mut inner = state.inner.lock().await;
        let sequence = inner.next_sequence;
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        let response = host_is_local.then(|| inner.responses.pop_front()).flatten();
        (sequence, response)
    };

    let (parts, body) = request.into_parts();
    let body_result = to_bytes(body, usize::MAX).await;
    let (body, body_error) = match body_result {
        Ok(body) => (body.to_vec(), None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    {
        let mut inner = state.inner.lock().await;
        inner.requests.insert(
            sequence,
            CapturedRequest {
                sequence,
                method,
                path,
                query,
                headers,
                body,
            },
        );
    }
    state.captured.notify_waiters();

    if let Some(error) = body_error {
        return plain_response(
            StatusCode::BAD_REQUEST,
            format!("failed to capture request body: {error}"),
        );
    }
    if !host_is_local {
        return plain_response(
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "provider test request Host must be {}; received {:?}",
                state.authority,
                parts.headers.get(HOST)
            ),
        );
    }
    match response_spec {
        Some(spec) => response_from_spec(spec, state.closing.subscribe()),
        None => plain_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "provider test server has no queued response".to_owned(),
        ),
    }
}

fn response_from_spec(spec: ResponseSpec, closing: watch::Receiver<bool>) -> Response<Body> {
    struct ChunkState {
        chunks: VecDeque<ResponseChunk>,
        keep_open: bool,
        closing: watch::Receiver<bool>,
    }

    let body_stream = stream::unfold(
        ChunkState {
            chunks: spec.chunks.into(),
            keep_open: spec.keep_open,
            closing,
        },
        |mut state| async move {
            if let Some(chunk) = state.chunks.pop_front() {
                if let Some(delay) = chunk.delay {
                    tokio::time::sleep(delay).await;
                }
                return Some((Ok::<Bytes, Infallible>(Bytes::from(chunk.bytes)), state));
            }
            if state.keep_open && !*state.closing.borrow() {
                let _changed = state.closing.changed().await;
            }
            None
        },
    );
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = spec.status;
    *response.headers_mut() = spec.headers;
    response
}

fn plain_response(status: StatusCode, body: String) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
}

fn validate_redirects(
    responses: &VecDeque<ResponseSpec>,
    authority: &str,
) -> Result<(), HttpHarnessError> {
    for response in responses {
        for value in response.headers.get_all(axum::http::header::LOCATION) {
            let value = value
                .to_str()
                .map_err(|error| HttpHarnessError::ExternalRedirect {
                    value: format!("{value:?}"),
                    authority: authority.to_owned(),
                    reason: format!("Location is not valid UTF-8: {error}"),
                })?;
            validate_redirect(value, authority)?;
        }
    }
    Ok(())
}

fn validate_redirect(value: &str, authority: &str) -> Result<(), HttpHarnessError> {
    if value.starts_with('/') && !value.starts_with("//") {
        return Ok(());
    }
    if !value.contains(':') && !value.starts_with("//") {
        return Ok(());
    }

    let parsed =
        reqwest::Url::parse(value).map_err(|error| HttpHarnessError::ExternalRedirect {
            value: value.to_owned(),
            authority: authority.to_owned(),
            reason: format!("invalid absolute URL: {error}"),
        })?;
    let is_local = parsed.scheme() == "http"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.host_str() == Some("127.0.0.1")
        && parsed.port()
            == authority
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok());
    if is_local {
        Ok(())
    } else {
        Err(HttpHarnessError::ExternalRedirect {
            value: value.to_owned(),
            authority: authority.to_owned(),
            reason: "absolute redirects must use this server's http loopback authority".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderName, HeaderValue};
    use reqwest::Client;

    use super::*;

    #[tokio::test]
    async fn binds_only_ephemeral_ipv4_loopback() -> Result<(), Box<dyn std::error::Error>> {
        let server = LocalHttpServer::start([]).await?;
        assert_eq!(server.address().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(server.address().port(), 0);
        server.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn streams_chunks_in_order_and_captures_request_exactly()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut spec = ResponseSpec::new(StatusCode::CREATED);
        spec.headers.insert(
            HeaderName::from_static("x-response"),
            HeaderValue::from_static("yes"),
        );
        spec.chunks = vec![
            ResponseChunk::immediate(b"one".to_vec()),
            ResponseChunk::delayed(b"two".to_vec(), Duration::from_millis(2)),
            ResponseChunk::immediate(b"three".to_vec()),
        ];
        let server = LocalHttpServer::start([spec]).await?;
        let response = Client::new()
            .post(format!("{}/v1/messages?stream=true", server.base_url()))
            .header("x-request", "present")
            .body(vec![0, 1, 2, 255])
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get("x-response"),
            Some(&HeaderValue::from_static("yes"))
        );
        assert_eq!(response.bytes().await?.as_ref(), b"onetwothree");

        let requests = server.shutdown().await?;
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.sequence, 0);
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.query.as_deref(), Some("stream=true"));
        assert_eq!(
            request.headers.get("x-request"),
            Some(&HeaderValue::from_static("present"))
        );
        assert_eq!(request.body, vec![0, 1, 2, 255]);
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_closes_a_kept_open_response() -> Result<(), Box<dyn std::error::Error>> {
        let mut spec = ResponseSpec::bytes(StatusCode::OK, b"prefix".to_vec());
        spec.keep_open = true;
        let server = LocalHttpServer::start([spec]).await?;
        let response = Client::new().get(server.base_url()).send().await?;
        server.wait_for_requests(1, Duration::from_secs(1)).await?;
        let shutdown = server.shutdown();
        let (requests, body) = tokio::join!(shutdown, response.bytes());
        assert_eq!(requests?.len(), 1);
        assert_eq!(body?.as_ref(), b"prefix");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_external_redirects_before_serving() {
        let mut spec = ResponseSpec::new(StatusCode::FOUND);
        spec.headers.insert(
            axum::http::header::LOCATION,
            HeaderValue::from_static("https://example.com/escape"),
        );
        let error = LocalHttpServer::start([spec]).await.err();
        assert!(matches!(
            error,
            Some(HttpHarnessError::ExternalRedirect { .. })
        ));
    }
}
