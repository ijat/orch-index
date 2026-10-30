//! Reads a repository from the git object store, so indexing works in sparse worktrees and
//! never depends on what happens to be checked out.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Io(#[from] std::io::Error),
    #[error("git {args} failed: {stderr}")]
    Command { args: String, stderr: String },
    #[error("unexpected output from git cat-file: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// One file (blob) in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    /// Hex object id of the blob; changes exactly when the content changes.
    pub oid: String,
}

/// A git repository addressed by its working directory or `.git` directory.
#[derive(Debug, Clone)]
pub struct Repo {
    dir: PathBuf,
}

impl Repo {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir).stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = self.git().args(args).output()?;
        if !out.status.success() {
            return Err(GitError::Command {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        Ok(out.stdout)
    }

    /// Resolves a revision such as `HEAD` to a full commit id.
    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.run(&["rev-parse", "--verify", &spec])?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    /// The repository's common git directory (shared by all its worktrees), absolute.
    pub fn common_dir(&self) -> Result<PathBuf> {
        let out = self.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
    }

    /// Lists every blob reachable from `rev`, recursively. Symlinks and submodules are skipped.
    pub fn ls_tree(&self, rev: &str) -> Result<Vec<TreeEntry>> {
        let out = self.run(&["ls-tree", "-r", "-z", "--full-tree", rev])?;
        let mut entries = Vec::new();
        for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            // "<mode> SP <type> SP <oid> TAB <path>"
            let rec = String::from_utf8_lossy(rec);
            let Some((meta, path)) = rec.split_once('\t') else {
                continue;
            };
            let mut parts = meta.split(' ');
            let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if kind == "blob" && mode != "120000" {
                entries.push(TreeEntry {
                    path: path.to_owned(),
                    oid: oid.to_owned(),
                });
            }
        }
        Ok(entries)
    }

    /// Paths changed between two revisions (both must exist locally).
    pub fn diff_names(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let out = self.run(&["diff", "--name-only", "-z", "--no-renames", from, to])?;
        Ok(out
            .split(|&b| b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect())
    }

    /// Opens a `git cat-file --batch` reader for fetching blob contents by id.
    pub fn blob_reader(&self) -> Result<BlobReader> {
        BlobReader::spawn(self.git())
    }
}

/// Streams blob contents from one long-lived `git cat-file --batch` process.
pub struct BlobReader {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl BlobReader {
    fn spawn(mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let stdout = BufReader::with_capacity(1 << 20, child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Reads one blob. Returns `None` if the object is missing.
    pub fn read(&mut self, oid: &str) -> Result<Option<Vec<u8>>> {
        writeln!(self.stdin, "{oid}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let header = header.trim_end();
        if header.ends_with(" missing") {
            return Ok(None);
        }
        // "<oid> SP <type> SP <size>"
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| GitError::Protocol(header.to_owned()))?;
        let mut buf = vec![0; size + 1]; // content plus trailing LF
        self.stdout.read_exact(&mut buf)?;
        buf.pop();
        Ok(Some(buf))
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        // Closing stdin ends the batch; reap the child so it does not linger.
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
