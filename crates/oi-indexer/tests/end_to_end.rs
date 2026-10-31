use std::path::Path;
use std::process::Command;

use oi_git::Repo;
use oi_indexer::Indexer;
use oi_model::{Provenance, SymbolKind};
use oi_query::Engine;
use oi_store::{Filter, Store};

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
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn write(dir: &Path, path: &str, body: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    write(
        d,
        "net/Listener.h",
        "class nsIStreamListener { public: virtual int OnStartRequest(nsIRequest* r) = 0; };\n",
    );
    write(
        d,
        "net/Channel.h",
        "#include \"Listener.h\"\nclass Channel : public nsIStreamListener {\n public:\n  int OnStartRequest(nsIRequest* r) override;\n};\n",
    );
    write(
        d,
        "net/Channel.cpp",
        "#include \"net/Channel.h\"\nint Channel::OnStartRequest(nsIRequest* r) { return Helper(r); }\n",
    );
    write(
        d,
        "dom/Loader.cpp",
        "#include \"mozilla/Channel.h\"\nclass Loader : public Channel {\n  int OnStartRequest(nsIRequest* r) override { return 0; }\n  int OnStartRequest(int ipc);\n};\n",
    );
    write(d, "other/Channel.h", "struct Unrelated {};\n");
    write(d, "README.md", "not code\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "one"]);
    tmp
}

#[test]
fn index_query_and_update() {
    let tmp = fixture();
    let repo = Repo::new(tmp.path());
    let mut store = Store::in_memory().unwrap();

    let stats = Indexer::new(repo.clone())
        .update(&mut store, "HEAD")
        .unwrap();
    assert_eq!(
        (stats.tree_files, stats.indexable, stats.extracted),
        (6, 5, 5)
    );

    let engine = Engine::new(&store, &repo);
    let defs = engine
        .def("Channel", &[SymbolKind::Class], Filter::default())
        .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!((defs[0].path.as_str(), defs[0].line), ("net/Channel.h", 2));

    // Two files are named Channel.h; the includes are classified accordingly.
    let inc = engine
        .includers("net/Channel.h", Filter::default())
        .unwrap();
    let got: Vec<_> = inc
        .iter()
        .map(|h| (h.path.as_str(), h.provenance, h.note.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("dom/Loader.cpp", Provenance::Syntactic, Some("basename")),
            ("net/Channel.cpp", Provenance::Resolved, None),
        ]
    );

    let over = engine
        .overrides(
            "OnStartRequest",
            Some("nsIStreamListener"),
            Some("(nsIRequest *)"),
            Filter::default(),
        )
        .unwrap();
    let classes: Vec<_> = over.iter().map(|h| h.detail.as_str()).collect();
    assert_eq!(classes, ["Channel (nsIRequest*)", "Loader (nsIRequest*)"]);
    assert_eq!(over[0].note.as_deref(), Some("declared+defined"));

    let mut calls = engine
        .refs("Helper", Some(oi_model::RefKind::Call), Filter::default())
        .unwrap();
    engine.attach_snippets(&mut calls).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].detail, "call in Channel::OnStartRequest");
    assert!(calls[0].snippet.as_deref().unwrap().contains("Helper(r)"));

    // Incremental: change one file, delete one; only those are touched.
    let d = tmp.path();
    write(
        d,
        "net/Channel.cpp",
        "#include \"net/Channel.h\"\n\nint Channel::OnStartRequest(nsIRequest* r) { return 1; }\n",
    );
    std::fs::remove_file(d.join("other/Channel.h")).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "two"]);
    let stats = Indexer::new(repo.clone())
        .update(&mut store, "HEAD")
        .unwrap();
    assert_eq!((stats.extracted, stats.unchanged, stats.removed), (1, 3, 2));

    let engine = Engine::new(&store, &repo);
    assert!(
        engine
            .refs("Helper", None, Filter::default())
            .unwrap()
            .is_empty()
    );
    let def = engine
        .def(
            "Channel::OnStartRequest",
            &[],
            Filter {
                path: Some("net/Channel.cpp"),
                lang: None,
            },
        )
        .unwrap();
    assert_eq!(def[0].line, 3);
    assert!(!engine.freshness().unwrap().is_stale());
    // With the other Channel.h gone, the include from dom/ is still only a basename match.
    let inc = engine
        .includers(
            "net/Channel.h",
            Filter {
                path: Some("dom/"),
                lang: None,
            },
        )
        .unwrap();
    assert_eq!(inc[0].note.as_deref(), Some("basename"));
}

#[test]
fn languages_imports_and_damage_labels() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    write(d, "web/src/util.ts", "export function log(x: string) {}\n");
    write(d, "web/src/widgets/index.ts", "export class Widget {}\n");
    write(
        d,
        "web/src/app.tsx",
        "import { log } from './util';\nimport { Widget } from './widgets';\nimport React from 'react';\nexport function App() { log('x'); return <div/>; }\n",
    );
    write(d, "web/other/util.ts", "export const z = 1;\n");
    write(d, "web/other/main.ts", "import { z } from './util';\n");
    write(
        d,
        "core/src/lib.rs",
        "use crate::store::Store;\npub trait Sink { fn put(&self); }\nimpl Sink for Store { fn put(&self) {} }\n",
    );
    write(
        d,
        "svc/Svc.cs",
        "using System.Linq;\nnamespace Svc { class Job : IRunnable { public void Run() {} } }\n",
    );
    // A declaration-level syntax error before a definition: hits from this file get labelled.
    write(
        d,
        "bad/broken.cc",
        "class Broken {\n  int x\n  int y;\n};\n}}}\nvoid Later() {}\n",
    );
    write(d, "bad/clean.cc", "void Later2() {}\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "one"]);

    let repo = Repo::new(d);
    let mut store = Store::in_memory().unwrap();
    let stats = Indexer::new(repo.clone())
        .update(&mut store, "HEAD")
        .unwrap();
    assert_eq!((stats.indexable, stats.extracted), (9, 9));
    let engine = Engine::new(&store, &repo);

    let inc = engine
        .includers("web/src/util.ts", Filter::default())
        .unwrap();
    let got: Vec<_> = inc
        .iter()
        .map(|h| (h.path.as_str(), h.provenance))
        .collect();
    assert_eq!(got, [("web/src/app.tsx", Provenance::Resolved)]);
    let inc = engine
        .includers("web/src/widgets/index.ts", Filter::default())
        .unwrap();
    assert_eq!(inc.len(), 1, "{inc:#?}");

    let imp = engine.importers("react", Filter::default()).unwrap();
    assert_eq!(imp[0].path, "web/src/app.tsx");
    assert_eq!(
        engine.importers("store::Store", Filter::default()).unwrap()[0].path,
        "core/src/lib.rs"
    );
    assert_eq!(
        engine.importers("System.Linq", Filter::default()).unwrap()[0].path,
        "svc/Svc.cs"
    );
    assert!(
        engine
            .importers("Store::store", Filter::default())
            .unwrap()
            .is_empty()
    );

    let over = engine
        .overrides("put", Some("Sink"), None, Filter::default())
        .unwrap();
    assert_eq!(over.len(), 1);
    assert_eq!(over[0].detail, "Store (&self)");
    assert_eq!(
        engine
            .def("Job", &[SymbolKind::Class], Filter::default())
            .unwrap()[0]
            .detail,
        "class Svc"
    );

    let later = engine.def("Later", &[], Filter::default()).unwrap();
    assert_eq!(later[0].note.as_deref(), Some("parse-errors"), "{later:#?}");
    let clean = engine.def("Later2", &[], Filter::default()).unwrap();
    // The language filter: `Store` is referenced only from Rust.
    let rust_only = Filter {
        path: None,
        lang: Some(oi_model::Lang::Rust),
    };
    assert_eq!(engine.refs("Store", None, rust_only).unwrap().len(), 2);
    let cs_only = Filter {
        path: None,
        lang: Some(oi_model::Lang::CSharp),
    };
    assert!(engine.refs("Store", None, cs_only).unwrap().is_empty());
    assert_eq!(clean[0].note, None);
}

/// A repository built to hurt the indexer: none of this may crash it, stall it or let file
/// contents forge output.
#[test]
#[cfg(unix)]
fn hostile_repository() {
    use std::os::unix::ffi::OsStrExt;
    use std::time::Instant;

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    // Deep nesting: quadratic in a naive walker, and a stack overflow in a recursive one.
    let depth = 3000;
    write(
        d,
        "deep.cc",
        &format!(
            "{}int x;{}",
            "namespace a{".repeat(depth),
            "}".repeat(depth)
        ),
    );
    let uses = 15_000;
    write(
        d,
        "deep.rs",
        &format!("use a::{}x{};\n", "{b::".repeat(uses), "}".repeat(uses)),
    );
    // Over the size limit: recorded, never read into memory.
    write(d, "big.cc", &"int y;\n".repeat(40_000));
    // A file name that would forge an extra result row if printed raw.
    write(d, "x\nFAKE.c", "void forged() {}\n");
    git(d, &["add", "-A"]);
    // Two paths that differ only in invalid UTF-8 bytes (and would collide after lossy decoding).
    let blob = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(["hash-object", "-w", "deep.cc"])
        .output()
        .unwrap();
    let blob = String::from_utf8(blob.stdout).unwrap();
    for bad in [&b"a\xfe.c"[..], &b"a\xff.c"[..]] {
        let mut info = b"100644,".to_vec();
        info.extend_from_slice(blob.trim().as_bytes());
        info.push(b',');
        info.extend_from_slice(bad);
        let status = Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(std::ffi::OsStr::from_bytes(&info))
            .status()
            .unwrap();
        assert!(status.success());
    }
    git(d, &["commit", "-q", "-m", "hostile"]);

    let repo = Repo::new(d);
    let mut store = Store::in_memory().unwrap();
    let start = Instant::now();
    let stats = Indexer::new(repo.clone())
        .with_max_file_bytes(200_000)
        .update(&mut store, "HEAD")
        .unwrap();
    assert!(start.elapsed().as_secs() < 20, "took {:?}", start.elapsed());
    assert_eq!(stats.skipped_non_utf8, 2);
    assert_eq!(stats.skipped_large, 1);
    assert_eq!(stats.failed, 0);

    let engine = Engine::new(&store, &repo);
    // Past the depth cap the walk stops, and hits from the file are flagged.
    let ns = engine
        .def("a", &[SymbolKind::Namespace], Filter::default())
        .unwrap();
    assert!(!ns.is_empty() && ns.len() < depth);
    assert_eq!(ns[0].note.as_deref(), Some("parse-errors"));
    let mut forged = engine.def("forged", &[], Filter::default()).unwrap();
    forged[0].sanitize();
    assert_eq!(forged[0].path, "x\\u{a}FAKE.c");
}

#[test]
fn review_regressions() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    write(
        d,
        "a/x.ts",
        "export class Widget extends Base { draw() {} }\nexport class Alpha extends Base { draw() {} }\n",
    );
    write(
        d,
        "b/y.ts",
        "export class Widget extends Base { draw() {} }\n",
    );
    write(d, "web/lib/index.ts", "export const v = 1;\n");
    write(d, "web/lib/sub/use.ts", "import { v } from '..';\n");
    write(d, "src/main.rs", "use std::io::{self, Write};\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "one"]);
    let repo = Repo::new(d);

    // Two concurrent full updates on one database: both succeed, one waits for the other.
    let db = d.join("index.db");
    let runs: Vec<_> = (0..2)
        .map(|_| {
            let (repo, db) = (repo.clone(), db.clone());
            std::thread::spawn(move || {
                let mut store = Store::open(&db).unwrap();
                Indexer::new(repo)
                    .update(&mut store, "HEAD")
                    .map(|s| s.indexable)
            })
        })
        .collect();
    for r in runs {
        assert_eq!(r.join().unwrap().unwrap(), 5);
    }
    let store = Store::open(&db).unwrap();
    let engine = Engine::new(&store, &repo);

    // Same qualified name in two files: two classes, not one.
    let over = engine
        .overrides("draw", Some("Base"), None, Filter::default())
        .unwrap();
    let rows: Vec<_> = over
        .iter()
        .map(|h| (h.detail.as_str(), h.path.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("Alpha ()", "a/x.ts"),
            ("Widget ()", "a/x.ts"),
            ("Widget ()", "b/y.ts")
        ]
    );
    let importers = engine.importers("std::io", Filter::default()).unwrap();
    assert_eq!(importers.len(), 1);
    let inc = engine
        .includers("web/lib/index.ts", Filter::default())
        .unwrap();
    assert_eq!(inc.len(), 1, "{inc:#?}");
    assert!(!engine.freshness().unwrap().is_stale());
}

#[test]
fn index_runs_cached_counts_and_lock() {
    let tmp = fixture();
    let repo = Repo::new(tmp.path());
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("index.db");
    let mut store = Store::open(&db).unwrap();
    let indexer = Indexer::new(repo);

    let first = indexer.update(&mut store, "HEAD").unwrap();
    assert!(first.full);
    // A sibling test thread may be between fork and exec of a `git` child, which briefly shares
    // the lock's open file description; the lock is gone once that child has exec'd.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while oi_store::is_indexing(&db) {
        assert!(
            std::time::Instant::now() < deadline,
            "lock not released after update"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let runs = store.index_runs(10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].kind, "full");
    assert_eq!(runs[0].extracted, Some(5));
    assert!(runs[0].db_bytes_after.unwrap() > 0);

    let second = indexer.update(&mut store, "HEAD").unwrap();
    assert!(!second.full);
    let runs = store.index_runs(10).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].kind, "incremental");
    assert_eq!(runs[0].extracted, Some(0));
    assert_eq!(runs[0].reused, Some(5));
    assert_eq!(runs[1].kind, "full");

    // The cache equals a live count.
    let cached = store.cached_counts().unwrap().expect("counts cached");
    let live: std::collections::HashMap<_, _> = store.counts().unwrap().into_iter().collect();
    assert_eq!(cached.files, live["files"]);
    assert_eq!(cached.symbols, live["symbols"]);
    assert_eq!(cached.refs, live["refs"]);
    assert_eq!(cached.includes, live["includes"]);
    assert_eq!(cached.bases, live["bases"]);
    assert_eq!(cached.names, live["names"]);
    let (errors, structural) = store.files_with_errors().unwrap();
    assert_eq!(
        (cached.parse_error_files, cached.parse_error_structural),
        (errors, structural)
    );
    assert_eq!(cached.languages.values().sum::<i64>(), cached.files);
}

#[test]
fn git_ancestry_helpers() {
    let tmp = fixture();
    let d = tmp.path();
    let repo = Repo::new(d);
    let first = repo.rev_parse("HEAD").unwrap();
    write(d, "README.md", "changed\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "two"]);
    let second = repo.rev_parse("HEAD").unwrap();

    assert!(repo.is_ancestor(&first, &second).unwrap());
    assert!(!repo.is_ancestor(&second, &first).unwrap());
    assert!(repo.is_ancestor(&first, &first).unwrap());
    assert_eq!(repo.count_range(&first, &second).unwrap(), 1);
    assert_eq!(repo.count_range(&second, &first).unwrap(), 0);
    assert_eq!(repo.toplevel().unwrap(), d.canonicalize().unwrap());
    // An unknown revision is an error, not "false".
    assert!(repo.is_ancestor(&first, &"0".repeat(40)).is_err());
}
