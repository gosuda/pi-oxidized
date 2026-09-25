//! Durable JSONL v4 session backend, legacy v3 importer, and repository.

/// JSONL v4 wire and on-disk header/transaction codec.
pub mod codec;
/// Legacy v3 JSONL session importer and normalizer.
pub mod legacy_v3;
/// Path encoding and filesystem helpers for the JSONL backend.
mod paths;
/// Session repository: create, open, list, delete, and fork.
pub mod repo;
/// Durable JSONL storage implementation.
pub mod storage;

pub use codec::{
    JSONL_FORMAT_VERSION, JSONL_STORAGE_VERSION, JsonlStorageHeader, ParsedHeader, parse_header,
    parse_transaction, serialize_transaction, split_complete_lines,
};
pub use repo::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlSessionRepo,
};
pub use storage::JsonlStorage;
