//! The indexing pipeline: git tree → changed blobs → parallel extraction → store.
//!
//! Incremental by construction: a file is re-extracted only when its blob id differs from the one
//! stored, so the first run and every later update take the same path.

use std::collections::HashSet;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, Instant};

use oi_git::{Blob, GitError, Repo, TreeEntry};
use oi_lang::Registry;
use oi_model::{FileFacts, Lang};
use oi_store::{Store, StoreError};
use rayon::iter::{ParallelBridge, ParallelIterator};

pub use oi_lang::EXTRACTOR_VERSION;

/// Blobs larger than this are skipped; at this size C/C++ is almost always generated.
pub const DEFAULT_MAX_FILE_BYTES: usize = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// What one update did, and where the time went.
#[derive(Debug, Clone, Default)]
pub struct UpdateStats {
    pub commit: String,
    pub tree_files: usize,
    pub indexable: usize,
    pub unchanged: usize,
    pub extracted: usize,
    pub removed: usize,
    pub skipped_large: usize,
    /// Paths that are not valid UTF-8; never indexed.
    pub skipped_non_utf8: usize,
    /// Extraction failed or panicked; the file is recorded without facts.
    pub failed: usize,
    pub bytes: u64,
    pub list_time: Duration,
    pub extract_time: Duration,
    pub total_time: Duration,
}

pub struct Indexer {
    repo: Repo,
    registry: Registry,
    max_file_bytes: usize,
}

struct Extracted {
    entry: TreeEntry,
    lang: Lang,
    outcome: Outcome,
}

enum Outcome {
    Facts(FileFacts, usize),
    TooLarge,
    Failed(usize),
}

impl Indexer {
    #[must_use]
    pub fn new(repo: Repo) -> Self {
        Self {
            repo,
            registry: Registry::builtin(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }

    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: usize) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Brings `store` up to date with `rev` (usually `HEAD`).
    pub fn update(&self, store: &mut Store, rev: &str) -> Result<UpdateStats> {
        let start = Instant::now();
        let mut stats = UpdateStats {
            commit: self.repo.rev_parse(rev)?,
            ..UpdateStats::default()
        };
        let tree = self.repo.ls_tree(&stats.commit)?;
        stats.skipped_non_utf8 = tree.skipped_non_utf8;
        let tree = tree.entries;
        stats.tree_files = tree.len();
        // Everything below happens in one write transaction, so a concurrent update can't
        // change the indexed set between reading it and writing.
        let mut writer = store.writer()?;
        // An index built by a different extractor is stale for every file, whatever its blob id.
        let same_extractor = writer.meta("extractor")?.as_deref() == Some(EXTRACTOR_VERSION);
        let indexed = writer.files()?;

        let mut todo: Vec<(TreeEntry, Lang)> = Vec::new();
        let mut live: HashSet<&str> = HashSet::new();
        for entry in &tree {
            let Some(extractor) = self.registry.for_path(&entry.path) else {
                continue;
            };
            stats.indexable += 1;
            live.insert(entry.path.as_str());
            match indexed.get(&entry.path) {
                Some(row) if same_extractor && row.oid == entry.oid => stats.unchanged += 1,
                _ => todo.push((entry.clone(), extractor.lang())),
            }
        }
        // Stale rows: deleted from the tree, or about to be replaced with new content.
        let changed: HashSet<&str> = todo.iter().map(|(e, _)| e.path.as_str()).collect();
        let stale: Vec<i64> = indexed
            .values()
            .filter(|row| !live.contains(row.path.as_str()) || changed.contains(row.path.as_str()))
            .map(|row| row.id)
            .collect();
        stats.removed = stale.len();
        stats.list_time = start.elapsed();

        let extract_start = Instant::now();
        for id in stale {
            writer.remove_file(id)?;
        }
        drop(changed);
        drop(live);
        self.extract_into(todo, &mut writer, &mut stats)?;
        writer.set_meta("commit", &stats.commit)?;
        writer.set_meta("extractor", EXTRACTOR_VERSION)?;
        writer.commit()?;
        stats.extract_time = extract_start.elapsed();
        stats.total_time = start.elapsed();
        Ok(stats)
    }

    /// Reads blobs on one thread, extracts on the rayon pool, writes on the caller's thread.
    /// Oversized blobs are discarded unbuffered; a panicking extractor fails only its own file.
    fn extract_into(
        &self,
        todo: Vec<(TreeEntry, Lang)>,
        writer: &mut oi_store::Writer<'_>,
        stats: &mut UpdateStats,
    ) -> Result<()> {
        let mut reader = self.repo.blob_reader()?;
        let (blob_tx, blob_rx) = sync_channel::<(TreeEntry, Lang, Option<Vec<u8>>)>(512);
        let (fact_tx, fact_rx) = sync_channel::<Extracted>(512);
        let max = u64::try_from(self.max_file_bytes).unwrap_or(u64::MAX);
        let registry = &self.registry;

        thread::scope(|s| {
            let read = s.spawn(move || -> Result<()> {
                for (entry, lang) in todo {
                    let bytes = match reader.read(&entry.oid, max)? {
                        Blob::Data(bytes) => Some(bytes),
                        Blob::TooLarge(_) => None,
                        Blob::Missing => continue,
                    };
                    if blob_tx.send((entry, lang, bytes)).is_err() {
                        break;
                    }
                }
                Ok(())
            });
            s.spawn(move || {
                blob_rx.into_iter().par_bridge().for_each_with(
                    fact_tx,
                    |tx, (entry, lang, bytes)| {
                        let outcome = match bytes {
                            None => Outcome::TooLarge,
                            Some(bytes) => extract_one(registry, &entry.path, &bytes),
                        };
                        let _ = tx.send(Extracted {
                            entry,
                            lang,
                            outcome,
                        });
                    },
                );
            });

            let mut result = Ok(());
            for item in fact_rx {
                if result.is_err() {
                    continue; // drain so the producers can finish
                }
                let facts = match item.outcome {
                    Outcome::Facts(f, len) => {
                        stats.extracted += 1;
                        stats.bytes += len as u64;
                        f
                    }
                    Outcome::TooLarge => {
                        stats.skipped_large += 1;
                        FileFacts::default()
                    }
                    Outcome::Failed(len) => {
                        stats.failed += 1;
                        stats.bytes += len as u64;
                        FileFacts::default()
                    }
                };
                // Skipped and failed files are still recorded, so the next update doesn't retry them.
                result = writer
                    .put_file(&item.entry.path, &item.entry.oid, item.lang, &facts)
                    .map_err(IndexError::from);
            }
            read.join().expect("blob reader thread panicked")?;
            result
        })
    }
}

/// Extracts one file. A panic inside a parser or extractor is contained here: it fails this
/// file only, instead of tearing down the whole index run.
fn extract_one(registry: &Registry, path: &str, bytes: &[u8]) -> Outcome {
    let run = || {
        registry
            .for_path(path)
            .and_then(|x| x.extract_path(path, bytes).ok())
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(Some(facts)) => Outcome::Facts(facts, bytes.len()),
        Ok(None) | Err(_) => Outcome::Failed(bytes.len()),
    }
}
