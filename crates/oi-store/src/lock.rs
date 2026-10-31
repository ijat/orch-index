//! The advisory `<db>.lock` file an `index` run holds, so readers can tell an update is running.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::Result;

/// Held for the whole of an index update; the lock is released when this is dropped.
#[derive(Debug)]
pub struct IndexLock {
    _file: File,
}

/// `<db>.lock`: the database path with `.lock` appended.
#[must_use]
pub fn lock_path(db: &Path) -> PathBuf {
    let mut p = OsString::from(db.as_os_str());
    p.push(".lock");
    PathBuf::from(p)
}

/// Creates or opens `<db>.lock` and blocks until its exclusive lock is ours.
pub(crate) fn acquire(db: &Path) -> Result<IndexLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(db))?;
    file.lock()?;
    Ok(IndexLock { _file: file })
}

/// Whether an index update currently holds `<db>.lock`. Never creates the file: a missing or
/// unopenable lock file means not indexing.
#[must_use]
pub fn is_indexing(db: &Path) -> bool {
    let Ok(file) = File::open(lock_path(db)) else {
        return false;
    };
    // A successful shared lock is released when `file` drops at the end of this call.
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}
