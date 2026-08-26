//! Remote-session wire stack (R1–R2): transport-neutral codec, framing, and schemas.
//!
//! This layer ports the upstream `pi-protocol` wire exactly: strict RFC 8949
//! CBOR payloads inside 4-byte big-endian length-prefixed frames with a
//! 16 MiB default bound. The error surface is exclusively [`codec::CodecError`]
//! and [`framing::FrameError`] — this layer cannot originate
//! disposed/detached/ownership states.
//!
//! See `docs/PAR-WIRE-remote-session-wire-format.md` for the binding decision.

/// Private serde ↔ CborValue adapter.
mod serde_cbor;

pub mod codec;
pub mod framing;
pub mod schemas;
