//! Per-day query usage aggregates: latency histogram, hit counts and label mix, one row per
//! (UTC day, command). Nothing about a query itself (name, path, text) is stored.

use std::collections::BTreeMap;
use std::time::Duration;

use rusqlite::{Transaction, TransactionBehavior, params};

use crate::{Result, Store};

/// Upper bounds in milliseconds of histogram buckets 0..=10; bucket 11 is everything above.
pub const LATENCY_BOUNDS_MS: [u64; 11] = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000];

/// Number of histogram buckets (`LATENCY_BOUNDS_MS` plus the unbounded last one).
pub const LATENCY_BUCKETS: usize = LATENCY_BOUNDS_MS.len() + 1;

/// Days of usage kept, counting the current one.
const USAGE_DAYS_KEPT: i64 = 30;

/// The bucket `ms` falls in: the first whose bound is at least `ms`, else the last.
#[must_use]
pub fn latency_bucket(ms: u64) -> usize {
    LATENCY_BOUNDS_MS
        .iter()
        .position(|&b| ms <= b)
        .unwrap_or(LATENCY_BOUNDS_MS.len())
}

/// How a hit is labelled for the label mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelClass {
    Resolved,
    Syntactic,
    Ambiguous,
    ParseErrors,
    Compiler,
}

/// Hits per [`LabelClass`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LabelCounts {
    pub resolved: u64,
    pub syntactic: u64,
    pub ambiguous: u64,
    pub parse_errors: u64,
    pub compiler: u64,
}

impl LabelCounts {
    pub const fn add(&mut self, class: LabelClass) {
        match class {
            LabelClass::Resolved => self.resolved += 1,
            LabelClass::Syntactic => self.syntactic += 1,
            LabelClass::Ambiguous => self.ambiguous += 1,
            LabelClass::ParseErrors => self.parse_errors += 1,
            LabelClass::Compiler => self.compiler += 1,
        }
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.resolved + self.syntactic + self.ambiguous + self.parse_errors + self.compiler
    }

    const fn plus(&mut self, o: &Self) {
        self.resolved += o.resolved;
        self.syntactic += o.syntactic;
        self.ambiguous += o.ambiguous;
        self.parse_errors += o.parse_errors;
        self.compiler += o.compiler;
    }
}

/// One answered query. `day` is `at / 86400` (UTC); `at` is unix seconds.
#[derive(Debug, Clone, Copy)]
pub struct UsageSample<'a> {
    pub day: i64,
    pub at: i64,
    pub command: &'a str,
    pub ms: u64,
    /// Total hits before any display limit.
    pub hits: u64,
    pub stale: bool,
    pub labels: LabelCounts,
}

/// Latency figures over some set of queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyStats {
    pub n: u64,
    pub hist: [u64; LATENCY_BUCKETS],
    pub total_ms: u64,
    pub max_ms: u64,
}

impl LatencyStats {
    /// The `q`-quantile (0..=1) in ms: the upper bound of the bucket holding rank `ceil(q * n)`,
    /// capped at `max_ms`; the unbounded bucket reports `max_ms`. 0 when there are no queries.
    #[must_use]
    pub fn percentile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let rank = ((q * self.n as f64).ceil().max(1.0) as u64).min(self.n);
        let mut seen = 0;
        for (i, &c) in self.hist.iter().enumerate() {
            seen += c;
            if seen >= rank {
                let bound = LATENCY_BOUNDS_MS.get(i).copied().unwrap_or(self.max_ms);
                return bound.min(self.max_ms);
            }
        }
        self.max_ms
    }

    fn plus(&mut self, o: &Self) {
        self.n += o.n;
        for (a, b) in self.hist.iter_mut().zip(o.hist) {
            *a += b;
        }
        self.total_ms += o.total_ms;
        self.max_ms = self.max_ms.max(o.max_ms);
    }
}

/// Usage over a window of days.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageSummary {
    pub queries: u64,
    pub zero_hit: u64,
    pub stale_answers: u64,
    pub hits_total: u64,
    pub labels: LabelCounts,
    /// Unix seconds of the latest recorded query; 0 if there are none.
    pub last_at: i64,
    /// All commands together.
    pub latency: LatencyStats,
    pub per_command: BTreeMap<String, LatencyStats>,
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl Store {
    /// Adds one query to its (day, command) row and drops days older than the retention window.
    /// Best-effort: for the call the busy timeout is 0 and WAL auto-checkpointing is off (both
    /// restored after), so a database that is being written fails at once and the commit never
    /// does checkpoint I/O inherited from a large `index` write; a query is never made to wait.
    pub fn record_usage(&self, s: &UsageSample<'_>) -> Result<()> {
        let previous_timeout: i64 = self
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        let previous_checkpoint: i64 =
            self.conn
                .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?;
        let result = self
            .conn
            .busy_timeout(Duration::ZERO)
            .map_err(Into::into)
            .and_then(|()| {
                self.conn
                    .pragma_update(None, "wal_autocheckpoint", 0)
                    .map_err(Into::into)
            })
            .and_then(|()| self.record_usage_inner(s));
        let restored_checkpoint =
            self.conn
                .pragma_update(None, "wal_autocheckpoint", previous_checkpoint);
        let restored_timeout = self
            .conn
            .busy_timeout(Duration::from_millis(to_u64(previous_timeout)));
        result?;
        restored_checkpoint?;
        restored_timeout?;
        Ok(())
    }

    fn record_usage_inner(&self, s: &UsageSample<'_>) -> Result<()> {
        let h = format!("h{}", latency_bucket(s.ms));
        let sql = format!(
            "INSERT INTO usage (day, command, count, total_ms, max_ms, {h}, hits_total, zero_hit,
                                stale_answers, lab_resolved, lab_syntactic, lab_ambiguous,
                                lab_parse_errors, lab_compiler, last_at)
             VALUES (?1, ?2, 1, ?3, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (day, command) DO UPDATE SET
                 count = count + 1,
                 total_ms = total_ms + excluded.total_ms,
                 max_ms = max(max_ms, excluded.max_ms),
                 {h} = {h} + 1,
                 hits_total = hits_total + excluded.hits_total,
                 zero_hit = zero_hit + excluded.zero_hit,
                 stale_answers = stale_answers + excluded.stale_answers,
                 lab_resolved = lab_resolved + excluded.lab_resolved,
                 lab_syntactic = lab_syntactic + excluded.lab_syntactic,
                 lab_ambiguous = lab_ambiguous + excluded.lab_ambiguous,
                 lab_parse_errors = lab_parse_errors + excluded.lab_parse_errors,
                 lab_compiler = lab_compiler + excluded.lab_compiler,
                 last_at = max(last_at, excluded.last_at)"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            &sql,
            params![
                s.day,
                s.command,
                to_i64(s.ms),
                to_i64(s.hits),
                i64::from(s.hits == 0),
                i64::from(s.stale),
                to_i64(s.labels.resolved),
                to_i64(s.labels.syntactic),
                to_i64(s.labels.ambiguous),
                to_i64(s.labels.parse_errors),
                to_i64(s.labels.compiler),
                s.at,
            ],
        )?;
        tx.execute(
            "DELETE FROM usage WHERE day < ?1",
            [s.day - (USAGE_DAYS_KEPT - 1)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Usage over days `>= since_day`; `None` when the database has no `usage` table (a v4
    /// index not yet migrated).
    pub fn usage_summary(&self, since_day: i64) -> Result<Option<UsageSummary>> {
        if !self.has_table("usage")? {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT command, count, total_ms, max_ms, h0, h1, h2, h3, h4, h5, h6, h7, h8, h9, h10,
                    h11, hits_total, zero_hit, stale_answers, lab_resolved, lab_syntactic,
                    lab_ambiguous, lab_parse_errors, lab_compiler, last_at
             FROM usage WHERE day >= ?1",
        )?;
        let mut rows = stmt.query([since_day])?;
        let mut out = UsageSummary::default();
        while let Some(r) = rows.next()? {
            let get = |i: usize| -> rusqlite::Result<i64> { r.get(i) };
            let command: String = r.get(0)?;
            let mut lat = LatencyStats {
                n: to_u64(get(1)?),
                total_ms: to_u64(get(2)?),
                max_ms: to_u64(get(3)?),
                ..LatencyStats::default()
            };
            for (i, slot) in lat.hist.iter_mut().enumerate() {
                *slot = to_u64(get(4 + i)?);
            }
            out.queries += lat.n;
            out.hits_total += to_u64(get(16)?);
            out.zero_hit += to_u64(get(17)?);
            out.stale_answers += to_u64(get(18)?);
            out.labels.plus(&LabelCounts {
                resolved: to_u64(get(19)?),
                syntactic: to_u64(get(20)?),
                ambiguous: to_u64(get(21)?),
                parse_errors: to_u64(get(22)?),
                compiler: to_u64(get(23)?),
            });
            out.last_at = out.last_at.max(get(24)?);
            out.latency.plus(&lat);
            out.per_command.entry(command).or_default().plus(&lat);
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use rusqlite::Connection;

    use super::*;
    use crate::StoreError;

    fn sample(day: i64, command: &str, ms: u64, hits: u64) -> UsageSample<'_> {
        UsageSample {
            day,
            at: day * 86400 + 5,
            command,
            ms,
            hits,
            stale: false,
            labels: LabelCounts::default(),
        }
    }

    fn row_count(store: &Store, day: i64) -> Option<i64> {
        store
            .conn
            .query_row("SELECT count FROM usage WHERE day = ?1", [day], |r| {
                r.get(0)
            })
            .ok()
    }

    #[test]
    fn bucket_edges() {
        for (ms, bucket) in [(0, 0), (1, 0), (2, 1), (3, 2), (2000, 10), (2001, 11)] {
            assert_eq!(latency_bucket(ms), bucket, "{ms} ms");
        }
    }

    #[test]
    fn percentile_from_histogram() {
        // 10 queries: 6 in bucket 0 (<=1), 3 in bucket 3 (<=10), 1 in the unbounded bucket.
        let mut lat = LatencyStats {
            n: 10,
            max_ms: 7000,
            ..LatencyStats::default()
        };
        lat.hist[0] = 6;
        lat.hist[3] = 3;
        lat.hist[11] = 1;
        assert_eq!(lat.percentile(0.5), 1); // rank 5 -> bucket 0
        assert_eq!(lat.percentile(0.7), 10); // rank 7 -> bucket 3
        assert_eq!(lat.percentile(0.95), 7000); // rank 10 -> unbounded bucket -> max_ms
        // The cap: every query is fast, so the bound 10 is above the observed max.
        let mut small = LatencyStats {
            n: 4,
            max_ms: 8,
            ..LatencyStats::default()
        };
        small.hist[3] = 4;
        assert_eq!(small.percentile(0.5), 8);
        assert_eq!(small.percentile(0.95), 8);
        assert_eq!(LatencyStats::default().percentile(0.5), 0);
    }

    #[test]
    fn retention_drops_days_older_than_29() {
        let store = Store::in_memory().unwrap();
        let d = 20_000;
        store.record_usage(&sample(d - 30, "def", 1, 1)).unwrap();
        store.record_usage(&sample(d - 29, "def", 1, 1)).unwrap();
        assert_eq!(row_count(&store, d - 30), Some(1));
        store.record_usage(&sample(d, "def", 1, 1)).unwrap();
        assert_eq!(row_count(&store, d - 30), None);
        assert_eq!(row_count(&store, d - 29), Some(1));
        assert_eq!(row_count(&store, d), Some(1));
    }

    #[test]
    fn upsert_and_summary() {
        let store = Store::in_memory().unwrap();
        let d = 20_000;
        let mut a = sample(d, "def", 3, 0);
        a.stale = true;
        store.record_usage(&a).unwrap();
        let mut b = sample(d, "def", 400, 3);
        b.labels = LabelCounts {
            resolved: 2,
            ambiguous: 1,
            ..LabelCounts::default()
        };
        store.record_usage(&b).unwrap();
        store.record_usage(&sample(d - 1, "refs", 1, 1)).unwrap();
        store.record_usage(&sample(d - 9, "refs", 1, 1)).unwrap();

        let s = store.usage_summary(d - 6).unwrap().unwrap();
        assert_eq!((s.queries, s.zero_hit, s.stale_answers), (3, 1, 1));
        assert_eq!(s.hits_total, 4);
        assert_eq!(s.labels.resolved, 2);
        assert_eq!(s.labels.ambiguous, 1);
        assert_eq!(s.last_at, d * 86400 + 5);
        assert_eq!(s.latency.max_ms, 400);
        assert_eq!(s.latency.total_ms, 404);
        assert_eq!(s.latency.hist[2], 1); // 3 ms
        assert_eq!(s.latency.hist[8], 1); // 400 ms
        assert_eq!(s.per_command["def"].n, 2);
        assert_eq!(s.per_command["refs"].n, 1);
    }

    #[test]
    fn busy_database_fails_fast_and_restores_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.db");
        let store = Store::open(&path).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let before: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        let start = Instant::now();
        let err = store.record_usage(&sample(20_000, "def", 1, 1));
        assert!(matches!(err, Err(StoreError::Sqlite(_))), "{err:?}");
        assert!(start.elapsed() < Duration::from_millis(50));
        let after: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        assert!(after > 0);
        other.execute_batch("ROLLBACK").unwrap();
        store.record_usage(&sample(20_000, "def", 1, 1)).unwrap();
    }

    #[test]
    fn usage_write_restores_wal_autocheckpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.db");
        let store = Store::open(&path).unwrap();
        let read = |store: &Store| -> i64 {
            store
                .conn
                .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
                .unwrap()
        };
        store
            .conn
            .pragma_update(None, "wal_autocheckpoint", 37)
            .unwrap();
        assert_eq!(read(&store), 37);
        store.record_usage(&sample(20_000, "def", 1, 1)).unwrap();
        assert_eq!(read(&store), 37, "restored after success");
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(store.record_usage(&sample(20_000, "def", 1, 1)).is_err());
        assert_eq!(read(&store), 37, "restored after a busy error");
        other.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn summary_is_none_without_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
                 INSERT INTO meta VALUES ('schema', '4');",
            )
            .unwrap();
        }
        let store = Store::open_read_only(&path).unwrap();
        assert_eq!(store.usage_summary(0).unwrap(), None);
    }
}
