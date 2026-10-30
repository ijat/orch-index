//! The indexing pipeline: git tree → changed blobs → parallel extraction → store.
//!
//! Incremental by construction: a file is re-extracted only when its blob id differs from the one
//! stored, so the first run and every later update take the same path.

use std::collections::HashSet;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, Instant};

use oi_git::{GitError, Repo, TreeEntry};
use oi_lang::Registry;
use oi_model::{FileFacts, Lang};
use oi_store::{Store, StoreError};
use rayon::iter::{ParallelBridge, ParallelIterator};

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
    facts: Option<FileFacts>,
    bytes: usize,
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
        stats.tree_files = tree.len();
        let indexed = store.files()?;

        let mut todo: Vec<(TreeEntry, Lang)> = Vec::new();
        let mut live: HashSet<&str> = HashSet::new();
        for entry in &tree {
            let Some(extractor) = self.registry.for_path(&entry.path) else {
                continue;
            };
            stats.indexable += 1;
            live.insert(entry.path.as_str());
            match indexed.get(&entry.path) {
                Some(row) if row.oid == entry.oid => stats.unchanged += 1,
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
        let mut writer = store.writer()?;
        for id in stale {
            writer.remove_file(id)?;
        }
        drop(changed);
        drop(live);
        self.extract_into(todo, &mut writer, &mut stats)?;
        writer.set_meta("commit", &stats.commit)?;
        writer.commit()?;
        stats.extract_time = extract_start.elapsed();
        stats.total_time = start.elapsed();
        Ok(stats)
    }

    /// Reads blobs on one thread, extracts on the rayon pool, writes on the caller's thread.
    fn extract_into(
        &self,
        todo: Vec<(TreeEntry, Lang)>,
        writer: &mut oi_store::Writer<'_>,
        stats: &mut UpdateStats,
    ) -> Result<()> {
        let mut reader = self.repo.blob_reader()?;
        let (blob_tx, blob_rx) = sync_channel::<(TreeEntry, Lang, Vec<u8>)>(512);
        let (fact_tx, fact_rx) = sync_channel::<Extracted>(512);
        let max = self.max_file_bytes;
        let registry = &self.registry;

        thread::scope(|s| {
            let read = s.spawn(move || -> Result<()> {
                for (entry, lang) in todo {
                    let Some(bytes) = reader.read(&entry.oid)? else {
                        continue;
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
                        let facts = if bytes.len() > max {
                            None
                        } else {
                            registry
                                .for_path(&entry.path)
                                .and_then(|x| x.extract(&bytes).ok())
                        };
                        let _ = tx.send(Extracted {
                            entry,
                            lang,
                            facts,
                            bytes: bytes.len(),
                        });
                    },
                );
            });

            let mut result = Ok(());
            for item in fact_rx {
                if result.is_err() {
                    continue; // drain so the producers can finish
                }
                stats.bytes += item.bytes as u64;
                let facts = match item.facts {
                    Some(f) => {
                        stats.extracted += 1;
                        f
                    }
                    None if item.bytes > max => {
                        stats.skipped_large += 1;
                        FileFacts::default()
                    }
                    None => {
                        stats.failed += 1;
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
