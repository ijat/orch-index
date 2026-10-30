//! `SQLite` persistence for extracted facts, and the lookups that queries are built on.
//!
//! Names are interned into `names` so the large tables (`refs` above all) hold integers only.
//! Each file's facts are replaced wholesale when its blob id changes.

mod schema;

use std::collections::HashMap;
use std::path::Path;

use oi_model::{FileFacts, Lang, Provenance, RefKind, SymbolKind};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "index schema version {found} is not supported (expected {expected}); rebuild the index"
    )]
    SchemaVersion { found: i64, expected: i64 },
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Restricts lookups to files under a path prefix and/or of one language.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter<'a> {
    pub path: Option<&'a str>,
    pub lang: Option<Lang>,
}

impl Filter<'_> {
    fn lang_str(&self) -> Option<&'static str> {
        self.lang.map(Lang::as_str)
    }
}

/// An indexed file as recorded in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub oid: String,
}

/// A symbol row joined with its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    pub path: String,
    pub oid: String,
    pub line: u32,
    pub name: String,
    pub scope: String,
    pub kind: SymbolKind,
    pub is_definition: bool,
    pub signature: String,
    pub is_virtual: bool,
    pub provenance: Provenance,
    /// The file had declaration-level syntax errors; facts from it may be incomplete.
    pub file_damaged: bool,
    pub lang: Option<Lang>,
}

/// A reference row joined with its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefHit {
    pub path: String,
    pub oid: String,
    pub line: u32,
    pub kind: RefKind,
    /// Qualified name of the enclosing function, if any.
    pub container: Option<String>,
    pub file_damaged: bool,
}

/// An include directive joined with the including file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeHit {
    pub path: String,
    pub oid: String,
    pub line: u32,
    pub spec: String,
    pub file_damaged: bool,
}

/// A base-class edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseHit {
    pub path: String,
    pub line: u32,
    pub derived: String,
    pub derived_scope: String,
    /// The base as written at the edge, e.g. `URLRequest::Delegate`.
    pub base_written: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the index at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        schema::init(&conn)?;
        Ok(Self { conn })
    }

    /// An in-memory store, for tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::init(&conn)?;
        Ok(Self { conn })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// All indexed files, keyed by path.
    pub fn files(&self) -> Result<HashMap<String, FileRow>> {
        read_files(&self.conn)
    }

    /// Starts a write batch. Nothing is visible to readers until [`Writer::commit`]. The
    /// transaction takes the write lock immediately, so a concurrent update waits (up to the
    /// busy timeout) instead of failing halfway; read state through the writer, not the store.
    pub fn writer(&mut self) -> Result<Writer<'_>> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Names are looked up lazily: preloading millions of them would make a no-op update
        // cost seconds.
        Ok(Writer {
            tx,
            names: HashMap::new(),
        })
    }

    fn name_id(&self, name: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM names WHERE name = ?1", [name], |r| r.get(0))
            .optional()?)
    }

    /// Symbols named `name`, optionally restricted to `kinds` and a path prefix.
    pub fn symbols(
        &self,
        name: &str,
        kinds: &[SymbolKind],
        filter: Filter<'_>,
    ) -> Result<Vec<SymbolHit>> {
        let Some(name_id) = self.name_id(name)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path, f.oid, s.line, s.scope, s.kind, s.is_def, s.signature, s.is_virtual, s.prov,
                    f.structural > 0, f.lang
             FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE s.name_id = ?1 AND (?2 IS NULL OR substr(f.path, 1, length(?2)) = ?2)
               AND (?3 IS NULL OR f.lang = ?3)
             ORDER BY f.path, s.line",
        )?;
        let rows = stmt.query_map(params![name_id, filter.path, filter.lang_str()], |r| {
            Ok(SymbolHit {
                path: r.get(0)?,
                oid: r.get(1)?,
                line: r.get(2)?,
                name: name.to_owned(),
                scope: r.get(3)?,
                kind: SymbolKind::parse(&r.get::<_, String>(4)?).unwrap_or(SymbolKind::Variable),
                is_definition: r.get(5)?,
                signature: r.get(6)?,
                is_virtual: r.get(7)?,
                provenance: Provenance::parse(&r.get::<_, String>(8)?)
                    .unwrap_or(Provenance::Syntactic),
                file_damaged: r.get(9)?,
                lang: Lang::parse(&r.get::<_, String>(10)?),
            })
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let hit = row?;
            if kinds.is_empty() || kinds.contains(&hit.kind) {
                hits.push(hit);
            }
        }
        Ok(hits)
    }

    /// References to `name`, optionally of one kind and under a path prefix.
    pub fn refs(
        &self,
        name: &str,
        kind: Option<RefKind>,
        filter: Filter<'_>,
    ) -> Result<Vec<RefHit>> {
        let Some(name_id) = self.name_id(name)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path, f.oid, r.line, r.kind, c.name, f.structural > 0
             FROM refs r JOIN files f ON f.id = r.file_id
             LEFT JOIN names c ON c.id = r.container_id
             WHERE r.name_id = ?1 AND (?2 IS NULL OR r.kind = ?2)
               AND (?3 IS NULL OR substr(f.path, 1, length(?3)) = ?3)
               AND (?4 IS NULL OR f.lang = ?4)
             ORDER BY f.path, r.line",
        )?;
        let kind_code = kind.map(ref_kind_code);
        let rows = stmt.query_map(
            params![name_id, kind_code, filter.path, filter.lang_str()],
            |r| {
                Ok(RefHit {
                    path: r.get(0)?,
                    oid: r.get(1)?,
                    line: r.get(2)?,
                    kind: ref_kind_from_code(r.get(3)?),
                    container: r.get(4)?,
                    file_damaged: r.get(5)?,
                })
            },
        )?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Include/import directives with lookup key `key` (see [`oi_model::Include::key`]), either
    /// module imports or file includes.
    pub fn includes_by_key(
        &self,
        key: &str,
        module: bool,
        filter: Filter<'_>,
    ) -> Result<Vec<IncludeHit>> {
        let Some(base_id) = self.name_id(key)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path, f.oid, i.line, i.spec, f.structural > 0
             FROM includes i JOIN files f ON f.id = i.file_id
             WHERE i.base_id = ?1 AND i.is_module = ?2
               AND (?3 IS NULL OR substr(f.path, 1, length(?3)) = ?3)
               AND (?4 IS NULL OR f.lang = ?4)
             ORDER BY f.path, i.line",
        )?;
        let rows = stmt.query_map(
            params![base_id, module, filter.path, filter.lang_str()],
            |r| {
                Ok(IncludeHit {
                    path: r.get(0)?,
                    oid: r.get(1)?,
                    line: r.get(2)?,
                    spec: r.get(3)?,
                    file_damaged: r.get(4)?,
                })
            },
        )?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Paths of all indexed files whose file name is `basename`.
    pub fn files_by_basename(&self, basename: &str) -> Result<Vec<String>> {
        let Some(base_id) = self.name_id(basename)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM files WHERE base_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map([base_id], |r| r.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Classes that list `base` as a direct base.
    pub fn derived_of(&self, base: &str) -> Result<Vec<BaseHit>> {
        let Some(base_id) = self.name_id(base)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path, b.line, d.name, b.derived_scope, b.written
             FROM bases b JOIN files f ON f.id = b.file_id JOIN names d ON d.id = b.derived_id
             WHERE b.base_id = ?1 ORDER BY f.path, b.line",
        )?;
        let rows = stmt.query_map([base_id], |r| {
            Ok(BaseHit {
                path: r.get(0)?,
                line: r.get(1)?,
                derived: r.get(2)?,
                derived_scope: r.get(3)?,
                base_written: r.get(4)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Indexed files whose parse had syntax errors: (any, declaration-level).
    pub fn files_with_errors(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT count(*) FILTER (WHERE errors > 0), count(*) FILTER (WHERE structural > 0)
             FROM files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Row counts per table, for `stats`.
    pub fn counts(&self) -> Result<Vec<(&'static str, i64)>> {
        ["files", "names", "symbols", "refs", "includes", "bases"]
            .into_iter()
            .map(|t| {
                let n = self
                    .conn
                    .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?;
                Ok((t, n))
            })
            .collect()
    }
}

/// A write batch over one transaction, with an in-memory name-interning cache.
pub struct Writer<'a> {
    tx: Transaction<'a>,
    names: HashMap<String, i64>,
}

impl Writer<'_> {
    /// All indexed files, as seen inside this transaction.
    pub fn files(&self) -> Result<HashMap<String, FileRow>> {
        read_files(&self.tx)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    fn intern(&mut self, name: &str) -> Result<i64> {
        if let Some(&id) = self.names.get(name) {
            return Ok(id);
        }
        let existing: Option<i64> = self
            .tx
            .prepare_cached("SELECT id FROM names WHERE name = ?1")?
            .query_row([name], |r| r.get(0))
            .optional()?;
        let id = if let Some(id) = existing {
            id
        } else {
            self.tx
                .prepare_cached("INSERT INTO names (name) VALUES (?1)")?
                .execute([name])?;
            self.tx.last_insert_rowid()
        };
        self.names.insert(name.to_owned(), id);
        Ok(id)
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Deletes a file and every fact extracted from it.
    pub fn remove_file(&mut self, file_id: i64) -> Result<()> {
        for table in ["symbols", "refs", "includes", "bases"] {
            self.tx
                .prepare_cached(&format!("DELETE FROM {table} WHERE file_id = ?1"))?
                .execute([file_id])?;
        }
        self.tx
            .prepare_cached("DELETE FROM files WHERE id = ?1")?
            .execute([file_id])?;
        Ok(())
    }

    /// Inserts a file and its facts. The path must not already be present.
    pub fn put_file(&mut self, path: &str, oid: &str, lang: Lang, facts: &FileFacts) -> Result<()> {
        let basename = path.rsplit('/').next().unwrap_or(path);
        let base_id = self.intern(basename)?;
        self.tx
            .prepare_cached(
                "INSERT INTO files (path, oid, lang, errors, structural, base_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                path,
                oid,
                lang.as_str(),
                facts.parse_errors,
                facts.structural_errors,
                base_id
            ])?;
        let file_id = self.tx.last_insert_rowid();
        let prov = Provenance::Syntactic.as_str();

        for s in &facts.symbols {
            let name_id = self.intern(&s.name)?;
            self.tx
                .prepare_cached(
                    "INSERT INTO symbols (file_id, name_id, line, scope, kind, is_def, signature, is_virtual, prov)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?
                .execute(params![
                    file_id,
                    name_id,
                    s.line,
                    s.scope,
                    s.kind.as_str(),
                    s.is_definition,
                    s.signature,
                    s.is_virtual,
                    prov
                ])?;
        }
        for r in &facts.references {
            let name_id = self.intern(&r.name)?;
            let container_id = match &r.container {
                Some(c) => Some(self.intern(c)?),
                None => None,
            };
            self.tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO refs (name_id, file_id, line, kind, container_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    name_id,
                    file_id,
                    r.line,
                    ref_kind_code(r.kind),
                    container_id
                ])?;
        }
        for i in &facts.includes {
            let base_id = self.intern(i.key())?;
            self.tx
                .prepare_cached(
                    "INSERT INTO includes (file_id, line, spec, is_system, is_module, base_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?
                .execute(params![
                    file_id, i.line, i.spec, i.system, i.module, base_id
                ])?;
        }
        for b in &facts.bases {
            let derived_id = self.intern(&b.derived)?;
            let base_id = self.intern(&b.base)?;
            self.tx
                .prepare_cached(
                    "INSERT INTO bases (file_id, line, derived_id, derived_scope, base_id, written)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?
                .execute(params![
                    file_id,
                    b.line,
                    derived_id,
                    b.derived_scope,
                    base_id,
                    b.base_written
                ])?;
        }
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}

fn read_files(conn: &Connection) -> Result<HashMap<String, FileRow>> {
    let mut stmt = conn.prepare("SELECT id, path, oid FROM files")?;
    let rows = stmt.query_map([], |r| {
        Ok(FileRow {
            id: r.get(0)?,
            path: r.get(1)?,
            oid: r.get(2)?,
        })
    })?;
    rows.map(|r| r.map(|f| (f.path.clone(), f)).map_err(Into::into))
        .collect()
}

const fn ref_kind_code(kind: RefKind) -> i64 {
    match kind {
        RefKind::Call => 0,
        RefKind::Type => 1,
        RefKind::Use => 2,
    }
}

const fn ref_kind_from_code(code: i64) -> RefKind {
    match code {
        0 => RefKind::Call,
        1 => RefKind::Type,
        _ => RefKind::Use,
    }
}
