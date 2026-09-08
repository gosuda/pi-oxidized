//! Durable, backend-neutral session domain and in-memory implementation.
//!
//! Persistence formats belong to the product crate. This module owns the value
//! model, validation rules, traits, and the reference memory backend.

/// Typed `(namespace, key)` addresses for durable session state, plus the
/// frozen `pi.*` namespace helpers every backend and runtime path shares.
pub mod address;
pub mod configuration;
/// Committed and not-yet-materialized session entries and their bodies.
pub mod entry;
/// Backend-neutral storage failure classification carried across the trait
/// boundary as `SessionError`.
pub mod error;
/// Backend-neutral branch and tree fork snapshots.
pub mod fork;
/// Newtype identifiers for entries, lanes, operations, and usage rows.
pub mod ids;
/// Persisted lane configuration, lane execution state, inbox items, and
/// uncommitted pending entries.
pub mod lane_state;
/// Reference in-memory storage and repository implementations.
pub mod memory;
/// Durable operation records: state, metadata, results, and preparation.
pub mod operation;
/// Scan, cursor, and raw-read shapes shared by every backend query.
pub mod scan;
/// Shared session and branch behavior over a storage backend.
mod backed;
/// The backend contract: `Storage`, read/mutation/session/branch traits, and
/// the session repository.
pub mod traits;
/// The write batch a backend commits atomically, its validation rules, and
/// commit results.
pub mod write;

pub use address::*;
pub use configuration::*;
pub use entry::*;
pub use error::*;
pub use fork::*;
pub use ids::*;
pub use lane_state::*;
pub use memory::*;
pub use operation::*;
pub use scan::*;
pub use backed::*;
pub use traits::*;
pub use write::*;
