//! Shared HTTP execution and standards-style server-sent event decoding.

use std::collections::BTreeMap;

use futures::StreamExt;
use reqwest::{Client, IntoUrl, Request, RequestBuilder, Response};
use tokio_util::sync::CancellationToken;

use crate::provider::{OnResponseFn, ProviderError, ProviderResponse};
use crate::types::Model;

/// A reusable HTTP client for adapter-built requests.
#[derive(Clone, Debug)]
pub(crate) struct HttpTransport {
    client: Client,
}

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

impl HttpTransport {
    /// Wrap an already configured HTTP client.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Build a POST request with the configured client.
    pub(crate) fn post(&self, url: impl IntoUrl) -> RequestBuilder {
        self.client.post(url)
    }

    /// Execute a request, stopping cancellation at the response-header boundary.
    ///
    /// The optional callback observes headers before the response body is returned
    /// to the adapter for consumption.
    pub(crate) async fn execute(
        &self,
        request: Request,
        model: &Model,
        cancellation: Option<&CancellationToken>,
        on_response: Option<&OnResponseFn>,
    ) -> Result<Response, TransportError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(TransportError::Cancelled);
        }

        let response = if let Some(signal) = cancellation {
            tokio::select! {
                () = signal.cancelled() => return Err(TransportError::Cancelled),
                result = self.client.execute(request) => result.map_err(TransportError::Request)?,
            }
        } else {
            self.client
                .execute(request)
                .await
                .map_err(TransportError::Request)?
        };

        if let Some(callback) = on_response {
            let metadata = response_metadata(&response);
            callback(&metadata, model)
                .await
                .map_err(TransportError::Callback)?;
        }

        Ok(response)
    }

    /// Read a bounded HTTP error body while honoring request cancellation.
    pub(crate) async fn read_error_body(
        response: Response,
        cancellation: Option<&CancellationToken>,
    ) -> Result<String, TransportError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(TransportError::Cancelled);
        }

        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        loop {
            let next = if let Some(signal) = cancellation {
                tokio::select! {
                    () = signal.cancelled() => return Err(TransportError::Cancelled),
                    chunk = chunks.next() => chunk,
                }
            } else {
                chunks.next().await
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(TransportError::Body)?;
            let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
            if remaining == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if body.len() >= MAX_ERROR_BODY_BYTES {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// Failure classes produced before an adapter starts consuming an HTTP body.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// The request was cancelled before response headers arrived.
    #[error("request cancelled")]
    Cancelled,
    /// Sending the HTTP request failed.
    #[error("request failed: {0}")]
    Request(#[source] reqwest::Error),
    /// The response callback rejected the received headers.
    #[error("response callback failed: {0}")]
    Callback(#[source] ProviderError),
    /// Reading the response body failed.
    #[error("response body failed: {0}")]
    Body(#[source] reqwest::Error),
}

fn response_metadata(response: &Response) -> ProviderResponse {
    let mut headers = BTreeMap::new();
    for (name, value) in response.headers() {
        if let Ok(value) = value.to_str() {
            headers
                .entry(name.as_str().to_owned())
                .and_modify(|existing: &mut String| {
                    existing.push_str(", ");
                    existing.push_str(value);
                })
                .or_insert_with(|| value.to_owned());
        }
    }
    ProviderResponse {
        status: response.status().as_u16(),
        headers,
    }
}

/// A byte-preserving incremental line splitter for SSE transports.
///
/// Returned lines exclude their terminators. Empty lines are retained, and all
/// three line endings (`CR`, `LF`, and `CRLF`) are recognized across chunks.
#[derive(Debug, Default)]
pub(crate) struct SseLineBuffer {
    pending: Vec<u8>,
    trailing_cr: bool,
}

impl SseLineBuffer {
    /// Add bytes and return every complete line made available by the chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        for &byte in chunk {
            if self.trailing_cr {
                lines.push(std::mem::take(&mut self.pending));
                self.trailing_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                b'\r' => self.trailing_cr = true,
                b'\n' => lines.push(std::mem::take(&mut self.pending)),
                _ => self.pending.push(byte),
            }
        }
        lines
    }

    /// Finish the stream and return its final unterminated line, if any.
    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        if self.trailing_cr {
            lines.push(std::mem::take(&mut self.pending));
            self.trailing_cr = false;
        } else if !self.pending.is_empty() {
            lines.push(std::mem::take(&mut self.pending));
        }
        lines
    }
}

/// One complete standards-style data SSE event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DataSseEvent {
    /// Data lines joined with a single newline.
    Data(String),
    /// The conventional provider stream terminator.
    Done,
}

/// A decoding failure for a non-UTF-8 SSE data line.
#[derive(Debug, thiserror::Error)]
#[error("SSE data is not valid UTF-8")]
pub(crate) struct SseDecodeError(#[from] std::str::Utf8Error);

/// Incrementally decodes standard SSE events containing one or more `data:` lines.
///
/// Comments and non-data fields are ignored. A blank line dispatches the joined
/// data payload, and a payload equal to `[DONE]` becomes [`DataSseEvent::Done`].
#[derive(Debug, Default)]
pub(crate) struct DataSseDecoder {
    lines: SseLineBuffer,
    data: String,
    has_data: bool,
}

impl DataSseDecoder {
    /// Consume an arbitrary response-body chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<DataSseEvent>, SseDecodeError> {
        let mut events = Vec::new();
        for line in self.lines.push(chunk) {
            if let Some(event) = self.push_line(&line)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Finish the response body, dispatching a pending data event at EOF.
    pub(crate) fn finish(&mut self) -> Result<Vec<DataSseEvent>, SseDecodeError> {
        let mut events = Vec::new();
        for line in self.lines.finish() {
            if let Some(event) = self.push_line(&line)? {
                events.push(event);
            }
        }
        if let Some(event) = self.dispatch() {
            events.push(event);
        }
        Ok(events)
    }

    fn push_line(&mut self, line: &[u8]) -> Result<Option<DataSseEvent>, SseDecodeError> {
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line[0] == b':' {
            return Ok(None);
        }

        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (line, &[][..]),
        };
        if field != b"data" {
            return Ok(None);
        }

        let value = value.strip_prefix(b" ").unwrap_or(value);
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(std::str::from_utf8(value)?);
        self.has_data = true;
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<DataSseEvent> {
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        let data = std::mem::take(&mut self.data);
        Some(if data == "[DONE]" {
            DataSseEvent::Done
        } else {
            DataSseEvent::Data(data)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    use futures::future::BoxFuture;

    use super::*;
    use crate::types::{ModelCost, ModelInput};

    fn model() -> Model {
        Model {
            id: "test".into(),
            name: "Test".into(),
            api: "test-api".into(),
            provider: "test-provider".into(),
            base_url: "http://127.0.0.1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    /// Render one HTTP/1.1 stub reply with an exact `Content-Length`.
    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
    /// Drain one HTTP request from a test stub connection before replying.
    fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut buf = [0_u8; 4096];
        for _ in 0..16 {
            match stream.read(&mut buf) {
                Ok(n) if n > 0 => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                _ => break,
            }
        }
        if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = pos + 4;
            let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while raw.len() < header_end + content_length {
                match stream.read(&mut buf) {
                    Ok(n) if n > 0 => raw.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
        }
        raw
    }

    #[test]
    fn line_buffer_accepts_cr_lf_and_crlf_across_chunks() {
        let mut lines = SseLineBuffer::default();
        assert_eq!(lines.push(b"a\rb\nc\r"), vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(lines.push(b"\nd"), vec![b"c".to_vec()]);
        assert_eq!(lines.finish(), vec![b"d".to_vec()]);
    }

    #[test]
    fn data_sse_handles_multiple_data_lines_comments_and_done() -> Result<(), SseDecodeError> {
        let fixture =
            b": comment\r\nevent: ignored\r\ndata: one\r\ndata:two\r\n\r\ndata: [DONE]\n\n";
        let mut decoder = DataSseDecoder::default();
        let mut events = decoder.push(fixture)?;
        events.extend(decoder.finish()?);
        assert_eq!(
            events,
            vec![DataSseEvent::Data("one\ntwo".into()), DataSseEvent::Done]
        );
        Ok(())
    }

    #[test]
    fn standards_fixture_is_invariant_at_every_byte_split() -> Result<(), SseDecodeError> {
        let fixture = b"data: {\"a\":1}\r\ndata: next\r\n\r\n: keepalive\ndata: [DONE]\n\n";
        let expected = vec![
            DataSseEvent::Data("{\"a\":1}\nnext".into()),
            DataSseEvent::Done,
        ];
        for split in 0..=fixture.len() {
            let mut decoder = DataSseDecoder::default();
            let mut actual = decoder.push(&fixture[..split])?;
            actual.extend(decoder.push(&fixture[split..])?);
            actual.extend(decoder.finish()?);
            assert_eq!(actual, expected, "split at byte {split}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_and_callback_failures_stay_distinct()
    -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new();
        let transport = HttpTransport::new(client.clone());
        let token = CancellationToken::new();
        token.cancel();
        let request = client.get("http://127.0.0.1:1/").build()?;
        assert!(matches!(
            transport
                .execute(request, &model(), Some(&token), None)
                .await,
            Err(TransportError::Cancelled)
        ));

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> std::io::Result<()> {
            for _ in 0..16 {
                let Ok((mut socket, _)) = listener.accept() else {
                    return Ok(());
                };
                let req = read_http_request(&mut socket);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with("GET /callback-test ") {
                    let _ = socket.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                socket.write_all(
                    b"HTTP/1.1 204 No Content\r\nX-Test: yes\r\nContent-Length: 0\r\n\r\n",
                )?;
                return Ok(());
            }
            Ok(())
        });
        let callback: OnResponseFn = Arc::new(|response, _model| {
            let status = response.status;
            Box::pin(async move { Err(ProviderError::new(format!("rejected {status}"))) })
                as BoxFuture<'_, Result<(), ProviderError>>
        });
        let request = client
            .get(format!("http://{address}/callback-test"))
            .build()?;
        let result = transport
            .execute(request, &model(), None, Some(&callback))
            .await;
        assert!(matches!(result, Err(TransportError::Callback(_))));
        server.join().map_err(|_| "server thread failed")??;
        Ok(())
    }

    #[tokio::test]
    async fn post_uses_the_configured_client() -> Result<(), Box<dyn std::error::Error>> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-client",
            reqwest::header::HeaderValue::from_static("configured"),
        );
        let client = Client::builder().default_headers(headers).build()?;
        let transport = HttpTransport::new(client);

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            for _ in 0..16 {
                let Ok((mut socket, _)) = listener.accept() else {
                    return Ok(Vec::new());
                };
                let req = read_http_request(&mut socket);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with("POST /error ") {
                    let _ = socket.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                let request_bytes = req.clone();
                socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")?;
                return Ok(request_bytes);
            }
            Ok(Vec::new())
        });

        let response = transport
            .post(format!("http://{address}/error"))
            .body("{}")
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        let request = server.join().map_err(|_| "server thread failed")??;
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("POST /error "));
        assert!(request.contains("x-client: configured"));
        Ok(())
    }

    #[tokio::test]
    async fn read_error_body_stops_at_64kib() -> Result<(), Box<dyn std::error::Error>> {
        let payload = vec![b'a'; MAX_ERROR_BODY_BYTES + 128];
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn({
            let payload = payload.clone();
            move || -> std::io::Result<()> {
                for _ in 0..16 {
                    let Ok((mut socket, _)) = listener.accept() else {
                        return Ok(());
                    };
                    let req = read_http_request(&mut socket);
                    let text = String::from_utf8_lossy(&req);
                    if !text.starts_with("GET /body-64k ") {
                        let _ = socket.write_all(http_response("404 Not Found", "").as_slice());
                        continue;
                    }
                    let header = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n",
                        payload.len()
                    );
                    socket.write_all(header.as_bytes())?;
                    socket.write_all(&payload)?;
                    return Ok(());
                }
                Ok(())
            }
        });

        let client = Client::new();
        let response = client
            .get(format!("http://{address}/body-64k"))
            .send()
            .await?;
        let body = HttpTransport::read_error_body(response, None).await?;
        assert_eq!(body.len(), MAX_ERROR_BODY_BYTES);
        assert!(body.bytes().all(|byte| byte == b'a'));
        server.join().map_err(|_| "server thread failed")??;
        Ok(())
    }

    #[tokio::test]
    async fn read_error_body_honors_cancellation_on_kept_open_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || -> std::io::Result<()> {
            for _ in 0..16 {
                let Ok((mut socket, _)) = listener.accept() else {
                    return Ok(());
                };
                let req = read_http_request(&mut socket);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with("GET /hang ") {
                    let _ = socket.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                socket.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nTransfer-Encoding: chunked\r\n\r\n",
                )?;
                socket.write_all(b"5\r\nhello\r\n")?;
                socket.flush()?;
                let _ = started_tx.send(());
                // Keep the response open until the client disconnects.
                let mut sink = [0_u8; 64];
                let _ = socket.read(&mut sink);
                return Ok(());
            }
            Ok(())
        });

        let client = Client::new();
        let response = client.get(format!("http://{address}/hang")).send().await?;
        started_rx.recv_timeout(std::time::Duration::from_secs(2))?;
        let token = CancellationToken::new();
        let cancel = token.clone();
        let read =
            tokio::spawn(
                async move { HttpTransport::read_error_body(response, Some(&token)).await },
            );
        // Allow the reader to enter the next-chunk wait, then cancel mid-body.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), read).await??;
        assert!(matches!(result, Err(TransportError::Cancelled)));
        server.join().map_err(|_| "server thread failed")??;
        Ok(())
    }

    /// Send one sweeper-style `GET /` at a stub, draining its 404 reply.
    fn sweep_probe_stub(address: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut stream = TcpStream::connect(address)?;
        stream.write_all(b"GET / HTTP/1.1\r\nHost: sweep\r\nConnection: close\r\n\r\n")?;
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf);
        Ok(())
    }

    /// A localhost sweep (`GET /` at every new listener) must not consume
    /// the scripted stub reply: after probing the stub, the real POST
    /// still gets its 204 response with the configured header.
    #[tokio::test]
    async fn sweep_get_does_not_consume_stub_scripts() -> Result<(), Box<dyn std::error::Error>> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-client",
            reqwest::header::HeaderValue::from_static("configured"),
        );
        let client = Client::builder().default_headers(headers).build()?;
        let transport = HttpTransport::new(client);

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            for _ in 0..16 {
                let Ok((mut socket, _)) = listener.accept() else {
                    return Ok(Vec::new());
                };
                let req = read_http_request(&mut socket);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with("POST /error ") {
                    let _ = socket.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                let request_bytes = req.clone();
                socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")?;
                return Ok(request_bytes);
            }
            Ok(Vec::new())
        });
        sweep_probe_stub(&address.to_string())?;
        let response = transport
            .post(format!("http://{address}/error"))
            .body("{}")
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        let request = server.join().map_err(|_| "server thread failed")??;
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("POST /error "));
        assert!(request.contains("x-client: configured"));
        Ok(())
    }
}
