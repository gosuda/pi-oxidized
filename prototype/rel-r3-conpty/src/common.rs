//! Host-agnostic scaffold shared by the Windows witness: JSONL transcript,
//! reader pump with quiescence settling, VT frame decode, byte scanning.
//!
//! Compiles on every host so `cargo check` here covers it; no CLI printing
//! lives in this module (diagnostics are transcript events).

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use sonic_rs::{Value, json};

/// One chunk from the reader pump; `Eof` marks pipe close or reader failure.
pub enum Chunk {
    Bytes(Vec<u8>),
    Eof,
}

/// Background reader draining the ConPTY master into a channel.
pub struct Pump {
    rx: Receiver<Chunk>,
    handle: Option<JoinHandle<()>>,
}

/// Spawns the pump thread for a cloned master reader.
///
/// # Errors
/// Returns the spawn failure verbatim.
pub fn spawn_pump(mut reader: Box<dyn Read + Send>) -> io::Result<Pump> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = thread::Builder::new()
        .name("rel-r3-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Chunk::Eof);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Chunk::Bytes(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        let _ = tx.send(Chunk::Eof);
                        break;
                    }
                }
            }
        })?;
    Ok(Pump {
        rx,
        handle: Some(handle),
    })
}

impl Pump {
    /// Collects output until `idle` passes with no new bytes, or `deadline`
    /// elapses, or the pipe closes. Returns the bytes plus EOF status.
    pub fn settle(
        &mut self,
        idle: std::time::Duration,
        deadline: std::time::Duration,
    ) -> (Vec<u8>, bool) {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut last = Instant::now();
        loop {
            if !out.is_empty() && last.elapsed() >= idle {
                return (out, false);
            }
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                return (out, false);
            }
            let wait = idle.min(deadline - elapsed);
            match self.rx.recv_timeout(wait) {
                Ok(Chunk::Bytes(b)) => {
                    if !b.is_empty() {
                        last = Instant::now();
                        out.extend_from_slice(&b);
                    }
                }
                Ok(Chunk::Eof) => return (out, true),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return (out, true),
            }
        }
    }

    /// Waits for pipe EOF, draining trailing bytes, up to `deadline`.
    pub fn wait_eof(&mut self, deadline: std::time::Duration) -> (Vec<u8>, bool) {
        let start = Instant::now();
        let mut out = Vec::new();
        while start.elapsed() < deadline {
            match self.rx.recv_timeout(deadline - start.elapsed()) {
                Ok(Chunk::Bytes(b)) => out.extend_from_slice(&b),
                Ok(Chunk::Eof) | Err(RecvTimeoutError::Disconnected) => return (out, true),
                Err(RecvTimeoutError::Timeout) => return (out, false),
            }
        }
        (out, false)
    }

    /// Joins the pump thread (after EOF).
    pub fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Deterministic JSONL transcript: sequence numbers and monotonic
/// milliseconds only; raw bytes recorded lossily (escaping via sonic-rs).
pub struct Transcript {
    seq: u64,
    t0: Instant,
    file: File,
    io_failed: bool,
}

impl Transcript {
    /// Creates (truncates) the transcript file.
    ///
    /// # Errors
    /// Propagates file-creation failure.
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            seq: 0,
            t0: Instant::now(),
            file: File::create(path)?,
            io_failed: false,
        })
    }

    /// Appends one event line; caller fields nest under `"fields"`.
    ///
    /// # Errors
    /// Propagates write or serialization failure.
    pub fn event(&mut self, kind: &str, fields: Value) -> io::Result<()> {
        self.seq += 1;
        let line = json!({
            "seq": self.seq,
            "t_ms": self.t0.elapsed().as_millis() as u64,
            "kind": kind,
            "fields": fields,
        });
        let mut bytes = sonic_rs::to_vec(&line).map_err(|err| io::Error::other(err.to_string()))?;
        bytes.push(b'\n');
        let result = self.file.write_all(&bytes);
        if result.is_err() {
            self.io_failed = true;
        }
        result
    }

    /// Whether any transcript write failed (gates the verdict exit code).
    pub fn io_failed(&self) -> bool {
        self.io_failed
    }

    /// Bounded lossy prefix for compact events.
    pub fn head(bytes: &[u8], max: usize) -> String {
        let cut = bytes.len().min(max);
        String::from_utf8_lossy(&bytes[..cut]).into_owned()
    }
}

/// Decodes the whole raw stream at one geometry into visible text lines,
/// mirroring the testkit's `snapshot_from_raw`: active-buffer lines first
/// (alt-screen aware), main-buffer fallback when the active view is blank.
pub fn frame(log: &[u8], cols: u16, rows: u16) -> Vec<String> {
    let mut vt = avt::Vt::builder()
        .size(usize::from(cols.max(1)), usize::from(rows.max(1)))
        .scrollback_limit(10_000)
        .build();
    let _ = vt.feed_str(&String::from_utf8_lossy(log));
    let mut lines: Vec<String> = vt.lines().map(|l| l.text().trim_end().to_owned()).collect();
    if lines.iter().all(|l| l.trim().is_empty()) {
        lines = vt
            .text()
            .into_iter()
            .map(|l| l.trim_end().to_owned())
            .collect();
    }
    lines
}

/// Counts matches at every byte offset (overlapping); the escape-sequence
/// needles used here cannot self-overlap, so this equals a non-overlap count.
pub fn count_seq(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// True when any decoded line contains `marker`.
pub fn any_line_contains(lines: &[String], marker: &str) -> bool {
    lines.iter().any(|l| l.contains(marker))
}

/// Joins non-blank trimmed lines for compact transcript events.
pub fn visible_text(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}
