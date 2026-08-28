//! 4-byte big-endian length-prefix framing — portable port of upstream
//! `framing.ts`.
//!
//! Every frame is `[u32 BE length][payload]`. The default upper bound for one
//! payload is 16 MiB (`DEFAULT_MAX_FRAME_LENGTH`). [`FrameDecoder`] splits
//! arbitrary byte chunks into complete payloads incrementally; it never panics
//! and surfaces every failure as a typed [`FrameError`].

/// Header length in bytes (one unsigned 32-bit big-endian integer).
pub const FRAME_HEADER_LENGTH: usize = 4;

/// Default upper bound for a single framed payload (16 MiB).
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

const MAX_UINT32: u64 = 0xffff_ffff;

/// Typed framing error. This is one of the two error types the remote layer
/// can originate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The byte buffer does not contain a complete 4-byte length prefix.
    #[error("Frame does not contain a complete length prefix")]
    IncompleteHeader,
    /// The declared frame length exceeds the configured limit.
    #[error("Frame length {declared} exceeds configured limit of {limit}")]
    Oversized { declared: u64, limit: u64 },
    /// The buffer does not contain exactly one complete payload.
    #[error("Frame must contain exactly one complete payload")]
    NotOneCompletePayload,
    /// `push` was called after `end` or after a prior failure.
    #[error("Frame decoder has ended")]
    Ended,
    /// `push` was called after the decoder entered the failed state.
    #[error("Frame decoder has failed")]
    Failed,
    /// `end` was called with a partial frame still buffered.
    #[error("Truncated frame at end of stream")]
    Truncated,
    /// `max_frame_length` is not a valid u32.
    #[error("max_frame_length must be an integer between 0 and {0}")]
    InvalidLimit(u64),
}

/// Options for frame decoding.
#[derive(Debug, Clone, Copy)]
pub struct FrameDecoderOptions {
    /// Maximum accepted payload length. Defaults to [`DEFAULT_MAX_FRAME_LENGTH`].
    pub max_frame_length: usize,
}

impl FrameDecoderOptions {
    /// Default options (16 MiB max).
    #[must_use]
    pub const fn default() -> Self {
        Self {
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
        }
    }
    /// Create options with a custom max frame length.
    pub fn with_max(max_frame_length: usize) -> Result<Self, FrameError> {
        if u64::try_from(max_frame_length).is_ok_and(|v| v <= MAX_UINT32) {
            Ok(Self { max_frame_length })
        } else {
            Err(FrameError::InvalidLimit(max_frame_length as u64))
        }
    }
}

impl Default for FrameDecoderOptions {
    fn default() -> Self {
        Self::default()
    }
}

fn resolve_max(options: Option<FrameDecoderOptions>) -> Result<usize, FrameError> {
    let max = options.map_or(DEFAULT_MAX_FRAME_LENGTH, |o| o.max_frame_length);
    if u64::try_from(max).is_ok_and(|v| v <= MAX_UINT32) {
        Ok(max)
    } else {
        Err(FrameError::InvalidLimit(max as u64))
    }
}

/// Prefixes a payload with its unsigned 32-bit big-endian byte length.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut frame = Vec::with_capacity(FRAME_HEADER_LENGTH + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Validates that `frame` contains exactly one complete frame within the configured limit.
pub fn assert_complete_frame(
    frame: &[u8],
    options: Option<FrameDecoderOptions>,
) -> Result<(), FrameError> {
    if frame.len() < FRAME_HEADER_LENGTH {
        return Err(FrameError::IncompleteHeader);
    }
    let length = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as u64;
    let max = resolve_max(options)? as u64;
    if length > max {
        return Err(FrameError::Oversized {
            declared: length,
            limit: max,
        });
    }
    if frame.len() != FRAME_HEADER_LENGTH + length as usize {
        return Err(FrameError::NotOneCompletePayload);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    Open,
    Ended,
    Failed,
}

/// Incrementally splits arbitrary byte chunks into length-prefixed payloads.
#[derive(Debug)]
pub struct FrameDecoder {
    header: [u8; FRAME_HEADER_LENGTH],
    header_len: usize,
    max_frame_length: usize,
    payload: Vec<u8>,
    expected_payload_len: Option<usize>,
    state: DecoderState,
}

impl FrameDecoder {
    /// Create a decoder with the given options.
    pub fn new(options: Option<FrameDecoderOptions>) -> Result<Self, FrameError> {
        Ok(Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_len: 0,
            max_frame_length: resolve_max(options)?,
            payload: Vec::new(),
            expected_payload_len: None,
            state: DecoderState::Open,
        })
    }
    /// Create a decoder with default options (16 MiB max).
    #[must_use]
    pub fn default() -> Self {
        Self::new(None).unwrap_or_else(|_| Self {
            header: [0; FRAME_HEADER_LENGTH],
            header_len: 0,
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
            payload: Vec::new(),
            expected_payload_len: None,
            state: DecoderState::Open,
        })
    }
    /// Feed a chunk; returns all complete payloads decoded from it.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        match self.state {
            DecoderState::Ended => return Err(FrameError::Ended),
            DecoderState::Failed => return Err(FrameError::Failed),
            DecoderState::Open => {}
        }
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            if self.expected_payload_len.is_none() {
                let needed = FRAME_HEADER_LENGTH - self.header_len;
                let avail = chunk.len() - offset;
                let take = needed.min(avail);
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&chunk[offset..offset + take]);
                self.header_len += take;
                offset += take;
                if self.header_len < FRAME_HEADER_LENGTH {
                    continue;
                }
                let frame_length = u32::from_be_bytes(self.header) as u64;
                self.header_len = 0;
                if frame_length > self.max_frame_length as u64 {
                    self.fail();
                    return Err(FrameError::Oversized {
                        declared: frame_length,
                        limit: self.max_frame_length as u64,
                    });
                }
                if frame_length == 0 {
                    frames.push(Vec::new());
                    continue;
                }
                let len = frame_length as usize;
                self.expected_payload_len = Some(len);
                self.payload.clear();
                self.payload.reserve(len);
            }
            let expected = self.expected_payload_len.expect("checked above");
            let avail = chunk.len() - offset;
            let remaining = expected - self.payload.len();
            let take = avail.min(remaining);
            self.payload
                .extend_from_slice(&chunk[offset..offset + take]);
            offset += take;
            if self.payload.len() == expected {
                frames.push(std::mem::take(&mut self.payload));
                self.expected_payload_len = None;
            }
        }
        Ok(frames)
    }
    /// Assert no partial frame remains; transitions to the ended state.
    pub fn end(&mut self) -> Result<(), FrameError> {
        match self.state {
            DecoderState::Ended => return Err(FrameError::Ended),
            DecoderState::Failed => return Err(FrameError::Failed),
            DecoderState::Open => {}
        }
        if self.header_len != 0 || self.expected_payload_len.is_some() {
            self.fail();
            return Err(FrameError::Truncated);
        }
        self.state = DecoderState::Ended;
        Ok(())
    }
    fn fail(&mut self) {
        self.state = DecoderState::Failed;
        self.header_len = 0;
        self.payload.clear();
        self.expected_payload_len = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_frame() {
        let payload = b"hello world";
        let frame = encode_frame(payload);
        let mut dec = FrameDecoder::default();
        let out = dec.push(&frame).expect("decode");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], payload);
        dec.end().expect("end");
    }

    #[test]
    fn incremental_split_byte_by_byte() {
        let payload = b"abcdefgh";
        let frame = encode_frame(payload);
        let mut dec = FrameDecoder::default();
        let mut got = Vec::new();
        for byte in &frame {
            got.extend(dec.push(std::slice::from_ref(byte)).expect("decode"));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], payload);
        dec.end().expect("end");
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let f1 = encode_frame(b"one");
        let f2 = encode_frame(b"two");
        let f3 = encode_frame(b"three");
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&f1);
        chunk.extend_from_slice(&f2);
        chunk.extend_from_slice(&f3);
        let mut dec = FrameDecoder::default();
        let out = dec.push(&chunk).expect("decode");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], b"one");
        assert_eq!(out[1], b"two");
        assert_eq!(out[2], b"three");
        dec.end().expect("end");
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&(16 * 1024 * 1024 + 1u32).to_be_bytes());
        let mut dec = FrameDecoder::default();
        let err = dec.push(&prefix).expect_err("expected error");
        assert_eq!(
            err,
            FrameError::Oversized {
                declared: 16 * 1024 * 1024 + 1,
                limit: 16 * 1024 * 1024
            }
        );
    }

    #[test]
    fn truncated_at_end() {
        let payload = b"abc";
        let frame = encode_frame(payload);
        let mut dec = FrameDecoder::default();
        let _ = dec.push(&frame[..FRAME_HEADER_LENGTH]).expect("header");
        let err = dec.end().expect_err("expected error");
        assert_eq!(err, FrameError::Truncated);
    }

    #[test]
    fn zero_length_frame() {
        let frame = encode_frame(&[]);
        let mut dec = FrameDecoder::default();
        let out = dec.push(&frame).expect("decode");
        assert_eq!(out.len(), 1);
        assert!(out[0].is_empty());
        dec.end().expect("end");
    }

    #[test]
    fn assert_complete_frame_validates() {
        let payload = b"test";
        let frame = encode_frame(payload);
        assert_complete_frame(&frame, None).expect("valid");
        let err = assert_complete_frame(&frame[..3], None).expect_err("expected error");
        assert_eq!(err, FrameError::IncompleteHeader);
    }
}
