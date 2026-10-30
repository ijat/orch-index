//! Agent-facing questions answered from the store.
//!
//! Every answer is a list of [`Hit`]s with a provenance and, where useful, a note explaining why
//! the confidence is what it is. Rendering is the caller's job.

use std::collections::{BTreeMap, HashSet, VecDeque};

use oi_git::{GitError, Repo};
use oi_model::{Provenance, RefKind, SymbolKind};
use oi_store::{Store, StoreError, SymbolHit};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] GitError),
}

pub type Result<T> = std::result::Result<T, QueryError>;

/// One answer row.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub line: u32,
    pub provenance: Provenance,
    /// Short qualifier shown before the snippet, e.g. `method mozilla::net::nsHttpChannel`.
    pub detail: String,
    /// Why the provenance is what it is, when it isn't self-evident (e.g. `ambiguous:3`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The source line, filled in by [`Engine::attach_snippets`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip)]
    oid: String,
}

/// Index freshness relative to the repository.
#[derive(Debug, Clone, Serialize)]
pub struct Freshness {
    pub indexed: Option<String>,
    pub head: Option<String>,
}

impl Freshness {
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.indexed != self.head
    }
}

pub struct Engine<'a> {
    store: &'a Store,
    repo: &'a Repo,
}

impl<'a> Engine<'a> {
    #[must_use]
    pub fn new(store: &'a Store, repo: &'a Repo) -> Self {
        Self { store, repo }
    }

    pub fn freshness(&self) -> Result<Freshness> {
        Ok(Freshness {
            indexed: self.store.meta("commit")?,
            head: self.repo.rev_parse("HEAD").ok(),
        })
    }

    /// Definitions and declarations of `name`, which may be qualified (`Class::Method`).
    pub fn def(&self, name: &str, kinds: &[SymbolKind], path: Option<&str>) -> Result<Vec<Hit>> {
        let (scope_suffix, base) = split_qualified(name);
        let hits = self.store.symbols(base, kinds, path)?;
        Ok(hits
            .into_iter()
            .filter(|s| scope_suffix.is_none_or(|q| scope_matches(&s.scope, q)))
            .map(|s| symbol_hit(&s))
            .collect())
    }

    /// Uses of `name`. References are by name only, so every hit is syntactic.
    pub fn refs(&self, name: &str, kind: Option<RefKind>, path: Option<&str>) -> Result<Vec<Hit>> {
        let (_, base) = split_qualified(name);
        Ok(self
            .store
            .refs(base, kind, path)?
            .into_iter()
            .map(|r| Hit {
                path: r.path,
                line: r.line,
                provenance: Provenance::Syntactic,
                detail: match &r.container {
                    Some(c) => format!("{} in {c}", r.kind.as_str()),
                    None => r.kind.as_str().to_owned(),
                },
                note: None,
                snippet: None,
                oid: r.oid,
            })
            .collect())
    }

    /// Files that `#include` the file at `target` (a repository path).
    ///
    /// `resolved`: the include spec matches `target` and no other indexed file.
    /// `syntactic` + `ambiguous:N`: the spec matches `target` and N-1 other files.
    /// `syntactic` + `basename`: the spec matches no indexed path (typically a build-system export
    /// path such as `mozilla/net/X.h`); only the file name agrees.
    pub fn includers(&self, target: &str, path: Option<&str>) -> Result<Vec<Hit>> {
        let basename = target.rsplit('/').next().unwrap_or(target);
        let same_name = self.store.files_by_basename(basename)?;
        let mut out = Vec::new();
        for inc in self.store.includes_by_basename(basename)? {
            if path.is_some_and(|p| !inc.path.starts_with(p)) {
                continue;
            }
            let relative = relative_target(&inc.path, &inc.spec);
            let (provenance, note) = if relative.as_deref() == Some(target) {
                (Provenance::Resolved, None)
            } else {
                let candidates: Vec<&String> = same_name
                    .iter()
                    .filter(|p| path_ends_with(p, &inc.spec))
                    .collect();
                if candidates.iter().any(|p| *p == target) {
                    if candidates.len() == 1 {
                        (Provenance::Resolved, None)
                    } else {
                        (
                            Provenance::Syntactic,
                            Some(format!("ambiguous:{}", candidates.len())),
                        )
                    }
                } else if candidates.is_empty()
                    && relative.as_ref().is_none_or(|r| !same_name.contains(r))
                {
                    (Provenance::Syntactic, Some("basename".to_owned()))
                } else {
                    continue; // the spec names a different file
                }
            };
            out.push(Hit {
                path: inc.path,
                line: inc.line,
                provenance,
                detail: format!("#include {}", inc.spec),
                note,
                snippet: None,
                oid: inc.oid,
            });
        }
        Ok(out)
    }

    /// Classes that declare or define method `method`, one hit per class.
    ///
    /// With `base`, only classes deriving from it (transitively, by name) are kept. With
    /// `signature`, only methods whose normalized parameter list matches.
    pub fn overrides(
        &self,
        method: &str,
        base: Option<&str>,
        signature: Option<&str>,
        path: Option<&str>,
    ) -> Result<Vec<Hit>> {
        let derived = base.map(|b| self.derived_closure(b)).transpose()?;
        let wanted_sig = signature.map(normalize_signature);
        let mut by_class: BTreeMap<String, Vec<SymbolHit>> = BTreeMap::new();
        for s in self.store.symbols(method, &[SymbolKind::Method], path)? {
            if wanted_sig
                .as_ref()
                .is_some_and(|w| normalize_signature(&s.signature) != *w)
            {
                continue;
            }
            let class = s.scope.rsplit("::").next().unwrap_or("").to_owned();
            if class.is_empty() || derived.as_ref().is_some_and(|d| !d.contains(&class)) {
                continue;
            }
            by_class.entry(s.scope.clone()).or_default().push(s);
        }
        Ok(by_class
            .into_values()
            .filter_map(|mut rows| {
                // Prefer the in-class declaration; fall back to the out-of-line definition.
                rows.sort_by_key(|s| (s.is_definition, s.path.clone(), s.line));
                let first = rows.first()?;
                let mut hit = symbol_hit(first);
                hit.detail = format!("{}{}", first.scope, first.signature);
                let defs = rows.iter().filter(|s| s.is_definition).count();
                hit.note = Some(match (first.is_definition, defs) {
                    (true, _) => "definition only".to_owned(),
                    (false, 0) => "declared".to_owned(),
                    (false, _) => "declared+defined".to_owned(),
                });
                Some(hit)
            })
            .collect())
    }

    /// Names of every class deriving from `base`, directly or indirectly.
    fn derived_closure(&self, base: &str) -> Result<HashSet<String>> {
        let (_, base) = split_qualified(base);
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([base.to_owned()]);
        while let Some(b) = queue.pop_front() {
            for edge in self.store.derived_of(&b)? {
                if seen.insert(edge.derived.clone()) {
                    queue.push_back(edge.derived);
                }
            }
        }
        Ok(seen)
    }

    /// Fills `snippet` from the indexed blobs, so output is correct even in a sparse worktree.
    pub fn attach_snippets(&self, hits: &mut [Hit]) -> Result<()> {
        let mut reader = self.repo.blob_reader()?;
        let mut oids: Vec<String> = hits.iter().map(|h| h.oid.clone()).collect();
        oids.sort();
        oids.dedup();
        for oid in oids {
            let Some(blob) = reader.read(&oid)? else {
                continue;
            };
            let text = String::from_utf8_lossy(&blob);
            let lines: Vec<&str> = text.lines().collect();
            for hit in hits.iter_mut().filter(|h| h.oid == oid) {
                let idx = usize::try_from(hit.line).unwrap_or(0).saturating_sub(1);
                if let Some(line) = lines.get(idx) {
                    hit.snippet = Some(clip(line.trim(), 160));
                }
            }
        }
        Ok(())
    }
}

fn symbol_hit(s: &SymbolHit) -> Hit {
    let what = if s.is_definition || !matches!(s.kind, SymbolKind::Function | SymbolKind::Method) {
        s.kind.as_str().to_owned()
    } else {
        format!("{} decl", s.kind.as_str())
    };
    Hit {
        path: s.path.clone(),
        line: s.line,
        provenance: s.provenance,
        detail: if s.scope.is_empty() {
            what
        } else {
            format!("{what} {}", s.scope)
        },
        note: None,
        snippet: None,
        oid: s.oid.clone(),
    }
}

/// `a::b::name` → (`Some("a::b")`, `"name"`).
fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once("::") {
        Some((scope, base)) if !scope.is_empty() => (Some(scope), base),
        Some((_, base)) => (None, base),
        None => (None, name),
    }
}

/// Whether `scope` ends with the `::`-separated `suffix`.
fn scope_matches(scope: &str, suffix: &str) -> bool {
    scope == suffix || scope.ends_with(&format!("::{suffix}"))
}

fn path_ends_with(path: &str, spec: &str) -> bool {
    path == spec || path.ends_with(&format!("/{spec}"))
}

/// The repository path `spec` names when resolved against the including file's directory.
fn relative_target(includer: &str, spec: &str) -> Option<String> {
    let dir = includer.rsplit_once('/').map_or("", |(d, _)| d);
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in spec.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn normalize_signature(sig: &str) -> String {
    sig.chars().filter(|c| !c.is_whitespace()).collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative_target("a/b/c.cpp", "d.h").as_deref(),
            Some("a/b/d.h")
        );
        assert_eq!(
            relative_target("a/b/c.cpp", "../d.h").as_deref(),
            Some("a/d.h")
        );
        assert_eq!(relative_target("c.cpp", "../d.h"), None);
    }

    #[test]
    fn qualified_names() {
        assert_eq!(split_qualified("A::B::f"), (Some("A::B"), "f"));
        assert_eq!(split_qualified("f"), (None, "f"));
        assert!(scope_matches(
            "mozilla::net::nsHttpChannel",
            "nsHttpChannel"
        ));
        assert!(!scope_matches(
            "mozilla::net::XnsHttpChannel",
            "nsHttpChannel"
        ));
    }
}
