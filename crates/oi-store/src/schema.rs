use rusqlite::{Connection, OptionalExtension};

use crate::{Result, StoreError};

/// Bump on any incompatible change; an old index is rebuilt rather than migrated.
const VERSION: i64 = 3;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS names (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    oid TEXT NOT NULL,
    lang TEXT NOT NULL,
    errors INTEGER NOT NULL,
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

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
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
                "INSERT INTO meta (key, value) VALUES ('schema', ?1)",
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
