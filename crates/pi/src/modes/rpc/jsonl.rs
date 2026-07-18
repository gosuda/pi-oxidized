//! Strict LF-only JSONL framing for RPC stdin/stdout.
//!
//! Port of `.references/pi/packages/coding-agent/src/modes/rpc/jsonl.ts`.
//!
//! Framing rules:
//! - records are split on `\n` only (not `\r\n` as a unit, not U+2028/U+2029)
//! - a single trailing `\r` is stripped from each emitted line
//! - payload strings may contain U+2028 / U+2029 raw (`serde_json` matches JS)
//! - incomplete UTF-8 sequences are held across reads; invalid sequences become U+FFFD
//! - on EOF, a non-empty residual buffer is emitted as a final line

use std::io;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Serialize a single strict JSONL record (JSON object + trailing LF).
///
/// `serde_json` emits U+2028 and U+2029 raw inside strings, matching
/// JavaScript `JSON.stringify` (not escaped as `\u2028` / `\u2029`).
///
/// # Errors
///
/// Returns any error from `serde_json::to_string`.
pub fn serialize_json_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Errors while reading the underlying stream.
#[derive(Debug, thiserror::Error)]
pub enum JsonlReadError {
    /// Underlying `AsyncRead` failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Incremental UTF-8 JSONL line reader over an [`AsyncRead`].
///
/// Splits on LF only, strips one trailing CR per line, and flushes a residual
/// unterminated line on EOF when non-empty.
pub struct JsonlLineReader<R> {
    reader: R,
    /// Undecoded trailing bytes of an incomplete UTF-8 sequence.
    pending_bytes: Vec<u8>,
    /// Decoded text waiting for a complete LF-terminated line (or EOF flush).
    decoded: String,
    /// True after the underlying reader returned `Ok(0)`.
    eof: bool,
    /// Scratch buffer for `AsyncRead` chunks.
    read_buf: Vec<u8>,
}

impl<R> JsonlLineReader<R>
where
    R: AsyncRead + Unpin,
{
    /// Create a reader over `reader` with an 8 KiB I/O scratch buffer.
    #[must_use]
    pub fn new(reader: R) -> Self {
        Self::with_capacity(reader, 8 * 1024)
    }

    /// Create a reader with an explicit I/O chunk capacity.
    #[must_use]
    pub fn with_capacity(reader: R, capacity: usize) -> Self {
        Self {
            reader,
            pending_bytes: Vec::new(),
            decoded: String::new(),
            eof: false,
            read_buf: vec![0_u8; capacity.max(1)],
        }
    }

    /// Read the next JSONL line, or `None` when the stream is exhausted.
    ///
    /// Empty lines (a bare `\n`) are returned as empty strings. A final
    /// residual without a trailing LF is returned once on EOF when non-empty.
    ///
    /// # Errors
    ///
    /// Returns [`JsonlReadError::Io`] when the underlying reader fails.
    pub async fn next_line(&mut self) -> Result<Option<String>, JsonlReadError> {
        loop {
            if let Some(line) = take_line(&mut self.decoded) {
                return Ok(Some(line));
            }

            if self.eof {
                if self.decoded.is_empty() {
                    return Ok(None);
                }
                let residual = std::mem::take(&mut self.decoded);
                return Ok(Some(strip_trailing_cr(residual)));
            }

            let n = self.reader.read(&mut self.read_buf).await?;
            if n == 0 {
                // Incomplete multi-byte sequence at EOF → U+FFFD (StringDecoder).
                if !self.pending_bytes.is_empty() {
                    self.decoded.push('\u{FFFD}');
                    self.pending_bytes.clear();
                }
                self.eof = true;
                continue;
            }

            decode_utf8_incremental(
                &mut self.pending_bytes,
                &self.read_buf[..n],
                &mut self.decoded,
            );
        }
    }

    /// Consume the reader, returning the inner `AsyncRead`.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.reader
    }
}

fn take_line(decoded: &mut String) -> Option<String> {
    let newline_index = decoded.find('\n')?;
    let mut line = decoded.drain(..=newline_index).collect::<String>();
    // Drop the trailing LF that was drained with the line.
    line.pop();
    Some(strip_trailing_cr(line))
}

fn strip_trailing_cr(mut line: String) -> String {
    if line.ends_with('\r') {
        line.pop();
    }
    line
}

/// Decode `chunk` as UTF-8, holding incomplete trailing sequences in `pending`
/// and replacing invalid sequences with U+FFFD (Node `StringDecoder` parity).
fn decode_utf8_incremental(pending: &mut Vec<u8>, chunk: &[u8], out: &mut String) {
    pending.extend_from_slice(chunk);

    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                return;
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to > 0 {
                    push_valid_utf8(out, &pending[..valid_up_to]);
                    pending.drain(..valid_up_to);
                    continue;
                }

                match err.error_len() {
                    Some(len) => {
                        out.push('\u{FFFD}');
                        let drain_len = len.min(pending.len());
                        pending.drain(..drain_len);
                    }
                    None => {
                        // Incomplete multi-byte sequence at the end of input.
                        return;
                    }
                }
            }
        }
    }
}

fn push_valid_utf8(out: &mut String, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(s) => out.push_str(s),
        // `valid_up_to` guarantees this slice is UTF-8; fall back conservatively.
        Err(_) => out.push_str(&String::from_utf8_lossy(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    /// `AsyncRead` over a list of chunks (simulates fragmented delivery).
    struct ChunkedRead {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl ChunkedRead {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self { chunks, index: 0 }
        }
    }

    impl AsyncRead for ChunkedRead {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.index >= self.chunks.len() {
                return Poll::Ready(Ok(()));
            }
            let index = self.index;
            let chunk = std::mem::take(&mut self.chunks[index]);
            let n = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk[..n]);
            if n == chunk.len() {
                self.index += 1;
            } else {
                self.chunks[index] = chunk[n..].to_vec();
            }
            Poll::Ready(Ok(()))
        }
    }

    async fn collect_lines<R: AsyncRead + Unpin>(reader: R) -> Result<Vec<String>, String> {
        let mut jsonl = JsonlLineReader::new(reader);
        let mut lines = Vec::new();
        loop {
            match jsonl.next_line().await {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => return Ok(lines),
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    #[test]
    fn serializes_strict_jsonl_without_escaping_unicode_separators() -> TestResult {
        let line = serialize_json_line(&json!({"text": "a\u{2028}b\u{2029}c"}))
            .map_err(|e| err(e.to_string()))?;
        if !line.contains('\u{2028}') {
            return Err(err(format!("U+2028 must be raw: {line}")));
        }
        if !line.contains('\u{2029}') {
            return Err(err(format!("U+2029 must be raw: {line}")));
        }
        if line.contains("\\u2028") {
            return Err(err(format!("must not escape U+2028: {line}")));
        }
        if line.contains("\\u2029") {
            return Err(err(format!("must not escape U+2029: {line}")));
        }
        if !line.ends_with('\n') {
            return Err(err("line must end with LF"));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim_end_matches('\n')).map_err(|e| err(e.to_string()))?;
        if parsed != json!({"text": "a\u{2028}b\u{2029}c"}) {
            return Err(err(format!("unexpected parsed value: {parsed}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn splits_on_lf_only_and_preserves_unicode_separators() -> TestResult {
        let payload = serialize_json_line(&json!({"text": "a\u{2028}b\u{2029}c"}))
            .map_err(|e| err(e.to_string()))?;
        let lines = collect_lines(Cursor::new(payload.into_bytes())).await?;
        if lines.len() != 1 {
            return Err(err(format!("expected 1 line, got {}", lines.len())));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&lines[0]).map_err(|e| err(e.to_string()))?;
        if parsed != json!({"text": "a\u{2028}b\u{2029}c"}) {
            return Err(err(format!("unexpected parsed value: {parsed}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn handles_crlf_delimited_input() -> TestResult {
        let input = b"{\"a\":1}\r\n{\"b\":2}\r\n";
        let lines = collect_lines(Cursor::new(input.to_vec())).await?;
        let expected = vec![r#"{"a":1}"#.to_owned(), r#"{"b":2}"#.to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn emits_final_line_without_trailing_lf() -> TestResult {
        let input = br#"{"a":1}"#;
        let lines = collect_lines(Cursor::new(input.to_vec())).await?;
        let expected = vec![r#"{"a":1}"#.to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn does_not_split_on_unicode_line_separators_alone() -> TestResult {
        let input = "{\"text\":\"a\u{2028}b\u{2029}c\"}";
        let lines = collect_lines(Cursor::new(input.as_bytes().to_vec())).await?;
        if lines.len() != 1 {
            return Err(err(format!("expected 1 line, got {}", lines.len())));
        }
        if lines[0] != input {
            return Err(err(format!("unexpected line: {}", lines[0])));
        }
        Ok(())
    }

    #[tokio::test]
    async fn strips_only_one_trailing_cr() -> TestResult {
        let input = b"abc\r\r\n";
        let lines = collect_lines(Cursor::new(input.to_vec())).await?;
        let expected = vec!["abc\r".to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn empty_stream_yields_no_lines() -> TestResult {
        let lines = collect_lines(Cursor::new(Vec::<u8>::new())).await?;
        if !lines.is_empty() {
            return Err(err(format!("expected no lines, got {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn bare_lf_emits_empty_line() -> TestResult {
        let lines = collect_lines(Cursor::new(b"\n".to_vec())).await?;
        let expected = vec![String::new()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn fragmented_chunks_across_newline() -> TestResult {
        let chunks = vec![b"{\"a\":".to_vec(), b"1}\n{\"b\":2}".to_vec()];
        let lines = collect_lines(ChunkedRead::new(chunks)).await?;
        let expected = vec![r#"{"a":1}"#.to_owned(), r#"{"b":2}"#.to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn fragmented_multibyte_utf8_across_chunks() -> TestResult {
        // U+2028 is E2 80 A8 — split mid-sequence.
        let chunks = vec![
            b"{\"t\":\"".to_vec(),
            vec![0xE2],
            vec![0x80, 0xA8],
            b"\"}\n".to_vec(),
        ];
        let lines = collect_lines(ChunkedRead::new(chunks)).await?;
        if lines.len() != 1 {
            return Err(err(format!("expected 1 line, got {}", lines.len())));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&lines[0]).map_err(|e| err(e.to_string()))?;
        if parsed["t"].as_str() != Some("\u{2028}") {
            return Err(err(format!("unexpected t: {}", parsed["t"])));
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_utf8_becomes_replacement_character() -> TestResult {
        let input = vec![b'a', 0xFF, b'b', b'\n'];
        let lines = collect_lines(Cursor::new(input)).await?;
        let expected = vec!["a\u{FFFD}b".to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_utf8_at_eof_becomes_replacement() -> TestResult {
        let input = vec![0xE2];
        let lines = collect_lines(Cursor::new(input)).await?;
        let expected = vec!["\u{FFFD}".to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn multiple_lines_in_single_chunk() -> TestResult {
        let input = b"one\ntwo\nthree\n";
        let lines = collect_lines(Cursor::new(input.to_vec())).await?;
        let expected = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn residual_cr_only_line_at_eof() -> TestResult {
        // Trailing CR without LF is stripped on the residual flush.
        let input = b"hello\r";
        let lines = collect_lines(Cursor::new(input.to_vec())).await?;
        let expected = vec!["hello".to_owned()];
        if lines != expected {
            return Err(err(format!("unexpected lines: {lines:?}")));
        }
        Ok(())
    }
}
