//! Shared HTTP helpers for OAuth token and device-code requests.
//!
//! Providers issue short JSON/form requests outside the streaming provider
//! transport. This client mirrors the transport body-read limits so error text
//! stays bounded and cancellation is observed through response headers and body
//! reads.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt;
use reqwest::{Client, Response};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Per-request timeout for OAuth token/device HTTP calls.
pub const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Raw error-body read cap, matching provider transport.
pub const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Maximum Unicode scalar values retained from an OAuth error body for display.
pub const MAX_ERROR_BODY_CHARS: usize = 4_000;

/// Failures from OAuth HTTP helpers.
#[derive(Debug, thiserror::Error)]
pub enum AuthHttpError {
    /// The request was cancelled before completion.
    #[error("Login cancelled")]
    Cancelled,
    /// Sending the HTTP request failed.
    #[error("request failed: {0}")]
    Request(#[source] reqwest::Error),
    /// Reading the response body failed.
    #[error("response body failed: {0}")]
    Body(#[source] reqwest::Error),
    /// The server returned a non-success status.
    #[error("HTTP request failed. status={status}; url={url}; body={body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Request URL.
        url: String,
        /// Bounded response body text.
        body: String,
    },
    /// The response body was not valid JSON.
    #[error("{0}")]
    InvalidJson(String),
}

impl AuthHttpError {
    /// Convert into a user-facing auth-flow error.
    #[must_use]
    pub fn into_auth_error(self) -> super::error::AuthError {
        match self {
            Self::Cancelled => super::error::AuthError::Cancelled,
            other => super::error::AuthError::message(other.to_string()),
        }
    }
}

/// Successful OAuth HTTP response with parsed JSON body when possible.
#[derive(Clone, Debug)]
pub struct AuthHttpResponse {
    /// Whether the status is in the 2xx range.
    pub ok: bool,
    /// HTTP status code.
    pub status: u16,
    /// Parsed JSON object body, or an empty object when the body was not an object.
    pub body: Value,
    /// Raw response body text (already length-capped for non-success paths).
    pub raw_body: String,
}

/// Shared reqwest client for OAuth flows.
#[derive(Clone, Debug)]
pub struct AuthHttpClient {
    client: Client,
}

impl AuthHttpClient {
    /// Build a client with the OAuth request timeout.
    ///
    /// # Errors
    ///
    /// Returns [`AuthHttpError::Request`] if the underlying client cannot be
    /// constructed (for example TLS backend failure).
    pub fn new() -> Result<Self, AuthHttpError> {
        let client = Client::builder()
            .timeout(OAUTH_REQUEST_TIMEOUT)
            .build()
            .map_err(AuthHttpError::Request)?;
        Ok(Self { client })
    }

    /// Wrap an already configured client.
    #[must_use]
    pub fn from_client(client: Client) -> Self {
        Self { client }
    }

    /// POST JSON and return the raw response body on success.
    ///
    /// # Errors
    ///
    /// Returns [`AuthHttpError::Cancelled`] on abort, [`AuthHttpError::Request`] /
    /// [`AuthHttpError::Body`] on transport failure, or [`AuthHttpError::Http`] for
    /// non-success status codes.
    pub async fn post_json<T: Serialize + ?Sized>(
        &self,
        url: &str,
        body: &T,
        headers: Option<&BTreeMap<String, String>>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<String, AuthHttpError> {
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body);
        if let Some(headers) = headers {
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        let response = execute_request(&self.client, request, cancellation).await?;
        let status = response.status();
        let raw = read_error_body(response, cancellation).await?;
        if !status.is_success() {
            return Err(AuthHttpError::Http {
                status: status.as_u16(),
                url: url.to_owned(),
                body: truncate_error_body(&raw),
            });
        }
        Ok(raw)
    }

    /// POST `application/x-www-form-urlencoded` and parse a JSON object body.
    ///
    /// # Errors
    ///
    /// Returns transport/cancellation failures, or [`AuthHttpError::InvalidJson`]
    /// when a successful response body is not JSON.
    pub async fn post_form(
        &self,
        url: &str,
        fields: &BTreeMap<String, String>,
        headers: Option<&BTreeMap<String, String>>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<AuthHttpResponse, AuthHttpError> {
        let body = encode_form(fields);
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body);
        if let Some(headers) = headers {
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        let response = execute_request(&self.client, request, cancellation).await?;
        finish_json_response(response, cancellation).await
    }

    /// GET JSON and parse a JSON object body.
    ///
    /// # Errors
    ///
    /// Returns transport/cancellation failures, or [`AuthHttpError::InvalidJson`]
    /// when a successful response body is not JSON.
    pub async fn get_json(
        &self,
        url: &str,
        headers: Option<&BTreeMap<String, String>>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<AuthHttpResponse, AuthHttpError> {
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(headers) = headers {
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        let response = execute_request(&self.client, request, cancellation).await?;
        finish_json_response(response, cancellation).await
    }
}

fn encode_form(fields: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&urlencoding_encode(key));
        out.push('=');
        out.push_str(&urlencoding_encode(value));
    }
    out
}

fn urlencoding_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
            }
        }
    }
    out
}

async fn execute_request(
    client: &Client,
    request: reqwest::RequestBuilder,
    cancellation: Option<&CancellationToken>,
) -> Result<Response, AuthHttpError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(AuthHttpError::Cancelled);
    }
    let built = request.build().map_err(AuthHttpError::Request)?;
    if let Some(signal) = cancellation {
        tokio::select! {
            () = signal.cancelled() => Err(AuthHttpError::Cancelled),
            result = client.execute(built) => result.map_err(AuthHttpError::Request),
        }
    } else {
        client.execute(built).await.map_err(AuthHttpError::Request)
    }
}

async fn finish_json_response(
    response: Response,
    cancellation: Option<&CancellationToken>,
) -> Result<AuthHttpResponse, AuthHttpError> {
    let status = response.status();
    let raw = read_error_body(response, cancellation).await?;
    if !status.is_success() {
        // Callers often inspect the body for OAuth error codes; still surface status.
        let body = parse_json_object(&raw);
        return Ok(AuthHttpResponse {
            ok: false,
            status: status.as_u16(),
            body,
            raw_body: truncate_error_body(&raw),
        });
    }
    if raw.trim().is_empty() {
        return Ok(AuthHttpResponse {
            ok: true,
            status: status.as_u16(),
            body: Value::Object(serde_json::Map::new()),
            raw_body: String::new(),
        });
    }
    if serde_json::from_str::<Value>(&raw).is_err() {
        return Err(AuthHttpError::InvalidJson(format!(
            "OAuth endpoint returned invalid JSON (HTTP {})",
            status.as_u16()
        )));
    }
    Ok(AuthHttpResponse {
        ok: true,
        status: status.as_u16(),
        body: parse_json_object(&raw),
        raw_body: raw,
    })
}

/// Read a bounded HTTP body while honoring cancellation.
///
/// # Errors
///
/// Returns [`AuthHttpError::Cancelled`] if the token fires, or
/// [`AuthHttpError::Body`] if a body chunk cannot be read.
pub async fn read_error_body(
    response: Response,
    cancellation: Option<&CancellationToken>,
) -> Result<String, AuthHttpError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(AuthHttpError::Cancelled);
    }

    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    loop {
        let next = if let Some(signal) = cancellation {
            tokio::select! {
                () = signal.cancelled() => return Err(AuthHttpError::Cancelled),
                chunk = chunks.next() => chunk,
            }
        } else {
            chunks.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(AuthHttpError::Body)?;
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

/// Trim and bound a body for inclusion in error messages.
#[must_use]
pub fn truncate_error_body(body: &str) -> String {
    let body = body.trim();
    let count = body.chars().count();
    if count <= MAX_ERROR_BODY_CHARS {
        return body.to_owned();
    }
    let kept: String = body.chars().take(MAX_ERROR_BODY_CHARS).collect();
    format!(
        "{kept}... [truncated {} chars]",
        count - MAX_ERROR_BODY_CHARS
    )
}

fn parse_json_object(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => other,
        Err(_) => Value::Object(serde_json::Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    use super::*;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    fn expect_err<T, E>(result: Result<T, E>, label: &str) -> Result<E, String> {
        match result {
            Ok(_) => Err(err(label)),
            Err(error) => Ok(error),
        }
    }

    /// Render one HTTP/1.1 stub reply with an exact `Content-Length`.
    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
    /// Drain one HTTP request from a test stub connection before replying.
    /// Reads the full head, then drains the body by declared Content-Length
    /// so `close()` never races with an RST from unread queued bytes.
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
    fn spawn_raw_server(
        expected_method: &str,
        expected_path: &str,
        handler: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        let expected_start = format!("{expected_method} {expected_path} ");
        thread::spawn(move || {
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req = read_http_request(&mut stream);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with(&expected_start) {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                handler(stream);
                return;
            }
        });
        Ok(format!("http://{address}{expected_path}"))
    }

    #[tokio::test]
    async fn post_json_success_returns_body() -> TestResult {
        let url = spawn_raw_server("POST", "/", |mut stream| {
            let body = br#"{"access_token":"tok"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                body.len(),
                // JSON body is ASCII.
                String::from_utf8_lossy(body)
            );
            let _ = stream.write_all(response.as_bytes());
        })?;

        let client = AuthHttpClient::new().map_err(|e| err(e.to_string()))?;
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        fields.insert("grant_type".into(), "refresh_token".into());
        let body = client
            .post_json(&url, &fields, None, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert!(body.contains("access_token"));
        Ok(())
    }

    #[tokio::test]
    async fn non_success_body_is_display_capped() -> TestResult {
        let long = "x".repeat(MAX_ERROR_BODY_CHARS + 50);
        let url = {
            let payload = long.clone();
            spawn_raw_server("POST", "/", move |mut stream| {
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
            })?
        };

        let client = AuthHttpClient::new().map_err(|e| err(e.to_string()))?;
        let err_value = expect_err(
            client
                .post_json(&url, &BTreeMap::<String, String>::new(), None, None)
                .await,
            "should fail",
        )?;
        match err_value {
            AuthHttpError::Http { body, status, .. } => {
                assert_eq!(status, 400);
                assert!(body.contains("[truncated"));
                assert!(body.chars().count() < long.chars().count());
                assert!(body.chars().count() <= MAX_ERROR_BODY_CHARS + 40);
            }
            other => return Err(err(format!("unexpected error: {other}"))),
        }
        Ok(())
    }

    #[tokio::test]
    async fn read_error_body_stops_at_64kib() -> TestResult {
        let payload = vec![b'a'; MAX_ERROR_BODY_BYTES + 128];
        let url = {
            let payload = payload.clone();
            spawn_raw_server("GET", "/body-64k", move |mut stream| {
                let header = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&payload);
            })?
        };

        let client = Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        let body = read_error_body(response, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(body.len(), MAX_ERROR_BODY_BYTES);
        assert!(body.bytes().all(|b| b == b'a'));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_during_body_read_maps_to_cancelled() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        thread::spawn(move || {
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req = read_http_request(&mut stream);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with("GET /hang ") {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                let header =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(header);
                // Never finish the body; wait for the client to drop.
                thread::sleep(Duration::from_secs(5));
                return;
            }
        });

        let client = Client::new();
        let response = client
            .get(format!("http://{address}/hang"))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        let token = CancellationToken::new();
        let cancel = token.clone();
        let read = tokio::spawn(async move { read_error_body(response, Some(&cancel)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        let joined = read.await.map_err(|e| err(e.to_string()))?;
        let err_value = expect_err(joined, "cancelled")?;
        assert!(matches!(err_value, AuthHttpError::Cancelled));
        assert_eq!(err_value.into_auth_error().to_string(), "Login cancelled");
        Ok(())
    }

    #[test]
    fn truncate_error_body_is_char_bounded() -> TestResult {
        let long = "😀".repeat(MAX_ERROR_BODY_CHARS + 3);
        let formatted = truncate_error_body(&long);
        assert!(formatted.contains("[truncated 3 chars]"));
        let prefix = formatted
            .split("... [truncated")
            .next()
            .ok_or_else(|| err("prefix"))?;
        let prefix_chars = prefix.chars().count();
        assert_eq!(prefix_chars, MAX_ERROR_BODY_CHARS);
        Ok(())
    }

    /// Send one sweeper-style `GET /` at a stub, draining its 404 reply.
    fn sweep_probe_stub(url: &str) -> Result<(), String> {
        let addr = url
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .ok_or_else(|| err("bad stub url"))?;
        let mut stream = TcpStream::connect(addr).map_err(|e| err(e.to_string()))?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: sweep\r\nConnection: close\r\n\r\n")
            .map_err(|e| err(e.to_string()))?;
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf);
        Ok(())
    }

    /// A localhost sweep (`GET /` at every new listener) must not consume
    /// the scripted stub reply: after probing the stub, the real POST still
    /// succeeds and returns its body.
    #[tokio::test]
    async fn sweep_get_does_not_consume_stub_scripts() -> TestResult {
        let url = spawn_raw_server("POST", "/", |mut stream| {
            let body = br#"{"access_token":"tok"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body)
            );
            let _ = stream.write_all(response.as_bytes());
        })?;
        sweep_probe_stub(&url)?;
        let client = AuthHttpClient::new().map_err(|e| err(e.to_string()))?;
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        fields.insert("grant_type".into(), "refresh_token".into());
        let body = client
            .post_json(&url, &fields, None, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert!(body.contains("access_token"));
        Ok(())
    }
}
