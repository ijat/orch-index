//! `orch-index`: build and query a code index for a git repository.
//!
//! Output is designed for agents: a count line first, then one `path:line` row per hit with its
//! provenance, capped by `--limit`, with a per-directory breakdown of whatever was cut.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use oi_git::Repo;
use oi_indexer::Indexer;
use oi_model::{RefKind, SymbolKind};
use oi_query::{Engine, Freshness, Hit};
use oi_store::Store;

#[derive(Parser)]
#[command(
    name = "orch-index",
    version,
    about = "Code index for large git repositories"
)]
struct Cli {
    /// Repository (any directory inside it, worktrees included).
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    /// Index database. Default: `<git-common-dir>/orch-index/index.db`.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Output {
    /// Only hits under this path prefix, e.g. `netwerk/`.
    #[arg(long)]
    path: Option<String>,
    /// Maximum rows to print; the rest are summarized by directory. 0 = no limit.
    #[arg(long, default_value_t = 30)]
    limit: usize,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Omit the source line after each hit.
    #[arg(long)]
    no_snippet: bool,
    /// One row per file, `path (hits) [label]`: for "which files ..." questions.
    #[arg(long)]
    files: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Build or incrementally update the index to a revision.
    Index {
        #[arg(long, default_value = "HEAD")]
        rev: String,
    },
    /// Row counts and the indexed commit.
    Stats,
    /// Debug: print the facts extracted from one file on disk, as JSON lines.
    #[command(hide = true)]
    Extract {
        file: PathBuf,
        /// Print syntax-error sites instead of symbols.
        #[arg(long)]
        errors: bool,
    },
    /// Where a symbol is defined or declared. NAME may be qualified: `Class::Method`.
    Def {
        name: String,
        /// Restrict to kinds, comma-separated (class,struct,function,method,macro,...).
        #[arg(long, value_delimiter = ',')]
        kind: Vec<String>,
        #[command(flatten)]
        out: Output,
    },
    /// Every use of a name (by name; not type-resolved).
    Refs {
        name: String,
        /// call, type or use.
        #[arg(long)]
        kind: Option<String>,
        #[command(flatten)]
        out: Output,
    },
    /// Call sites of a function or method, with the calling function.
    Callers {
        name: String,
        #[command(flatten)]
        out: Output,
    },
    /// Files that #include the given file (repository path).
    Includers {
        file: String,
        #[command(flatten)]
        out: Output,
    },
    /// Classes declaring or defining a method, one row per class.
    Overrides {
        method: String,
        /// Only classes deriving (transitively) from this class.
        #[arg(long)]
        base: Option<String>,
        /// Only this parameter list, e.g. "(nsIRequest*)".
        #[arg(long)]
        sig: Option<String>,
        #[command(flatten)]
        out: Output,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orch-index: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repo::new(&cli.repo);
    let db = if let Some(p) = cli.db {
        p
    } else {
        let dir = repo.common_dir()?.join("orch-index");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        dir.join("index.db")
    };
    if let Command::Extract { file, errors } = &cli.command {
        let path = file.to_string_lossy();
        let registry = oi_lang::Registry::builtin();
        let extractor = registry
            .for_path(&path)
            .context("no extractor for this file")?;
        let source = std::fs::read(file)?;
        let mut stdout = io::stdout().lock();
        if *errors {
            for e in extractor.syntax_errors(&source)? {
                writeln!(stdout, "{}", serde_json::to_string(&e)?)?;
            }
        } else {
            for s in &extractor.extract(&source)?.symbols {
                writeln!(stdout, "{}", serde_json::to_string(s)?)?;
            }
        }
        return Ok(());
    }
    let mut store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;

    match cli.command {
        Command::Index { rev } => {
            let stats = Indexer::new(repo).update(&mut store, &rev)?;
            println!(
                "indexed {} ({}): {} extracted, {} unchanged, {} removed, {} skipped (large), {} failed; {} indexable of {} files; {:.1} MB read",
                short(&stats.commit),
                rev,
                stats.extracted,
                stats.unchanged,
                stats.removed,
                stats.skipped_large,
                stats.failed,
                stats.indexable,
                stats.tree_files,
                mb(stats.bytes),
            );
            println!(
                "time: {:.2}s total ({:.2}s listing, {:.2}s extract+write)",
                stats.total_time.as_secs_f64(),
                stats.list_time.as_secs_f64(),
                stats.extract_time.as_secs_f64()
            );
        }
        Command::Extract { .. } => unreachable!("handled before opening the store"),
        Command::Stats => {
            println!(
                "commit {}",
                store.meta("commit")?.as_deref().unwrap_or("(none)")
            );
            for (table, n) in store.counts()? {
                println!("{table:9} {n}");
            }
            println!(
                "errors    {} files parsed with syntax errors",
                store.files_with_errors()?
            );
            if let Ok(meta) = std::fs::metadata(&db) {
                println!("db       {:.1} MB  {}", mb(meta.len()), db.display());
            }
        }
        query => run_query(&store, &repo, query)?,
    }
    Ok(())
}

fn run_query(store: &Store, repo: &Repo, command: Command) -> Result<()> {
    let engine = Engine::new(store, repo);
    match command {
        Command::Def { name, kind, out } => {
            let kinds = kind
                .iter()
                .map(|k| SymbolKind::parse(k).with_context(|| format!("unknown kind `{k}`")))
                .collect::<Result<Vec<_>>>()?;
            let hits = engine.def(&name, &kinds, out.path.as_deref())?;
            print_hits(&engine, &format!("definitions of {name}"), hits, &out, true)?;
        }
        Command::Refs { name, kind, out } => {
            let kind = kind
                .map(|k| RefKind::parse(&k).with_context(|| format!("unknown ref kind `{k}`")))
                .transpose()?;
            let hits = engine.refs(&name, kind, out.path.as_deref())?;
            print_hits(&engine, &format!("references to {name}"), hits, &out, true)?;
        }
        Command::Callers { name, out } => {
            let hits = engine.refs(&name, Some(RefKind::Call), out.path.as_deref())?;
            print_hits(&engine, &format!("call sites of {name}"), hits, &out, true)?;
        }
        Command::Includers { file, out } => {
            let hits = engine.includers(&file, out.path.as_deref())?;
            print_hits(&engine, &format!("includers of {file}"), hits, &out, false)?;
        }
        Command::Overrides {
            method,
            base,
            sig,
            out,
        } => {
            let hits = engine.overrides(
                &method,
                base.as_deref(),
                sig.as_deref(),
                out.path.as_deref(),
            )?;
            print_hits(
                &engine,
                &format!("classes with method {method}"),
                hits,
                &out,
                true,
            )?;
        }
        Command::Index { .. } | Command::Stats | Command::Extract { .. } => {
            unreachable!("not a query")
        }
    }
    Ok(())
}

fn print_hits(
    engine: &Engine<'_>,
    what: &str,
    mut hits: Vec<Hit>,
    out: &Output,
    snippets: bool,
) -> Result<()> {
    if out.files {
        return print_files(engine, what, &hits, out);
    }
    let total = hits.len();
    let shown = if out.limit == 0 {
        total
    } else {
        total.min(out.limit)
    };
    let rest: Vec<Hit> = hits.split_off(shown);
    if snippets && !out.no_snippet {
        engine.attach_snippets(&mut hits)?;
    }
    let fresh = engine.freshness()?;
    let mut stdout = io::stdout().lock();

    if out.json {
        let doc = serde_json::json!({
            "query": what,
            "total": total,
            "shown": shown,
            "index": fresh,
            "stale": fresh.is_stale(),
            "hits": hits,
            "omitted_by_dir": by_dir(&rest),
        });
        writeln!(stdout, "{}", serde_json::to_string(&doc)?)?;
        return Ok(());
    }

    writeln!(stdout, "{total} {what}{}", freshness_note(&fresh))?;
    for h in &hits {
        let label = match &h.note {
            Some(n) => format!("{}:{n}", h.provenance),
            None => h.provenance.to_string(),
        };
        match &h.snippet {
            Some(s) => writeln!(stdout, "{}:{} [{label}] {} | {s}", h.path, h.line, h.detail)?,
            None => writeln!(stdout, "{}:{} [{label}] {}", h.path, h.line, h.detail)?,
        }
    }
    if !rest.is_empty() {
        let dirs: Vec<String> = by_dir(&rest)
            .into_iter()
            .map(|(d, n)| format!("{d} {n}"))
            .collect();
        writeln!(
            stdout,
            "… {} more (narrow with --path or raise --limit): {}",
            rest.len(),
            dirs.join(", ")
        )?;
    }
    Ok(())
}

/// Hits grouped per file (hits arrive sorted by path), with the file's label when all agree.
fn print_files(engine: &Engine<'_>, what: &str, hits: &[Hit], out: &Output) -> Result<()> {
    let mut files: Vec<(&str, usize, BTreeSet<String>)> = Vec::new();
    for h in hits {
        let label = match &h.note {
            Some(n) => format!("{}:{n}", h.provenance),
            None => h.provenance.to_string(),
        };
        match files.last_mut() {
            Some((path, n, labels)) if *path == h.path => {
                *n += 1;
                labels.insert(label);
            }
            _ => files.push((&h.path, 1, BTreeSet::from([label]))),
        }
    }
    let total_files = files.len();
    let shown = if out.limit == 0 {
        total_files
    } else {
        total_files.min(out.limit)
    };
    let fresh = engine.freshness()?;
    let mut stdout = io::stdout().lock();
    if out.json {
        let rows: Vec<_> = files[..shown]
            .iter()
            .map(|(p, n, l)| serde_json::json!({"path": p, "hits": n, "labels": l}))
            .collect();
        let doc = serde_json::json!({
            "query": what, "total_hits": hits.len(), "total_files": total_files, "shown": shown,
            "index": fresh, "stale": fresh.is_stale(), "files": rows,
        });
        writeln!(stdout, "{}", serde_json::to_string(&doc)?)?;
        return Ok(());
    }
    writeln!(
        stdout,
        "{} {what} in {total_files} files{}",
        hits.len(),
        freshness_note(&fresh)
    )?;
    for (path, n, labels) in &files[..shown] {
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        writeln!(stdout, "{path} ({n}) [{}]", labels.join(","))?;
    }
    if shown < total_files {
        writeln!(
            stdout,
            "… {} more files (narrow with --path or raise --limit)",
            total_files - shown
        )?;
    }
    Ok(())
}

/// Counts per two-level directory, largest first.
fn by_dir(hits: &[Hit]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for h in hits {
        let dir: Vec<&str> = h.path.split('/').take(2).collect();
        let key = if dir.len() == 2 && h.path.matches('/').count() >= 2 {
            format!("{}/", dir.join("/"))
        } else {
            h.path
                .rsplit_once('/')
                .map_or_else(|| "./".to_owned(), |(d, _)| format!("{d}/"))
        };
        *counts.entry(key).or_default() += 1;
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn freshness_note(f: &Freshness) -> String {
    match (&f.indexed, &f.head) {
        (None, _) => " (no index: run `orch-index index`)".to_owned(),
        (Some(i), Some(h)) if i != h => format!(
            " (index at {}, HEAD is {}: results may be stale; run `orch-index index`)",
            short(i),
            short(h)
        ),
        _ => String::new(),
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

#[allow(clippy::cast_precision_loss)]
fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}
