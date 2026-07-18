//! Shared in-process fake-host harness for `pi-ext` tests.
//!
//! Provides a [`FakeHost`] speaking the JSONL protocol over `tokio::io::duplex`
//! pairs and a [`make_pair`] builder that wires a [`crate::client::HostClient`]
//! to it. Used by the `client`, `adapters`, and `sanitize` contract tests.

use std::error::Error;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::HostClient;
use crate::protocol::{
    Frame, FrameKind, HelloAck, Method, decode_frame_str, encode_frame, to_payload,
};

/// In-process fake host reading client frames and writing host frames.
pub struct FakeHost {
    /// Reads frames the client sent (client → host direction).
    pub read: BufReader<tokio::io::DuplexStream>,
    /// Writes frames back to the client (host → client direction).
    pub write: tokio::io::DuplexStream,
}

impl FakeHost {
    /// Read one complete frame line the client sent.
    ///
    /// Returns `None` on EOF or decode error.
    pub async fn read_frame(&mut self) -> Option<Frame> {
        let mut line = String::new();
        match self.read.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => decode_frame_str(line.trim_end()).ok(),
        }
    }

    /// Read one frame, returning an error if absent.
    ///
    /// # Errors
    ///
    /// Returns an io error when the stream ended before a full frame arrived.
    pub async fn require_frame(&mut self, label: &str) -> std::io::Result<Frame> {
        match self.read_frame().await {
            Some(frame) => Ok(frame),
            None => Err(std::io::Error::other(format!(
                "fake host expected a frame ({label}) but got EOF"
            ))),
        }
    }

    /// Write a frame to the client.
    ///
    /// # Errors
    ///
    /// Returns the underlying io error on write/encode failure.
    pub async fn write_frame(&mut self, frame: &Frame) -> Result<(), Box<dyn Error>> {
        let bytes = encode_frame(frame).map_err(|e| std::io::Error::other(e.to_string()))?;
        self.write.write_all(&bytes).await?;
        self.write.flush().await?;
        Ok(())
    }

    /// Answer a `hello` request with a local acknowledgment.
    pub async fn answer_hello(&mut self) -> Result<(), Box<dyn Error>> {
        let req = self.require_frame("hello").await?;
        assert_eq!(req.kind, FrameKind::Req);
        assert_eq!(req.method, Method::Hello.as_str());
        let ack = Frame::response(req.id, Method::Hello, to_payload(&HelloAck::local())?);
        self.write_frame(&ack).await
    }

    /// Close the host → client write half, simulating host EOF.
    pub async fn close(mut self) {
        let _ = self.write.shutdown().await;
    }
}

/// Build a client/fake-host pair backed by in-memory duplex pipes.
pub async fn make_pair() -> (HostClient, FakeHost) {
    let (client_to_host, host_from_client) = tokio::io::duplex(64 * 1024);
    let (host_to_client, client_from_host) = tokio::io::duplex(64 * 1024);
    let (client_err, _host_err) = tokio::io::duplex(4096);
    let client = HostClient::connect_boxed(
        Box::new(client_to_host),
        Box::new(client_from_host),
        Box::new(client_err),
        None,
    );
    let host = FakeHost {
        read: BufReader::new(host_from_client),
        write: host_to_client,
    };
    (client, host)
}
