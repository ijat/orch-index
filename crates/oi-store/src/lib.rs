//! `SQLite` persistence for extracted facts, and the lookups that queries are built on.
//!
//! Names are interned into `names` so the large tables (`refs` above all) hold integers only.
//! Each file's facts are replaced wholesale when its blob id changes.

mod lock;
mod schema;
mod usage;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use oi_model::{FileFacts, Lang, Provenance, RefKind, SymbolKind};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

pub use lock::{IndexLock, is_indexing, lock_path};
pub use usage::{
    LATENCY_BOUNDS_MS, LATENCY_BUCKETS, LabelClass, LabelCounts, LatencyStats, UsageSample,
    UsageSummary, latency_bucket,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "index schema version {found} is not supported (expected {expected}); rebuild the index"
    )]
    SchemaVersion { found: i64, expected: i64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
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

/// Row counts cached in `meta` by [`Writer::refresh_counts`], so readers need no table scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedCounts {
    pub files: i64,
    pub symbols: i64,
    pub refs: i64,
    pub includes: i64,
    pub bases: i64,
    pub names: i64,
    /// Files whose parse had any syntax error.
    pub parse_error_files: i64,
    /// Files with declaration-level syntax errors.
    pub parse_error_structural: i64,
    /// Indexed files per language.
    pub languages: BTreeMap<String, i64>,
}

/// One `index` run, as recorded in `index_runs`. Times are unix seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexRun {
    pub at: i64,
    /// `full` or `incremental`.
    pub kind: String,
    pub seconds: Option<f64>,
    pub list_seconds: Option<f64>,
    pub extract_seconds: Option<f64>,
    pub files_total: Option<i64>,
    pub extracted: Option<i64>,
    pub reused: Option<i64>,
    pub skipped_large: Option<i64>,
    pub db_bytes_before: Option<i64>,
    pub db_bytes_after: Option<i64>,
}

/// How many [`IndexRun`]s are kept; older ones are deleted as new ones are recorded.
const INDEX_RUNS_KEPT: i64 = 10;

/// A read-only open waits this long at most for a lock (WAL readers rarely need one).
const READ_ONLY_BUSY_TIMEOUT: Duration = Duration::from_millis(500);

pub struct Store {
    conn: Connection,
    /// The database file; `None` for an in-memory store.
    path: Option<PathBuf>,
    schema: i64,
}

impl Store {
    /// Opens (creating if needed) the index at `path`, migrating a v4 index to the current
    /// schema in place.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let schema = schema::init(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            schema,
        })
    }

    /// Opens an existing index without writing to it: no create, no DDL, no migration. Accepts
    /// the current schema and v4 (which lacks `index_runs` and `usage`, see [`Self::has_table`]);
    /// any other version is [`StoreError::SchemaVersion`].
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(READ_ONLY_BUSY_TIMEOUT)?;
        let schema = match schema::stored_version(&conn)? {
            Some(v) if v == schema::VERSION || v == schema::V4 => v,
            found => {
                return Err(StoreError::SchemaVersion {
                    // A database without a version was never finished by `open`.
                    found: found.unwrap_or(0),
                    expected: schema::VERSION,
                });
            }
        };
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            schema,
        })
    }

    /// An in-memory store, for tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let schema = schema::init(&conn)?;
        Ok(Self {
            conn,
            path: None,
            schema,
        })
    }

    /// The database file, or `None` for an in-memory store.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The schema version of the open database: always current for a read-write open, 4 or
    /// current for [`Self::open_read_only`].
    #[must_use]
    pub const fn schema_version(&self) -> i64 {
        self.schema
    }

    /// Whether the database has a table named `name`.
    pub fn has_table(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Takes the index lock (`<db>.lock`), blocking while another update holds it. `None` for an
    /// in-memory store. Released when the returned guard is dropped.
    pub fn index_lock(&self) -> Result<Option<IndexLock>> {
        self.path.as_deref().map(lock::acquire).transpose()
    }

    /// The counts last cached by [`Writer::refresh_counts`]; `None` if they never were (or the
    /// cache is incomplete).
    pub fn cached_counts(&self) -> Result<Option<CachedCounts>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value FROM meta
             WHERE substr(key, 1, 6) = 'count.' OR substr(key, 1, 5) = 'lang.'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut counts = HashMap::new();
        let mut languages = BTreeMap::new();
        for row in rows {
            let (key, value) = row?;
            let Ok(n) = value.parse::<i64>() else {
                return Ok(None);
            };
            if let Some(lang) = key.strip_prefix("lang.") {
                languages.insert(lang.to_owned(), n);
            } else {
                counts.insert(key, n);
            }
        }
        let get = |k: &str| counts.get(&format!("count.{k}")).copied();
        let (
            Some(files),
            Some(symbols),
            Some(refs),
            Some(includes),
            Some(bases),
            Some(names),
            Some(parse_error_files),
            Some(parse_error_structural),
        ) = (
            get("files"),
            get("symbols"),
            get("refs"),
            get("includes"),
            get("bases"),
            get("names"),
            get("parse_error_files"),
            get("parse_error_structural"),
        )
        else {
            return Ok(None);
        };
        Ok(Some(CachedCounts {
            files,
            symbols,
            refs,
            includes,
            bases,
            names,
            parse_error_files,
            parse_error_structural,
            languages,
        }))
    }

    /// Records an index run and deletes all but the newest [`INDEX_RUNS_KEPT`], in one
    /// transaction.
    pub fn record_index_run(&self, run: &IndexRun) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO index_runs (at, kind, seconds, list_seconds, extract_seconds, files_total,
                                     extracted, reused, skipped_large, db_bytes_before, db_bytes_after)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.at,
                run.kind,
                run.seconds,
                run.list_seconds,
                run.extract_seconds,
                run.files_total,
                run.extracted,
                run.reused,
                run.skipped_large,
                run.db_bytes_before,
                run.db_bytes_after
            ],
        )?;
        tx.execute(
            "DELETE FROM index_runs
             WHERE id NOT IN (SELECT id FROM index_runs ORDER BY id DESC LIMIT ?1)",
            [INDEX_RUNS_KEPT],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest `limit` index runs, newest first.
    pub fn index_runs(&self, limit: usize) -> Result<Vec<IndexRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, kind, seconds, list_seconds, extract_seconds, files_total, extracted,
                    reused, skipped_large, db_bytes_before, db_bytes_after
             FROM index_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
            Ok(IndexRun {
                at: r.get(0)?,
                kind: r.get(1)?,
                seconds: r.get(2)?,
                list_seconds: r.get(3)?,
                extract_seconds: r.get(4)?,
                files_total: r.get(5)?,
                extracted: r.get(6)?,
                reused: r.get(7)?,
                skipped_large: r.get(8)?,
                db_bytes_before: r.get(9)?,
                db_bytes_after: r.get(10)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
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
        files_with_errors(&self.conn)
    }

    /// Row counts per table, for `stats`.
    pub fn counts(&self) -> Result<Vec<(&'static str, i64)>> {
        table_counts(&self.conn)
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

    /// Caches row counts, parse-error counts and files per language in `meta` (see
    /// [`Store::cached_counts`]), replacing any stale `lang.*` keys. Call it last, so the counts
    /// describe what this transaction commits.
    pub fn refresh_counts(&mut self) -> Result<()> {
        let counts = table_counts(&self.tx)?;
        let (errors, structural) = files_with_errors(&self.tx)?;
        let languages: Vec<(String, i64)> = {
            let mut stmt = self
                .tx
                .prepare("SELECT lang, count(*) FROM files GROUP BY lang")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        self.tx
            .execute("DELETE FROM meta WHERE substr(key, 1, 5) = 'lang.'", [])?;
        for (table, n) in counts {
            self.set_meta(&format!("count.{table}"), &n.to_string())?;
        }
        self.set_meta("count.parse_error_files", &errors.to_string())?;
        self.set_meta("count.parse_error_structural", &structural.to_string())?;
        for (lang, n) in languages {
            self.set_meta(&format!("lang.{lang}"), &n.to_string())?;
        }
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}

fn files_with_errors(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*) FILTER (WHERE errors > 0), count(*) FILTER (WHERE structural > 0)
         FROM files",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

fn table_counts(conn: &Connection) -> Result<Vec<(&'static str, i64)>> {
    ["files", "names", "symbols", "refs", "includes", "bases"]
        .into_iter()
        .map(|t| {
            let n = conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?;
            Ok((t, n))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use oi_model::Symbol;

    use super::*;

    /// A v4 index as the previous release left it: its tables, `schema = 4`, one file.
    fn v4_db(path: &Path, version: i64) {
        let conn = Connection::open(path).unwrap();
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .unwrap();
        conn.execute_batch(schema::DDL).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO meta (key, value) VALUES ('schema', '{version}'), ('extractor', '9'),
                 ('commit', 'abc123');
             INSERT INTO names (id, name) VALUES (1, 'a.rs');
             INSERT INTO files (path, oid, lang, errors, structural, base_id)
                 VALUES ('src/a.rs', 'oid1', 'rust', 0, 0, 1);"
        ))
        .unwrap();
    }

    fn file_count(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap()
    }

    fn facts(names: &[&str], parse_errors: u32, structural_errors: u32) -> FileFacts {
        FileFacts {
            symbols: names
                .iter()
                .map(|n| Symbol {
                    name: (*n).to_owned(),
                    scope: String::new(),
                    kind: SymbolKind::Function,
                    line: 1,
                    is_definition: true,
                    signature: String::new(),
                    is_virtual: false,
                })
                .collect(),
            parse_errors,
            structural_errors,
            ..FileFacts::default()
        }
    }

    fn run(at: i64) -> IndexRun {
        IndexRun {
            at,
            kind: "incremental".to_owned(),
            seconds: Some(1.5),
            list_seconds: Some(0.25),
            extract_seconds: None,
            files_total: Some(3),
            extracted: Some(1),
            reused: Some(2),
            skipped_large: Some(0),
            db_bytes_before: Some(100),
            db_bytes_after: None,
        }
    }

    #[test]
    fn fresh_db_is_current_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let store = Store::open(&db).unwrap();
        assert_eq!(store.schema_version(), 5);
        assert_eq!(store.meta("schema").unwrap().as_deref(), Some("5"));
        assert!(store.has_table("index_runs").unwrap());
        assert!(store.has_table("usage").unwrap());
        assert_eq!(store.path(), Some(db.as_path()));
        drop(store);
        // Reopening a current index changes nothing.
        assert_eq!(Store::open(&db).unwrap().schema_version(), 5);
        let ro = Store::open_read_only(&db).unwrap();
        assert_eq!(ro.schema_version(), 5);
        assert!(ro.has_table("usage").unwrap());
        assert_eq!(Store::in_memory().unwrap().schema_version(), 5);
    }

    #[test]
    fn v4_is_migrated_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        v4_db(&db, 4);
        let store = Store::open(&db).unwrap();
        assert_eq!(store.schema_version(), 5);
        assert_eq!(store.meta("schema").unwrap().as_deref(), Some("5"));
        assert_eq!(store.meta("extractor").unwrap().as_deref(), Some("9"));
        assert_eq!(store.meta("commit").unwrap().as_deref(), Some("abc123"));
        assert!(store.has_table("index_runs").unwrap());
        assert!(store.has_table("usage").unwrap());
        assert_eq!(file_count(&store), 1);
        assert_eq!(store.cached_counts().unwrap(), None);
    }

    #[test]
    fn opening_a_current_index_does_not_wait_for_a_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let mut writing = Store::open(&db).unwrap();
        let w = writing.writer().unwrap();
        let start = std::time::Instant::now();
        let reader = Store::open(&db).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(reader.schema_version(), 5);
        assert!(Store::open_read_only(&db).is_ok());
        w.commit().unwrap();
    }

    #[test]
    fn read_only_open_of_v4_does_not_migrate() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        v4_db(&db, 4);
        let ro = Store::open_read_only(&db).unwrap();
        assert_eq!(ro.schema_version(), 4);
        assert!(!ro.has_table("usage").unwrap());
        assert!(!ro.has_table("index_runs").unwrap());
        assert!(ro.has_table("files").unwrap());
        assert_eq!(ro.cached_counts().unwrap(), None);
        assert_eq!(ro.meta("schema").unwrap().as_deref(), Some("4"));
        assert!(ro.record_index_run(&run(1)).is_err());
    }

    #[test]
    fn read_only_open_never_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        assert!(Store::open_read_only(&db).is_err());
        assert!(!db.exists());
        let nested = tmp.path().join("missing-dir").join("index.db");
        assert!(Store::open_read_only(&nested).is_err());
        assert!(!tmp.path().join("missing-dir").exists());
    }

    #[test]
    fn index_runs_keep_newest_ten() {
        let store = Store::in_memory().unwrap();
        assert!(store.index_runs(100).unwrap().is_empty());
        for at in 1..=12 {
            store.record_index_run(&run(at)).unwrap();
        }
        let runs = store.index_runs(100).unwrap();
        assert_eq!(runs.len(), 10);
        let ats: Vec<i64> = runs.iter().map(|r| r.at).collect();
        assert_eq!(ats, (3..=12).rev().collect::<Vec<_>>());
        assert_eq!(runs[0], run(12));
        assert_eq!(store.index_runs(2).unwrap().len(), 2);
    }

    #[test]
    fn cached_counts_match_live_counts() {
        let mut store = Store::in_memory().unwrap();
        assert_eq!(store.cached_counts().unwrap(), None);
        let mut w = store.writer().unwrap();
        w.put_file("a.rs", "o1", Lang::Rust, &facts(&["f", "g"], 0, 0))
            .unwrap();
        w.put_file("b.rs", "o2", Lang::Rust, &facts(&["h"], 2, 1))
            .unwrap();
        w.put_file("c.cc", "o3", Lang::Cpp, &facts(&["f"], 1, 0))
            .unwrap();
        w.refresh_counts().unwrap();
        w.commit().unwrap();

        let cached = store.cached_counts().unwrap().unwrap();
        let live: HashMap<_, _> = store.counts().unwrap().into_iter().collect();
        assert_eq!(cached.files, live["files"]);
        assert_eq!(cached.symbols, live["symbols"]);
        assert_eq!(cached.refs, live["refs"]);
        assert_eq!(cached.includes, live["includes"]);
        assert_eq!(cached.bases, live["bases"]);
        assert_eq!(cached.names, live["names"]);
        assert_eq!(
            (cached.parse_error_files, cached.parse_error_structural),
            store.files_with_errors().unwrap()
        );
        assert_eq!((cached.files, cached.symbols), (3, 4));
        assert_eq!(cached.parse_error_files, 2);
        assert_eq!(
            cached.languages,
            BTreeMap::from([("cpp".to_owned(), 1), ("rust".to_owned(), 2)])
        );

        // A language that is no longer indexed loses its key.
        let mut w = store.writer().unwrap();
        let id = w.files().unwrap()["c.cc"].id;
        w.remove_file(id).unwrap();
        w.refresh_counts().unwrap();
        w.commit().unwrap();
        let cached = store.cached_counts().unwrap().unwrap();
        assert_eq!(cached.files, 2);
        assert_eq!(cached.languages, BTreeMap::from([("rust".to_owned(), 2)]));
    }

    #[test]
    fn index_lock_sets_indexing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let lock_file = tmp.path().join("index.db.lock");
        assert_eq!(lock_path(&db), lock_file);
        // No lock file: not indexing, and checking does not create it.
        assert!(!is_indexing(&db));
        assert!(!lock_file.exists());

        let store = Store::open(&db).unwrap();
        let guard = store.index_lock().unwrap().unwrap();
        assert!(lock_file.exists());
        assert!(is_indexing(&db));
        drop(guard);
        assert!(!is_indexing(&db));
        // The lock can be taken again once released.
        let again = store.index_lock().unwrap();
        assert!(again.is_some());
        drop(again);

        assert!(Store::in_memory().unwrap().index_lock().unwrap().is_none());
    }

    #[test]
    fn other_schema_versions_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        v4_db(&db, 3);
        assert!(matches!(
            Store::open_read_only(&db),
            Err(StoreError::SchemaVersion {
                found: 3,
                expected: 5
            })
        ));
        assert!(matches!(
            Store::open(&db),
            Err(StoreError::SchemaVersion {
                found: 3,
                expected: 5
            })
        ));
        // Neither open rewrote the version.
        let conn = Connection::open(&db).unwrap();
        let v: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, "3");
    }

    #[test]
    fn non_numeric_schema_is_rejected_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(schema::DDL).unwrap();
        conn.execute("INSERT INTO meta (key, value) VALUES ('schema', 'x')", [])
            .unwrap();
        drop(conn);
        assert!(matches!(
            Store::open(&db),
            Err(StoreError::SchemaVersion { found: -1, .. })
        ));
    }
}
