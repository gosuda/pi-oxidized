//! Streaming output accumulator with bounded memory and full-output spill.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/output-accumulator.ts`.
//! Chunks arrive as raw bytes, are decoded through a streaming UTF-8 decoder
//! (partial multi-byte sequences are held across chunk boundaries), and only a
//! rolling decoded tail is kept for display snapshots. Once the raw byte,
//! decoded byte, or line totals cross the configured limits, every raw byte —
//! including chunks buffered before the spill opened — is persisted to a temp
//! file of the form `{tmpdir}/{prefix}-{16 lowercase hex chars}.log` (bash
//! uses prefix `pi-bash`, yielding `pi-bash-{16hex}.log`).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use thiserror::Error;
use uuid::Uuid;

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    truncate_tail,
};

/// Default temp-file name prefix when the caller does not set one
/// (TypeScript `pi-output`; the bash tool passes `pi-bash`).
pub const DEFAULT_TEMP_FILE_PREFIX: &str = "pi-output";

/// Options for [`OutputAccumulator`] (TypeScript `OutputAccumulatorOptions`).
#[derive(Clone, Debug)]
pub struct OutputAccumulatorOptions {
    /// Maximum number of lines kept in a snapshot (default
    /// [`DEFAULT_MAX_LINES`]).
    pub max_lines: usize,
    /// Maximum number of decoded bytes kept in a snapshot and the spill
    /// threshold (default [`DEFAULT_MAX_BYTES`]).
    pub max_bytes: usize,
    /// Temp-file name prefix (default `pi-output`).
    pub temp_file_prefix: String,
}

impl Default for OutputAccumulatorOptions {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            temp_file_prefix: DEFAULT_TEMP_FILE_PREFIX.to_owned(),
        }
    }
}

/// A point-in-time view of the accumulated output (TypeScript
/// `OutputSnapshot`).
#[derive(Clone, Debug)]
pub struct OutputSnapshot {
    /// The tail-truncated display content.
    pub content: String,
    /// Truncation metadata against the configured limits; `total_lines` /
    /// `total_bytes` always describe the full stream, not the kept tail.
    pub truncation: TruncationResult,
    /// Path of the spill file holding the full raw output, when opened.
    pub full_output_path: Option<PathBuf>,
}

/// Errors produced by [`OutputAccumulator`].
#[derive(Debug, Error)]
pub enum OutputAccumulatorError {
    /// A chunk was appended after [`OutputAccumulator::finish`].
    #[error("Cannot append to a finished output accumulator")]
    Finished,
    /// Creating the spill file failed.
    #[error("failed to create full output spill file {path}: {source}")]
    SpillCreate {
        /// The spill path that could not be created.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// Writing a chunk to the spill file failed.
    #[error("failed to write full output spill file {path}: {source}")]
    SpillWrite {
        /// The spill path that could not be written.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

/// Incremental UTF-8 decoder matching `TextDecoder` with `stream: true`:
/// incomplete trailing sequences are buffered until more bytes arrive, and
/// invalid bytes become U+FFFD following the WHATWG maximal-subpart rule
/// (the same rule `std::str::from_utf8` uses for its error length).
#[derive(Default)]
struct StreamingUtf8Decoder {
    pending: Vec<u8>,
}

impl StreamingUtf8Decoder {
    fn decode(&mut self, data: &[u8], stream: bool) -> String {
        if self.pending.is_empty() && data.is_empty() {
            return String::new();
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);

        let mut out = String::new();
        let mut rest: &[u8] = &buf;
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    out.push_str(&String::from_utf8_lossy(&rest[..valid_up_to]));
                    rest = &rest[valid_up_to..];
                    if let Some(len) = error.error_len() {
                        out.push('\u{FFFD}');
                        rest = &rest[len..];
                    } else {
                        // Incomplete trailing sequence.
                        if stream {
                            self.pending = rest.to_vec();
                        } else {
                            // End-of-stream flush: one replacement char.
                            out.push('\u{FFFD}');
                        }
                        break;
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self) -> String {
        self.decode(&[], false)
    }
}

/// Build the default spill path `{tmpdir}/{prefix}-{16 hex}.log`
/// (TypeScript `randomBytes(8).toString("hex")`).
fn default_temp_file_path(prefix: &str) -> PathBuf {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = Uuid::new_v4().into_bytes();
    let mut id = String::with_capacity(16);
    for byte in &bytes[..8] {
        id.push(char::from(HEX[usize::from(byte >> 4)]));
        id.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    std::env::temp_dir().join(format!("{prefix}-{id}.log"))
}

/// Incrementally tracks streaming output with bounded memory.
///
/// The synchronous file writes mirror Node's fire-and-forget `WriteStream`
/// chunks; unlike Node, write failures surface as
/// [`OutputAccumulatorError`] instead of vanishing.
pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    temp_file_prefix: String,
    decoder: StreamingUtf8Decoder,
    raw_chunks: Vec<Vec<u8>>,
    tail_text: String,
    tail_bytes: usize,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    finished: bool,
    temp_file_path: Option<PathBuf>,
    temp_file: Option<File>,
}

impl OutputAccumulator {
    /// Create an accumulator with the given limits and spill prefix.
    #[must_use]
    pub fn new(options: OutputAccumulatorOptions) -> Self {
        let max_rolling_bytes = (options.max_bytes * 2).max(1);
        Self {
            max_lines: options.max_lines,
            max_bytes: options.max_bytes,
            max_rolling_bytes,
            temp_file_prefix: options.temp_file_prefix,
            decoder: StreamingUtf8Decoder::default(),
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_bytes: 0,
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            temp_file_path: None,
            temp_file: None,
        }
    }

    /// Append one raw chunk. When the spill file is open — or the stream just
    /// crossed a limit — the chunk is written through; otherwise it is
    /// buffered so a later spill still captures the complete output.
    ///
    /// # Errors
    ///
    /// Returns [`OutputAccumulatorError::Finished`] after
    /// [`Self::finish`], or the spill create/write failure.
    pub fn append(&mut self, data: &[u8]) -> Result<(), OutputAccumulatorError> {
        if self.finished {
            return Err(OutputAccumulatorError::Finished);
        }

        self.total_raw_bytes += data.len();
        let text = self.decoder.decode(data, true);
        self.append_decoded_text(&text);

        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file()?;
            if let (Some(file), Some(path)) =
                (self.temp_file.as_mut(), self.temp_file_path.as_ref())
            {
                file.write_all(data)
                    .map_err(|source| OutputAccumulatorError::SpillWrite {
                        path: path.clone(),
                        source,
                    })?;
            }
        } else if !data.is_empty() {
            self.raw_chunks.push(data.to_vec());
        }
        Ok(())
    }

    /// Flush the decoder and open the spill file if the stream crossed a
    /// limit on the final chunk.
    ///
    /// # Errors
    ///
    /// Returns the spill create failure.
    pub fn finish(&mut self) -> Result<(), OutputAccumulatorError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let text = self.decoder.finish();
        self.append_decoded_text(&text);
        if self.should_use_temp_file() {
            self.ensure_temp_file()?;
        }
        Ok(())
    }

    /// Produce a tail-truncated snapshot against the configured limits. With
    /// `persist_if_truncated`, a truncated stream guarantees the spill file
    /// is open and reported via [`OutputSnapshot::full_output_path`].
    ///
    /// # Errors
    ///
    /// Returns the spill create failure when persistence is requested.
    pub fn snapshot(
        &mut self,
        persist_if_truncated: bool,
    ) -> Result<OutputSnapshot, OutputAccumulatorError> {
        let tail_truncation = truncate_tail(
            self.snapshot_text(),
            TruncationOptions {
                max_lines: Some(self.max_lines),
                max_bytes: Some(self.max_bytes),
            },
        );
        let truncated =
            self.total_lines > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = if truncated {
            Some(tail_truncation.truncated_by.unwrap_or({
                if self.total_decoded_bytes > self.max_bytes {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }
            }))
        } else {
            None
        };
        let truncation = TruncationResult {
            content: tail_truncation.content,
            truncated,
            truncated_by,
            total_lines: self.total_lines,
            total_bytes: self.total_decoded_bytes,
            output_lines: tail_truncation.output_lines,
            output_bytes: tail_truncation.output_bytes,
            last_line_partial: tail_truncation.last_line_partial,
            first_line_exceeds_limit: tail_truncation.first_line_exceeds_limit,
            max_lines: self.max_lines,
            max_bytes: self.max_bytes,
        };

        if persist_if_truncated && truncation.truncated {
            self.ensure_temp_file()?;
        }

        Ok(OutputSnapshot {
            content: truncation.content.clone(),
            truncation,
            full_output_path: self.temp_file_path.clone(),
        })
    }

    /// Close the spill file handle, if any (TypeScript `closeTempFile`). The
    /// path is retained: already-written bytes stay the stream's full record.
    pub fn close_temp_file(&mut self) {
        self.temp_file = None;
    }

    /// Byte length of the current open (unterminated) line
    /// (TypeScript `getLastLineBytes`).
    #[must_use]
    pub fn last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    fn append_decoded_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let bytes = text.len();
        self.total_decoded_bytes += bytes;
        self.tail_text.push_str(text);
        self.tail_bytes += bytes;
        if self.tail_bytes > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }

        let mut newlines = 0_usize;
        let mut last_newline = 0_usize;
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                newlines += 1;
                last_newline = index;
            }
        }
        if newlines == 0 {
            self.current_line_bytes += bytes;
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let tail = &text[last_newline + 1..];
            self.current_line_bytes = tail.len();
            self.has_open_line = !tail.is_empty();
        }
        self.total_lines = self.completed_lines + usize::from(self.has_open_line);
    }

    fn trim_tail(&mut self) {
        let bytes = self.tail_text.as_bytes();
        if bytes.len() <= self.max_rolling_bytes {
            self.tail_bytes = bytes.len();
            return;
        }

        let mut start = bytes.len() - self.max_rolling_bytes;
        while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
            start += 1;
        }
        // TypeScript keeps the previous flag when start == 0 (unreachable
        // here because bytes.len() > maxRollingBytes); otherwise the tail
        // starts at a line boundary exactly when the preceding byte was a
        // newline.
        if start > 0 {
            self.tail_starts_at_line_boundary = bytes[start - 1] == 0x0a;
        }
        self.tail_text = String::from_utf8_lossy(&bytes[start..]).into_owned();
        self.tail_bytes = self.tail_text.len();
    }

    fn snapshot_text(&self) -> &str {
        if self.tail_starts_at_line_boundary {
            return &self.tail_text;
        }
        match self.tail_text.find('\n') {
            Some(index) => &self.tail_text[index + 1..],
            None => &self.tail_text,
        }
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    fn ensure_temp_file(&mut self) -> Result<(), OutputAccumulatorError> {
        if self.temp_file_path.is_some() {
            return Ok(());
        }
        let path = default_temp_file_path(&self.temp_file_prefix);
        let mut file =
            File::create(&path).map_err(|source| OutputAccumulatorError::SpillCreate {
                path: path.clone(),
                source,
            })?;
        let chunks = std::mem::take(&mut self.raw_chunks);
        // Open the stream first (mirrors Node createWriteStream), then flush
        // every chunk buffered before the spill threshold was crossed.
        self.temp_file_path = Some(path.clone());
        for chunk in &chunks {
            file.write_all(chunk)
                .map_err(|source| OutputAccumulatorError::SpillWrite {
                    path: path.clone(),
                    source,
                })?;
        }
        self.temp_file = Some(file);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::path::Path;

    type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

    fn small_options() -> OutputAccumulatorOptions {
        OutputAccumulatorOptions {
            max_lines: 4,
            max_bytes: 16,
            temp_file_prefix: "pi-bash".to_owned(),
        }
    }

    fn cleanup(path: Option<&Path>) -> TestResult {
        let Some(path) = path else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }

    fn check_spill_name(path: &Path, prefix: &str) -> TestResult {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Err("spill path should have a UTF-8 file name".into());
        };
        let stem = name
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(".log"));
        match stem {
            Some(id)
                if id.len() == 16
                    && id
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) =>
            {
                Ok(())
            }
            _ => Err(format!("spill name {name} is not {prefix}-{{16 lowercase hex}}.log").into()),
        }
    }

    #[test]
    fn small_output_has_no_spill_and_full_content() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(b"hello\nworld\n")?;
        acc.finish()?;
        let snapshot = acc.snapshot(false)?;
        assert!(!snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.truncated_by, None);
        assert_eq!(snapshot.content, "hello\nworld\n");
        assert_eq!(snapshot.truncation.total_lines, 2);
        assert_eq!(snapshot.truncation.total_bytes, 12);
        assert_eq!(snapshot.full_output_path, None);
        Ok(())
    }

    #[test]
    fn crossing_byte_limit_spills_and_preserves_every_raw_byte() -> TestResult {
        let mut acc = OutputAccumulator::new(small_options());
        // First chunk stays under 16 bytes and is only buffered.
        acc.append(b"abcdefghij")?;
        // Second chunk pushes raw bytes to 17 > 16: the spill opens and must
        // capture the earlier buffered chunk too.
        acc.append(b"klmnopq")?;
        let snapshot = acc.snapshot(true)?;
        let path = snapshot.full_output_path.clone().ok_or_else(|| {
            std::io::Error::other("truncated snapshot should report the spill path")
        })?;
        check_spill_name(&path, "pi-bash")?;
        let persisted = std::fs::read(&path)?;
        assert_eq!(persisted, b"abcdefghijklmnopq");

        assert!(snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(snapshot.truncation.total_bytes, 17);
        assert_eq!(snapshot.truncation.max_bytes, 16);
        assert!(snapshot.truncation.output_bytes <= 16);
        acc.close_temp_file();
        cleanup(snapshot.full_output_path.as_deref())
    }

    #[test]
    fn crossing_line_limit_spills_and_tail_truncates() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 2,
            max_bytes: 1024 * 1024,
            temp_file_prefix: "pi-bash".to_owned(),
        });
        acc.append(b"a\nb\nc\n")?;
        acc.finish()?;
        let snapshot = acc.snapshot(true)?;
        assert!(snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.total_lines, 3);
        assert_eq!(snapshot.truncation.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(snapshot.content, "b\nc");
        let path = snapshot.full_output_path.clone().ok_or_else(|| {
            std::io::Error::other("spill should open once lines exceed the limit")
        })?;
        assert_eq!(std::fs::read(&path)?, b"a\nb\nc\n");
        acc.close_temp_file();
        cleanup(snapshot.full_output_path.as_deref())
    }

    #[test]
    fn snapshot_without_persist_does_not_open_spill_under_limits() -> TestResult {
        let mut acc = OutputAccumulator::new(small_options());
        acc.append(b"tiny")?;
        acc.finish()?;
        let snapshot = acc.snapshot(true)?;
        assert!(!snapshot.truncation.truncated);
        assert_eq!(snapshot.full_output_path, None);
        Ok(())
    }

    #[test]
    fn streaming_decoder_holds_partial_utf8_across_chunks() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(&[0xC3])?;
        acc.append(&[0xA9])?;
        acc.finish()?;
        let snapshot = acc.snapshot(false)?;
        assert_eq!(snapshot.content, "é");
        assert_eq!(snapshot.truncation.total_bytes, 2);
        assert!(!snapshot.content.contains('\u{FFFD}'));
        Ok(())
    }

    #[test]
    fn invalid_bytes_become_replacement_chars() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(&[0xFF, 0xFE])?;
        acc.finish()?;
        let snapshot = acc.snapshot(false)?;
        assert_eq!(snapshot.content, "\u{FFFD}\u{FFFD}");
        // Each invalid byte decodes to one 3-byte U+FFFD.
        assert_eq!(snapshot.truncation.total_bytes, 6);
        Ok(())
    }

    #[test]
    fn dangling_multibyte_tail_flushes_single_replacement_on_finish() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(&[0x61, 0xC3])?;
        acc.finish()?;
        let snapshot = acc.snapshot(false)?;
        assert_eq!(snapshot.content, "a\u{FFFD}");
        Ok(())
    }

    #[test]
    fn append_after_finish_is_rejected_with_exact_message() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(b"x")?;
        acc.finish()?;
        match acc.append(b"y") {
            Err(OutputAccumulatorError::Finished) => {}
            other => return Err(format!("expected Finished error, got {other:?}").into()),
        }
        assert_eq!(
            OutputAccumulatorError::Finished.to_string(),
            "Cannot append to a finished output accumulator"
        );
        Ok(())
    }

    #[test]
    fn last_line_bytes_tracks_open_line() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(b"ab\ncde")?;
        assert_eq!(acc.last_line_bytes(), 3);
        acc.append(b"f\ng")?;
        assert_eq!(acc.last_line_bytes(), 1);
        acc.append(b"\n")?;
        assert_eq!(acc.last_line_bytes(), 0);
        Ok(())
    }

    #[test]
    fn rolling_tail_is_bounded_while_totals_track_full_stream() -> TestResult {
        let mut acc = OutputAccumulator::new(small_options());
        let mut full = Vec::new();
        for index in 0..20 {
            let chunk = format!("line-{index:02}\n");
            full.extend_from_slice(chunk.as_bytes());
            acc.append(chunk.as_bytes())?;
        }
        acc.finish()?;
        let snapshot = acc.snapshot(true)?;
        // Limits: 4 lines / 16 bytes; totals describe all 160 bytes.
        assert_eq!(snapshot.truncation.total_lines, 20);
        assert_eq!(snapshot.truncation.total_bytes, 160);
        assert!(snapshot.truncation.truncated);
        assert!(snapshot.truncation.output_bytes <= 16);
        assert!(snapshot.truncation.output_lines <= 4);
        let path = snapshot
            .full_output_path
            .clone()
            .ok_or_else(|| std::io::Error::other("long stream should spill"))?;
        assert_eq!(std::fs::read(&path)?, full);
        acc.close_temp_file();
        cleanup(snapshot.full_output_path.as_deref())
    }

    #[test]
    fn snapshot_drops_partial_first_line_of_rolling_tail() -> TestResult {
        // 150 x's + "\n" + 10 y's; rolling keeps 32 bytes, landing mid-x-run,
        // so the snapshot text starts after the tail's first newline.
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 100,
            max_bytes: 16,
            temp_file_prefix: "pi-output".to_owned(),
        });
        let mut data = vec![b'x'; 150];
        data.push(b'\n');
        data.extend_from_slice(b"yyyyyyyyyy");
        acc.append(&data)?;
        acc.finish()?;
        let snapshot = acc.snapshot(false)?;
        assert_eq!(snapshot.content, "yyyyyyyyyy");
        assert!(snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.truncated_by, Some(TruncatedBy::Bytes));
        acc.close_temp_file();
        cleanup(snapshot.full_output_path.as_deref())
    }

    #[test]
    fn default_prefix_names_pi_output_files() -> TestResult {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 1,
            max_bytes: 4,
            temp_file_prefix: DEFAULT_TEMP_FILE_PREFIX.to_owned(),
        });
        acc.append(b"more than four bytes\nand lines")?;
        acc.finish()?;
        let snapshot = acc.snapshot(true)?;
        let path = snapshot
            .full_output_path
            .clone()
            .ok_or_else(|| std::io::Error::other("spill should open"))?;
        check_spill_name(&path, "pi-output")?;
        acc.close_temp_file();
        cleanup(snapshot.full_output_path.as_deref())
    }

    #[test]
    fn close_temp_file_is_idempotent() -> TestResult {
        let mut acc = OutputAccumulator::new(small_options());
        acc.append(b"this stream definitely exceeds sixteen bytes")?;
        acc.finish()?;
        let snapshot = acc.snapshot(true)?;
        acc.close_temp_file();
        acc.close_temp_file();
        // The path is still reported after the handle closes.
        assert!(snapshot.full_output_path.is_some());
        cleanup(snapshot.full_output_path.as_deref())
    }
}
