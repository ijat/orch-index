//! `orch-index status` through the real binary, on throwaway git repositories.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_orch-index");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn write(dir: &Path, path: &str, body: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A repository with one commit and a little C++.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    write(
        d,
        "net/Channel.h",
        "class Channel {\n public:\n  int Open(int fd);\n};\n",
    );
    write(
        d,
        "net/Channel.cpp",
        "#include \"net/Channel.h\"\nint Channel::Open(int fd) { return Helper(fd); }\n",
    );
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "first"]);
    tmp
}

fn commit_change(dir: &Path, n: u32) {
    write(dir, &format!("net/Extra{n}.h"), "struct Extra {};\n");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", &format!("change {n}")]);
}

/// Runs the binary against `dir` with no usage opt-out unless `env` adds one.
fn run(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--repo")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .env_remove("ORCH_INDEX_NO_USAGE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn index(dir: &Path) {
    let out = run(dir, &["index"], &[]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn status_with(dir: &Path, env: &[(&str, &str)]) -> Value {
    let out = run(dir, &["status", "--json"], env);
    assert!(
        out.status.success(),
        "status: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("one JSON object on stdout")
}

fn status(dir: &Path) -> Value {
    status_with(dir, &[])
}

fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 20
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

#[test]
fn not_a_git_repository_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["status", "--json"], &[]);
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty());
}

#[test]
fn no_index_is_state_none_and_creates_nothing() {
    let tmp = fixture();
    let v = status(tmp.path());
    assert_eq!(v["state"], "none");
    assert_eq!(v["schema"], 1);
    assert_eq!(v["indexing"], false);
    assert!(v["db_bytes"].is_null());
    assert!(v["indexed_commit"].is_null());
    assert!(v["counts"].is_null());
    assert!(!tmp.path().join(".git/orch-index").exists());
    // The text form is read-only too.
    let out = run(tmp.path(), &["status"], &[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("index: none"));
    assert!(!tmp.path().join(".git/orch-index").exists());
}

#[test]
fn after_index_reports_current() {
    let tmp = fixture();
    index(tmp.path());
    let v = status(tmp.path());
    for key in [
        "schema",
        "state",
        "indexing",
        "indexed_commit",
        "head_commit",
        "commits_behind",
        "indexed_at",
        "db_bytes",
        "counts",
        "parse_error_files",
        "parse_error_structural",
    ] {
        assert!(v.get(key).is_some(), "missing required key {key}: {v}");
    }
    assert_eq!(v["state"], "current");
    assert_eq!(v["commits_behind"], 0);
    assert_eq!(v["indexed_commit"], v["head_commit"]);
    assert!(v["counts"]["files"].as_i64().unwrap() >= 1);
    assert!(is_rfc3339(v["indexed_at"].as_str().unwrap()), "{v}");
    assert_eq!(v["last_index"]["kind"], "full");
    assert_eq!(v["history"].as_array().unwrap().len(), 1);
    assert_eq!(v["extractor_version"], v["binary_extractor_version"]);
    assert!(v["db_bytes"].as_u64().unwrap() > 0);
    assert!(v["languages"]["cpp"].as_i64().unwrap() >= 1);
    assert!(v.get("usage").is_none(), "no queries yet");
    assert!(Path::new(v["repo"].as_str().unwrap()).is_absolute());
    assert!(Path::new(v["db"].as_str().unwrap()).is_absolute());
}

#[test]
fn new_commit_is_stale_then_reindex_is_incremental() {
    let tmp = fixture();
    index(tmp.path());
    commit_change(tmp.path(), 1);
    let v = status(tmp.path());
    assert_eq!(v["state"], "stale");
    assert_eq!(v["commits_behind"], 1);
    index(tmp.path());
    let v = status(tmp.path());
    assert_eq!(v["state"], "current");
    assert_eq!(v["history"][0]["kind"], "incremental");
    assert_eq!(v["history"].as_array().unwrap().len(), 2);
    assert_eq!(v["last_index"]["kind"], "incremental");
}

#[test]
fn unrelated_head_is_stale_with_unknown_distance() {
    let tmp = fixture();
    index(tmp.path());
    git(tmp.path(), &["checkout", "-q", "--orphan", "other"]);
    git(
        tmp.path(),
        &["commit", "-q", "--allow-empty", "-m", "orphan"],
    );
    let v = status(tmp.path());
    assert_eq!(v["state"], "stale");
    assert!(v["commits_behind"].is_null(), "{v}");
}

#[test]
fn usage_counts_queries_not_status_runs() {
    let tmp = fixture();
    index(tmp.path());
    let d = tmp.path();
    assert!(run(d, &["def", "Channel"], &[]).status.success());
    assert!(run(d, &["def", "Open"], &[]).status.success());
    assert!(run(d, &["callers", "Helper"], &[]).status.success());
    let v = status(d);
    assert_eq!(v["usage"]["queries"], 3, "{v}");
    assert_eq!(v["usage"]["by_command"]["def"]["n"], 2);
    assert_eq!(v["usage"]["by_command"]["callers"]["n"], 1);
    assert_eq!(v["usage"]["window_days"], 7);
    assert!(is_rfc3339(v["usage"]["last_query_at"].as_str().unwrap()));
    // Polling does not count.
    status(d);
    status(d);
    assert_eq!(status(d)["usage"]["queries"], 3);
    // Opt-out: the query still works but records nothing.
    let out = run(d, &["def", "Channel"], &[("ORCH_INDEX_NO_USAGE", "1")]);
    assert!(out.status.success());
    assert_eq!(status(d)["usage"]["queries"], 3);
    // And `usage` is absent from the report under the opt-out.
    let v = status_with(d, &[("ORCH_INDEX_NO_USAGE", "1")]);
    assert!(v.get("usage").is_none(), "{v}");
}

#[test]
fn old_schemas_report_null_counts_and_outdated_without_migrating() {
    let tmp = fixture();
    let d = tmp.path();
    let db = d.join("old.db");
    let set_schema = |v: &str| {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [v])
            .unwrap();
    };
    let schema = || -> String {
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let at_db = |d: &Path| -> Value {
        let out = run(d, &["--db", db.to_str().unwrap(), "status", "--json"], &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
             INSERT INTO meta VALUES ('schema', '4'), ('commit', 'abc'), ('extractor', 'old');",
        )
        .unwrap();
    }
    let v = at_db(d);
    assert!(v["counts"].is_null());
    for key in ["last_index", "history", "usage"] {
        assert!(v.get(key).is_none(), "{key} present on a v4 db");
    }
    assert_eq!(schema(), "4", "status must not migrate");

    set_schema("3");
    let v = at_db(d);
    assert_eq!(v["state"], "outdated");
    assert!(v["counts"].is_null());
    assert!(v["indexed_commit"].is_null());
    assert_eq!(schema(), "3");
}

#[test]
fn held_index_lock_shows_indexing() {
    let tmp = fixture();
    index(tmp.path());
    assert_eq!(status(tmp.path())["indexing"], false);
    let db = tmp.path().join(".git/orch-index/index.db");
    let store = oi_store::Store::open(&db).unwrap();
    let lock = store.index_lock().unwrap().expect("file-backed store");
    assert_eq!(status(tmp.path())["indexing"], true);
    drop(lock);
    assert_eq!(status(tmp.path())["indexing"], false);
}

#[cfg(unix)]
#[test]
fn db_path_is_sanitized() {
    let tmp = fixture();
    let db = tmp.path().join("evil\u{1b}[2Jname.db");
    let out = run(
        tmp.path(),
        &["--db", db.to_str().unwrap(), "status", "--json"],
        &[],
    );
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let shown = v["db"].as_str().unwrap();
    assert!(!shown.chars().any(char::is_control), "{shown:?}");
    assert!(shown.contains("\\u{1b}"), "{shown:?}");
    assert_eq!(v["state"], "none");
    assert!(!db.exists());
}
