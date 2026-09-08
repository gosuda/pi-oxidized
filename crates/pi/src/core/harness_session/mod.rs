//! Durable harness session repositories.
//!
//! JSONL and SQLite are separate repository contracts. JSONL places one
//! session per file under an encoded-cwd directory and carries a required cwd
//! in its wire header. SQLite places sessions in id-named containers or one
//! shared database, lists every stored row, and stores no cwd.

pub mod jsonl;
pub mod sqlite;

pub use jsonl::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlSessionRepo,
    JsonlStorageHeader,
};
pub use sqlite::{
    SqliteSessionCreateOptions, SqliteSessionMetadata, SqliteSessionRepo, SqliteSessionRepoOptions,
};
