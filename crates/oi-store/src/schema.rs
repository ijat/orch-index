use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::{Result, StoreError};

/// Bump on any change to the layout. An incompatible change makes old indexes unsupported (they
/// are rebuilt); an additive one migrates the previous version in place, see [`init`].
pub(crate) const VERSION: i64 = 5;

/// The previous version, migrated to [`VERSION`] by adding [`V5_DDL`].
pub(crate) const V4: i64 = 4;

pub(crate) const DDL: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS names (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    oid TEXT NOT NULL,
    lang TEXT NOT NULL,
    errors INTEGER NOT NULL,
    structural INTEGER NOT NULL,
    base_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS files_base ON files (base_id);
CREATE TABLE IF NOT EXISTS symbols (
    file_id INTEGER NOT NULL,
    name_id INTEGER NOT NULL,
    line INTEGER NOT NULL,
    scope TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_def INTEGER NOT NULL,
    signature TEXT NOT NULL,
    is_virtual INTEGER NOT NULL,
    prov TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols (name_id);
CREATE INDEX IF NOT EXISTS symbols_file ON symbols (file_id);
CREATE TABLE IF NOT EXISTS refs (
    name_id INTEGER NOT NULL,
    file_id INTEGER NOT NULL,
    line INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    container_id INTEGER,
    PRIMARY KEY (name_id, file_id, line, kind)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS refs_file ON refs (file_id);
CREATE TABLE IF NOT EXISTS includes (
    file_id INTEGER NOT NULL,
    line INTEGER NOT NULL,
    spec TEXT NOT NULL,
    is_system INTEGER NOT NULL,
    is_module INTEGER NOT NULL,
    base_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS includes_base ON includes (base_id);
CREATE INDEX IF NOT EXISTS includes_file ON includes (file_id);
CREATE TABLE IF NOT EXISTS bases (
    file_id INTEGER NOT NULL,
    line INTEGER NOT NULL,
    derived_id INTEGER NOT NULL,
    derived_scope TEXT NOT NULL,
    base_id INTEGER NOT NULL,
    written TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS bases_base ON bases (base_id);
CREATE INDEX IF NOT EXISTS bases_derived ON bases (derived_id);
CREATE INDEX IF NOT EXISTS bases_file ON bases (file_id);
";

/// Tables added in schema 5. Additive only: a v4 index gains them without touching its facts.
const V5_DDL: &str = "
CREATE TABLE IF NOT EXISTS index_runs (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    seconds REAL,
    list_seconds REAL,
    extract_seconds REAL,
    files_total INTEGER,
    extracted INTEGER,
    reused INTEGER,
    skipped_large INTEGER,
    db_bytes_before INTEGER,
    db_bytes_after INTEGER
);
CREATE TABLE IF NOT EXISTS usage (
    day INTEGER NOT NULL,
    command TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    total_ms INTEGER NOT NULL DEFAULT 0,
    max_ms INTEGER NOT NULL DEFAULT 0,
    h0 INTEGER NOT NULL DEFAULT 0,
    h1 INTEGER NOT NULL DEFAULT 0,
    h2 INTEGER NOT NULL DEFAULT 0,
    h3 INTEGER NOT NULL DEFAULT 0,
    h4 INTEGER NOT NULL DEFAULT 0,
    h5 INTEGER NOT NULL DEFAULT 0,
    h6 INTEGER NOT NULL DEFAULT 0,
    h7 INTEGER NOT NULL DEFAULT 0,
    h8 INTEGER NOT NULL DEFAULT 0,
    h9 INTEGER NOT NULL DEFAULT 0,
    h10 INTEGER NOT NULL DEFAULT 0,
    h11 INTEGER NOT NULL DEFAULT 0,
    hits_total INTEGER NOT NULL DEFAULT 0,
    zero_hit INTEGER NOT NULL DEFAULT 0,
    stale_answers INTEGER NOT NULL DEFAULT 0,
    lab_resolved INTEGER NOT NULL DEFAULT 0,
    lab_syntactic INTEGER NOT NULL DEFAULT 0,
    lab_ambiguous INTEGER NOT NULL DEFAULT 0,
    lab_parse_errors INTEGER NOT NULL DEFAULT 0,
    lab_compiler INTEGER NOT NULL DEFAULT 0,
    last_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, command)
) WITHOUT ROWID;
";

const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Switches to WAL once. Changing the journal mode needs an exclusive lock and `SQLite` does not
/// run the busy handler for it, so retry while another connection is writing.
fn enable_wal(conn: &Connection) -> Result<()> {
    let start = Instant::now();
    loop {
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if mode.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        // The reply is the mode now in effect: `memory` for in-memory databases, which cannot
        // use WAL. Either way the switch is settled; only a busy database is retried.
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && start.elapsed() < BUSY_TIMEOUT =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// The stored schema version: `None` when the key is absent (a fresh database). A value that is
/// not a number reads as `-1`, so it is reported unsupported rather than overwritten.
pub(crate) fn stored_version(conn: &Connection) -> Result<Option<i64>> {
    let found: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(found.map(|v| v.parse::<i64>().unwrap_or(-1)))
}

/// Prepares a read-write connection and returns the schema version now stored ([`VERSION`]).
///
/// A fresh database gets the current schema; a v4 one is migrated in place (new tables plus
/// `schema = 5`, in one transaction, leaving `extractor`, `commit` and every fact untouched). A
/// current one is opened without writing, so queries never wait behind a running `index`.
pub(crate) fn init(conn: &Connection) -> Result<i64> {
    // Concurrent `index` runs (parallel agents) wait for each other rather than fail.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    enable_wal(conn)?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -262144;",
    )?;
    conn.execute_batch(DDL)?;
    match stored_version(conn)? {
        Some(VERSION) => Ok(VERSION),
        None | Some(V4) => migrate(conn),
        Some(v) => Err(StoreError::SchemaVersion {
            found: v,
            expected: VERSION,
        }),
    }
}

/// Brings a fresh or v4 database to [`VERSION`] under the write lock. The version is re-read
/// inside the transaction: another process may have created or migrated it meanwhile.
fn migrate(conn: &Connection) -> Result<i64> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    match stored_version(&tx)? {
        Some(VERSION) => return Ok(VERSION),
        None | Some(V4) => {}
        Some(v) => {
            return Err(StoreError::SchemaVersion {
                found: v,
                expected: VERSION,
            });
        }
    }
    tx.execute_batch(V5_DDL)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('schema', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [VERSION.to_string()],
    )?;
    tx.commit()?;
    Ok(VERSION)
}
