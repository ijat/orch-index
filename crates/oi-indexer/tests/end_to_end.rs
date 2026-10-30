use std::path::Path;
use std::process::Command;

use oi_git::Repo;
use oi_indexer::Indexer;
use oi_model::{Provenance, SymbolKind};
use oi_query::Engine;
use oi_store::Store;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
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
    write(d, "net/Listener.h", "class nsIStreamListener { public: virtual int OnStartRequest(nsIRequest* r) = 0; };\n");
    write(d, "net/Channel.h", "#include \"Listener.h\"\nclass Channel : public nsIStreamListener {\n public:\n  int OnStartRequest(nsIRequest* r) override;\n};\n");
    write(d, "net/Channel.cpp", "#include \"net/Channel.h\"\nint Channel::OnStartRequest(nsIRequest* r) { return Helper(r); }\n");
    write(d, "dom/Loader.cpp", "#include \"mozilla/Channel.h\"\nclass Loader : public Channel {\n  int OnStartRequest(nsIRequest* r) override { return 0; }\n  int OnStartRequest(int ipc);\n};\n");
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

    let stats = Indexer::new(repo.clone()).update(&mut store, "HEAD").unwrap();
    assert_eq!((stats.tree_files, stats.indexable, stats.extracted), (6, 5, 5));

    let engine = Engine::new(&store, &repo);
    let defs = engine.def("Channel", &[SymbolKind::Class], None).unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!((defs[0].path.as_str(), defs[0].line), ("net/Channel.h", 2));

    // Two files are named Channel.h; the includes are classified accordingly.
    let inc = engine.includers("net/Channel.h", None).unwrap();
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
        .overrides("OnStartRequest", Some("nsIStreamListener"), Some("(nsIRequest *)"), None)
        .unwrap();
    let classes: Vec<_> = over.iter().map(|h| h.detail.as_str()).collect();
    assert_eq!(classes, ["Channel(nsIRequest*)", "Loader(nsIRequest*)"]);
    assert_eq!(over[0].note.as_deref(), Some("declared+defined"));

    let mut calls = engine.refs("Helper", Some(oi_model::RefKind::Call), None).unwrap();
    engine.attach_snippets(&mut calls).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].detail, "call in Channel::OnStartRequest");
    assert!(calls[0].snippet.as_deref().unwrap().contains("Helper(r)"));
    drop(engine);

    // Incremental: change one file, delete one; only those are touched.
    let d = tmp.path();
    write(d, "net/Channel.cpp", "#include \"net/Channel.h\"\n\nint Channel::OnStartRequest(nsIRequest* r) { return 1; }\n");
    std::fs::remove_file(d.join("other/Channel.h")).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "two"]);
    let stats = Indexer::new(repo.clone()).update(&mut store, "HEAD").unwrap();
    assert_eq!((stats.extracted, stats.unchanged, stats.removed), (1, 3, 2));

    let engine = Engine::new(&store, &repo);
    assert!(engine.refs("Helper", None, None).unwrap().is_empty());
    let def = engine.def("Channel::OnStartRequest", &[], Some("net/Channel.cpp")).unwrap();
    assert_eq!(def[0].line, 3);
    assert!(!engine.freshness().unwrap().is_stale());
    // With the other Channel.h gone, the include from dom/ is still only a basename match.
    let inc = engine.includers("net/Channel.h", Some("dom/")).unwrap();
    assert_eq!(inc[0].note.as_deref(), Some("basename"));
}
