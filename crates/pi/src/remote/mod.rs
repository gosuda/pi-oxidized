//! Remote protocol v8: transport-neutral schemas, framing, codec, client,
//! and routed server.
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
//! The [`server`] hosts presentation-scoped service capabilities through
//! `ServerHost`, preserving repository-specific metadata while routing session
//! attachments.  Opaque calls, replies, and updates use the canonical
//! `pi_agent::service::value::JsonValue`; private `CborValue`/`OpaqueJson`
//! adapters exist only at the envelope boundary.  In-process values retain
//! UTF-16 lone surrogates, but CBOR encoding rejects them with a scalar-value
//! error, and byte strings never enter opaque JSON.  Service state uses the
//! shared Chord state codec.  The Unix listener preset retains the
//! platform-gated listen-spec error boundary.
//!
//! See `docs/PAR-WIRE-remote-session-wire-format.md` for the binding
//! decision.

/// Private serde ↔ CborValue adapter.
mod serde_cbor;

pub mod client;
pub mod codec;
pub mod framing;
pub mod product;
pub mod schemas;
pub mod server;
pub mod transport;
