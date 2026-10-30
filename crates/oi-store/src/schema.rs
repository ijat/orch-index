use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};

use crate::{Result, StoreError};

/// Bump on any incompatible change; an old index is rebuilt rather than migrated.
const VERSION: i64 = 4;

const DDL: &str = "
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

pub(crate) fn init(conn: &Connection) -> Result<()> {
    // Concurrent `index` runs (parallel agents) wait for each other rather than fail.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    enable_wal(conn)?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -262144;",
    )?;
    conn.execute_batch(DDL)?;
    let found: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .optional()?;
    match found.and_then(|v| v.parse::<i64>().ok()) {
        None => {
            conn.execute(
                // Two processes may create the same fresh index at once.
                "INSERT INTO meta (key, value) VALUES ('schema', ?1) ON CONFLICT(key) DO NOTHING",
                [VERSION.to_string()],
            )?;
        }
        Some(v) if v != VERSION => {
            return Err(StoreError::SchemaVersion {
                found: v,
                expected: VERSION,
            });
        }
        Some(_) => {}
    }
    Ok(())
}
