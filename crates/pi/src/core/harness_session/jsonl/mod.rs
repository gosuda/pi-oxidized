//! Durable JSONL v4 session backend, legacy v3 importer, and repository.

pub mod codec;
pub mod legacy_v3;
mod paths;
pub mod repo;
pub mod storage;

pub use codec::{
    parse_header, parse_transaction, serialize_transaction, split_complete_lines, JsonlStorageHeader,
    ParsedHeader, JSONL_FORMAT_VERSION, JSONL_STORAGE_VERSION,
};
pub use repo::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlSessionRepo,
};
pub use storage::JsonlStorage;
