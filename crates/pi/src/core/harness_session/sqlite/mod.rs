//! Durable SQLite v1 session backend and repository.

pub mod repo;
/// Schema constants and connection initialization for SQLite session storage.
pub mod schema;
/// Transactional SQLite session storage.
pub mod storage;

pub use repo::{
    SqliteSessionCreateOptions, SqliteSessionMetadata, SqliteSessionRepo, SqliteSessionRepoOptions,
};
pub use schema::{SQLITE_SESSION_EXTENSION, SQLITE_STORAGE_VERSION};
pub use storage::SqliteStorage;
