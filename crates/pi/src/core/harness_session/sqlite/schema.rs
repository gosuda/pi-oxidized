use rusqlite::{Connection, OpenFlags};

/// SQLite session storage version implemented by this backend.
pub const SQLITE_STORAGE_VERSION: u32 = 1;
/// File extension used by the SQLite session repository.
pub const SQLITE_SESSION_EXTENSION: &str = ".sqlite";
/// Sequence assigned to the first committed write in a new session.
pub const FIRST_COMMIT_SEQ: u64 = 1;

const SOURCE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    parent_session_id TEXT,
    storage_version INTEGER NOT NULL,
    metadata TEXT,
    message_count INTEGER NOT NULL,
    usage_payload TEXT NOT NULL,
    next_seq INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS entries (
    session_id TEXT NOT NULL,
    id TEXT NOT NULL,
    parent_id TEXT,
    seq INTEGER NOT NULL,
    type TEXT NOT NULL,
    custom_type TEXT,
    timestamp INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (session_id, id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS ix_entry_parent ON entries(session_id, parent_id);
CREATE INDEX IF NOT EXISTS ix_entry_seq ON entries(session_id, seq, type);

CREATE TABLE IF NOT EXISTS scalar_values (
    session_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    seq INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (session_id, namespace, key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS list_values (
    session_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    seq INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (session_id, namespace, key, seq)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS usage_ledger (
    session_id TEXT NOT NULL,
    id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    entry_id TEXT,
    adjustment INTEGER NOT NULL,
    usage TEXT NOT NULL,
    details TEXT,
    PRIMARY KEY (session_id, id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS ix_usage_seq ON usage_ledger(session_id, seq);

CREATE TRIGGER IF NOT EXISTS trg_entries_validate
BEFORE INSERT ON entries
BEGIN
    SELECT RAISE(ABORT, 'missing parent entry')
    WHERE NEW.parent_id IS NOT NULL
        AND NOT EXISTS (
            SELECT 1 FROM entries WHERE session_id = NEW.session_id AND id = NEW.parent_id
        );

    SELECT RAISE(ABORT, 'duplicate entry or usage id')
    WHERE EXISTS (
        SELECT 1 FROM usage_ledger WHERE session_id = NEW.session_id AND id = NEW.id
    );
END;

CREATE TRIGGER IF NOT EXISTS trg_usage_ledger_validate
BEFORE INSERT ON usage_ledger
BEGIN
    SELECT RAISE(ABORT, 'duplicate entry or usage id')
    WHERE EXISTS (
        SELECT 1 FROM entries WHERE session_id = NEW.session_id AND id = NEW.id
    );
END;

CREATE TABLE IF NOT EXISTS branch_entries (
    session_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    entry_seq INTEGER NOT NULL,
    entry_type TEXT NOT NULL,
    PRIMARY KEY (session_id, branch_id, entry_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS ix_be_seq ON branch_entries(session_id, branch_id, entry_seq, entry_id, entry_type);
CREATE INDEX IF NOT EXISTS ix_be_type ON branch_entries(session_id, branch_id, entry_type, entry_seq, entry_id);
CREATE INDEX IF NOT EXISTS ix_be_entry ON branch_entries(session_id, entry_id);

CREATE TABLE IF NOT EXISTS branch_meta (
    session_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    tip_entry_id TEXT NOT NULL,
    tip_seq INTEGER NOT NULL,
    base_branch_id TEXT,
    base_seq INTEGER,
    PRIMARY KEY (session_id, branch_id)
) WITHOUT ROWID;

CREATE UNIQUE INDEX IF NOT EXISTS ix_bm_tip ON branch_meta(session_id, tip_entry_id);
";

/// Opens a writable connection without creating a missing database.
pub(crate) fn open_existing(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
}

/// Opens a read-only connection without creating a missing database.
pub(crate) fn open_read_only(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

/// Opens a writable connection, creating a fresh file when needed.
pub(crate) fn open_create(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )
}

/// Configures a writable connection with the source backend's PRAGMAs.
pub(crate) fn configure_writable(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
}

/// Configures a read-only connection with the source backend's PRAGMA.
pub(crate) fn configure_read_only(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA busy_timeout = 5000;")
}

/// Applies the source v1 schema to a newly created database.
pub(crate) fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(SOURCE_SCHEMA)
}
