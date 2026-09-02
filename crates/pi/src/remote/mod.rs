//! Remote-session wire stack (R1–R4): transport-neutral codec, framing,
//! schemas, transports, and client.
//!
//! The R1–R2 layers port the upstream `pi-protocol` wire exactly: strict
//! RFC 8949 CBOR payloads inside 4-byte big-endian length-prefixed frames
//! with a 16 MiB default bound. Their error surface is exclusively
//! [`codec::CodecError`] and [`framing::FrameError`] — those layers cannot
//! originate disposed/detached/ownership states.
//!
//! R3 adds the byte-transport seam ([`transport`]) and the transport-neutral
//! [`client`]. The in-memory adapter and the client compile on every tier;
//! the Unix-domain adapter exists only on the Unix tier and a Unix endpoint
//! built off that tier fails with a typed
//! [`transport::EndpointSpecError::UnsupportedOnPlatform`].
//!
//! R4 adds the multi-session [`server`] (portable core, zero `cfg`
//! branches) with its `AgentSession` hosting seam and the
//! `#[cfg(unix)]` Unix listener preset, which shares the same typed
//! [`transport::EndpointSpecError`] owner for platform-gated listen
//! specs.
//!
//! See `docs/PAR-WIRE-remote-session-wire-format.md` for the binding
//! decision.

/// Private serde ↔ CborValue adapter.
mod serde_cbor;

pub mod client;
pub mod codec;
pub mod framing;
pub mod schemas;
pub mod server;
pub mod transport;
