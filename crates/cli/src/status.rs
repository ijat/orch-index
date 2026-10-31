//! `orch-index status`: a cheap, read-only health report (schema 1, see
//! `docs/STATUS-JSON-REQUIREMENTS.md`).
//!
//! It never creates the database, its directory or the lock file, never records usage, and
//! reads only `meta`, `index_runs` and `usage`: no table scan and no tree walk.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use oi_git::Repo;
use oi_indexer::EXTRACTOR_VERSION;
use oi_query::sanitize;
use oi_store::{CachedCounts, IndexRun, Store, StoreError, UsageSummary, is_indexing};
use serde_json::{Map, Value, json};

const SCHEMA: i64 = 1;
const WINDOW_DAYS: i64 = 7;
const HISTORY: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    None,
    Current,
    Stale,
    Outdated,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Outdated => "outdated",
        }
    }
}

/// What an index says about itself, as read from `meta`, `index_runs` and `usage`.
struct Index {
    commit: Option<String>,
    extractor: Option<String>,
    counts: Option<CachedCounts>,
    runs: Vec<IndexRun>,
    usage: Option<UsageSummary>,
}

enum Read {
    /// No database file (or one that cannot be read at all).
    Missing,
    /// A schema this binary does not support.
    Unsupported,
    Index(Box<Index>),
}

pub fn run(repo: &Repo, db: Option<&Path>, json: bool) -> Result<()> {
    // Not a git repository is the only failure.
    let common = repo.common_dir().context("not a git repository")?;
    let db = match db {
        Some(p) => std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?,
        None => common.join("orch-index").join("index.db"),
    };
    let report = build(repo, &db);
    let mut out = std::io::stdout().lock();
    let text = if json {
        serde_json::to_string(&report)?
    } else {
        render_text(&report)
    };
    std::io::Write::write_all(&mut out, text.as_bytes())?;
    std::io::Write::write_all(&mut out, b"\n")?;
    Ok(())
}

fn build(repo: &Repo, db: &Path) -> Value {
    let head = repo.rev_parse("HEAD").ok();
    let read = read_index(db);
    let (state, index) = match &read {
        Read::Missing => (State::None, None),
        Read::Unsupported => (State::Outdated, None),
        Read::Index(i) => (
            state_of(
                i.commit.as_deref(),
                head.as_deref(),
                i.extractor.as_deref(),
                EXTRACTOR_VERSION,
            ),
            Some(i),
        ),
    };
    let indexed = index.and_then(|i| i.commit.as_deref());
    let counts = index.and_then(|i| i.counts.as_ref());

    let mut doc = Map::new();
    doc.insert("schema".into(), json!(SCHEMA));
    doc.insert("state".into(), json!(state.as_str()));
    doc.insert("indexing".into(), json!(is_indexing(db)));
    doc.insert(
        "repo".into(),
        opt_str(repo.toplevel().ok().map(|p| path_text(&p)).as_deref()),
    );
    doc.insert("db".into(), json!(clean(&path_text(db))));
    doc.insert("db_bytes".into(), json!(db_bytes(db)));
    doc.insert("indexed_commit".into(), opt_str(indexed));
    doc.insert("head_commit".into(), opt_str(head.as_deref()));
    doc.insert(
        "commits_behind".into(),
        json!(commits_behind(repo, indexed, head.as_deref())),
    );
    doc.insert(
        "indexed_at".into(),
        json!(index.and_then(|i| i.runs.first()).map(|r| rfc3339(r.at))),
    );
    doc.insert(
        "extractor_version".into(),
        opt_str(index.and_then(|i| i.extractor.as_deref())),
    );
    doc.insert("binary_extractor_version".into(), json!(EXTRACTOR_VERSION));
    doc.insert(
        "counts".into(),
        counts.map_or(Value::Null, |c| {
            json!({
                "files": c.files, "symbols": c.symbols, "refs": c.refs,
                "includes": c.includes, "bases": c.bases, "names": c.names,
            })
        }),
    );
    if let Some(c) = counts {
        let langs: Map<String, Value> = c
            .languages
            .iter()
            .map(|(k, v)| (clean(k), json!(v)))
            .collect();
        doc.insert("languages".into(), Value::Object(langs));
    }
    doc.insert(
        "parse_error_files".into(),
        json!(counts.map(|c| c.parse_error_files)),
    );
    doc.insert(
        "parse_error_structural".into(),
        json!(counts.map(|c| c.parse_error_structural)),
    );
    if let Some(run) = index.and_then(|i| i.runs.first()) {
        doc.insert("last_index".into(), last_index(run));
        let history: Vec<Value> = index
            .map(|i| i.runs.iter().map(history_entry).collect())
            .unwrap_or_default();
        doc.insert("history".into(), Value::Array(history));
    }
    if let Some(u) = index.and_then(|i| i.usage.as_ref()) {
        doc.insert("usage".into(), usage_json(u));
    }
    Value::Object(doc)
}

/// Opens the database read-only and reads what `status` reports. A database that cannot be
/// read for any reason other than its schema counts as missing (with a note on stderr), so a
/// poller always gets a report.
fn read_index(db: &Path) -> Read {
    if !db.exists() {
        return Read::Missing;
    }
    match read_index_inner(db) {
        Ok(i) => Read::Index(Box::new(i)),
        Err(StoreError::SchemaVersion { .. }) => Read::Unsupported,
        Err(e) => {
            eprintln!("orch-index: cannot read {}: {}", clean(&path_text(db)), e);
            Read::Missing
        }
    }
}

fn read_index_inner(db: &Path) -> Result<Index, StoreError> {
    let store = Store::open_read_only(db)?;
    let commit = store.meta("commit")?;
    if commit.is_none() {
        // Created by something other than a finished `index` (e.g. `stats` on a fresh path).
        return Ok(Index {
            commit: None,
            extractor: None,
            counts: None,
            runs: Vec::new(),
            usage: None,
        });
    }
    let runs = if store.has_table("index_runs")? {
        store.index_runs(HISTORY)?
    } else {
        Vec::new()
    };
    let usage = if std::env::var_os("ORCH_INDEX_NO_USAGE").is_some_and(|v| v == "1") {
        None
    } else {
        store
            .usage_summary(today() - (WINDOW_DAYS - 1))?
            .filter(|u| u.queries > 0)
    };
    Ok(Index {
        commit,
        extractor: store.meta("extractor")?,
        counts: store.cached_counts()?,
        runs,
        usage,
    })
}

/// The state of an index. `outdated` wins over `stale`: only a re-index fixes it.
fn state_of(
    indexed: Option<&str>,
    head: Option<&str>,
    indexed_extractor: Option<&str>,
    binary_extractor: &str,
) -> State {
    if indexed.is_none() {
        State::None
    } else if indexed_extractor != Some(binary_extractor) {
        State::Outdated
    } else if indexed != head {
        State::Stale
    } else {
        State::Current
    }
}

/// 0 when the commits are equal; else the commits between them if the indexed one is an
/// ancestor of HEAD; `null` when it is not, or on any git error.
fn commits_behind(repo: &Repo, indexed: Option<&str>, head: Option<&str>) -> Option<u64> {
    let (indexed, head) = (indexed?, head?);
    if indexed == head {
        return Some(0);
    }
    match repo.is_ancestor(indexed, head) {
        Ok(true) => repo.count_range(indexed, head).ok(),
        _ => None,
    }
}

fn last_index(run: &IndexRun) -> Value {
    let files_per_sec = match (run.extracted, run.extract_seconds) {
        #[allow(clippy::cast_precision_loss)]
        (Some(n), Some(s)) if s > 0.0 => Some(round(n as f64 / s, 1)),
        _ => None,
    };
    json!({
        "kind": clean(&run.kind),
        "seconds": run.seconds,
        "list_seconds": run.list_seconds,
        "extract_seconds": run.extract_seconds,
        "files_total": run.files_total,
        "extracted": run.extracted,
        "reused": run.reused,
        "skipped_large": run.skipped_large,
        "files_per_sec": files_per_sec,
        "db_bytes_before": run.db_bytes_before,
        "db_bytes_after": run.db_bytes_after,
    })
}

fn history_entry(run: &IndexRun) -> Value {
    json!({
        "at": rfc3339(run.at),
        "kind": clean(&run.kind),
        "seconds": run.seconds,
        "extracted": run.extracted,
    })
}

fn usage_json(u: &UsageSummary) -> Value {
    let by_command: Map<String, Value> = u
        .per_command
        .iter()
        .map(|(cmd, l)| {
            (
                clean(cmd),
                json!({"n": l.n, "p50_ms": l.percentile(0.5), "p95_ms": l.percentile(0.95)}),
            )
        })
        .collect();
    let hits = u.labels.total();
    let label_mix = if hits == 0 {
        Value::Null
    } else {
        let mut mix = Map::new();
        for (key, n) in [
            ("resolved", u.labels.resolved),
            ("syntactic", u.labels.syntactic),
            ("ambiguous", u.labels.ambiguous),
            ("parse_errors", u.labels.parse_errors),
        ] {
            mix.insert(key.into(), json!(round(ratio(n, hits), 3)));
        }
        if u.labels.compiler > 0 {
            mix.insert(
                "compiler".into(),
                json!(round(ratio(u.labels.compiler, hits), 3)),
            );
        }
        Value::Object(mix)
    };
    json!({
        "window_days": WINDOW_DAYS,
        "queries": u.queries,
        "by_command": by_command,
        "zero_hit_pct": round(100.0 * ratio(u.zero_hit, u.queries), 1),
        "stale_answer_pct": round(100.0 * ratio(u.stale_answers, u.queries), 1),
        "label_mix": label_mix,
        "last_query_at": (u.last_at > 0).then(|| rfc3339(u.last_at)),
    })
}

/// A short text form of the report, built from the (already sanitized) JSON document.
fn render_text(doc: &Value) -> String {
    let get = |k: &str| &doc[k];
    let text = |v: &Value| v.as_str().map_or_else(|| "-".to_owned(), str::to_owned);
    let num = |v: &Value| v.as_i64().map_or_else(|| "?".to_owned(), |n| n.to_string());
    let short = |v: &Value| v.as_str().map_or("-", |s| &s[..s.len().min(10)]).to_owned();
    let mut lines = Vec::new();

    let mut head = format!("index: {}", text(get("state")));
    if get("indexing") == &Value::Bool(true) {
        head.push_str(" (indexing now)");
    }
    if let Some(at) = get("indexed_at").as_str() {
        head.push_str(", indexed ");
        head.push_str(at);
    }
    lines.push(head);

    let behind = match &get("commits_behind") {
        Value::Number(n) => format!(" ({n} behind)"),
        _ => String::new(),
    };
    lines.push(format!(
        "commits: indexed {}, HEAD {}{behind}",
        short(get("indexed_commit")),
        short(get("head_commit"))
    ));

    let c = get("counts");
    if c.is_object() {
        lines.push(format!(
            "counts: {} files, {} symbols, {} refs, {} includes, {} bases; {} files with parse errors",
            num(&c["files"]),
            num(&c["symbols"]),
            num(&c["refs"]),
            num(&c["includes"]),
            num(&c["bases"]),
            num(get("parse_error_files")),
        ));
    }
    let l = get("last_index");
    if l.is_object() {
        let secs = l["seconds"]
            .as_f64()
            .map_or_else(|| "?".to_owned(), |s| format!("{s:.1}"));
        lines.push(format!(
            "last run: {} in {secs}s, {} extracted, {} reused",
            text(&l["kind"]),
            num(&l["extracted"]),
            num(&l["reused"]),
        ));
    }
    let u = get("usage");
    if u.is_object() {
        lines.push(format!(
            "usage ({} days): {} queries, {}% zero-hit, {}% stale",
            num(&u["window_days"]),
            num(&u["queries"]),
            u["zero_hit_pct"],
            u["stale_answer_pct"],
        ));
    }
    lines.join("\n")
}

fn db_bytes(db: &Path) -> Option<u64> {
    let main = std::fs::metadata(db).ok()?.len();
    let mut wal = OsString::from(db.as_os_str());
    wal.push("-wal");
    Some(main + std::fs::metadata(PathBuf::from(wal)).map_or(0, |m| m.len()))
}

fn today() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .map_or(0, |s| s / 86_400)
}

fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn clean(s: &str) -> String {
    sanitize(s).into_owned()
}

fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| json!(clean(s)))
}

#[allow(clippy::cast_precision_loss)]
fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// `YYYY-MM-DDTHH:MM:SSZ` for unix seconds (proleptic Gregorian, UTC).
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_431_200), "2026-10-31T07:20:00Z");
    }

    #[test]
    fn state_rules() {
        let (a, b) = (Some("aaa"), Some("bbb"));
        let v = Some(EXTRACTOR_VERSION);
        assert_eq!(state_of(a, a, v, EXTRACTOR_VERSION), State::Current);
        assert_eq!(state_of(a, b, v, EXTRACTOR_VERSION), State::Stale);
        assert_eq!(
            state_of(a, a, Some("old"), EXTRACTOR_VERSION),
            State::Outdated
        );
        assert_eq!(
            state_of(a, b, Some("old"), EXTRACTOR_VERSION),
            State::Outdated
        );
        assert_eq!(state_of(None, b, None, EXTRACTOR_VERSION), State::None);
    }
}
