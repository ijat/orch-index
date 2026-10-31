//! Agent-facing questions answered from the store.
//!
//! Every answer is a list of [`Hit`]s with a provenance and, where useful, a note explaining why
//! the confidence is what it is. Rendering is the caller's job.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt::Write as _;

use oi_git::{Blob, GitError, Repo};

/// Blobs larger than this are not read for snippets (indexed files are far smaller).
const SNIPPET_MAX_BLOB: u64 = 64 << 20;
use oi_model::{Lang, Provenance, RefKind, SymbolKind, key_of};
use oi_store::{Filter, LabelClass, Store, StoreError, SymbolHit};
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

impl Hit {
    /// The class this hit counts under in the usage label mix: a parse-error note wins, then an
    /// ambiguity note (`ambiguous:N`, `basename`), else the provenance. The only place that
    /// knows the note vocabulary.
    #[must_use]
    pub fn label_class(&self) -> LabelClass {
        match self.note.as_deref() {
            Some(n) if n.contains("parse-errors") => LabelClass::ParseErrors,
            Some(n) if n.starts_with("ambiguous") || n == "basename" => LabelClass::Ambiguous,
            _ => match self.provenance {
                Provenance::Resolved => LabelClass::Resolved,
                Provenance::Syntactic => LabelClass::Syntactic,
                Provenance::Compiler => LabelClass::Compiler,
            },
        }
    }

    /// Escapes control characters in every text field (see [`sanitize`]). Call before printing.
    pub fn sanitize(&mut self) {
        for field in [&mut self.path, &mut self.detail] {
            if let Cow::Owned(clean) = sanitize(field) {
                *field = clean;
            }
        }
        for field in [&mut self.note, &mut self.snippet].into_iter().flatten() {
            if let Cow::Owned(clean) = sanitize(field) {
                *field = clean;
            }
        }
    }
}

/// Escapes C0 controls (including newline and CR), DEL and C1 controls as `\u{..}`, so text from
/// an indexed repository cannot forge output rows or send terminal escape sequences.
#[must_use]
pub fn sanitize(s: &str) -> Cow<'_, str> {
    let is_control = |c: char| c.is_control();
    if !s.chars().any(is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if is_control(c) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Index freshness relative to the repository.
#[derive(Debug, Clone, Serialize)]
pub struct Freshness {
    pub indexed: Option<String>,
    pub head: Option<String>,
    /// The index was built by a different extractor version than this binary's.
    pub extractor_outdated: bool,
}

impl Freshness {
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.indexed != self.head || self.extractor_outdated
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
            extractor_outdated: self.store.meta("extractor")?.as_deref()
                != Some(oi_lang::EXTRACTOR_VERSION),
        })
    }

    /// Definitions and declarations of `name`, which may be qualified (`Class::Method`).
    pub fn def(&self, name: &str, kinds: &[SymbolKind], filter: Filter<'_>) -> Result<Vec<Hit>> {
        let (scope_suffix, base) = split_qualified(name);
        let hits = self.store.symbols(base, kinds, filter)?;
        Ok(hits
            .into_iter()
            .filter(|s| scope_suffix.is_none_or(|q| scope_matches(&s.scope, q)))
            .map(|s| symbol_hit(&s))
            .collect())
    }

    /// Uses of `name`. References are by name only, so every hit is syntactic.
    pub fn refs(&self, name: &str, kind: Option<RefKind>, filter: Filter<'_>) -> Result<Vec<Hit>> {
        let (_, base) = split_qualified(name);
        Ok(self
            .store
            .refs(base, kind, filter)?
            .into_iter()
            .map(|r| Hit {
                path: r.path,
                line: r.line,
                provenance: Provenance::Syntactic,
                detail: match &r.container {
                    Some(c) => format!("{} in {c}", r.kind.as_str()),
                    None => r.kind.as_str().to_owned(),
                },
                note: damaged_note(r.file_damaged),
                snippet: None,
                oid: r.oid,
            })
            .collect())
    }

    /// Files that include or import the file at `target` (a repository path).
    ///
    /// `resolved`: the spec names `target` relative to the including file, or as a path suffix
    /// that no other indexed file has.
    /// `syntactic` + `ambiguous:N`: the spec matches `target` and N-1 other files.
    /// `syntactic` + `basename`: the spec matches no indexed path (typically a build-system export
    /// path such as `mozilla/net/X.h`); only the file name agrees.
    pub fn includers(&self, target: &str, filter: Filter<'_>) -> Result<Vec<Hit>> {
        let basename = target.rsplit('/').next().unwrap_or(target);
        let same_name = self.store.files_by_basename(basename)?;
        let mut keys = vec![key_of(target, false).to_owned()];
        // `import "./widgets"` resolves to `widgets/index.ts`: look it up by directory name too.
        if keys[0] == "index"
            && let Some(dir) = target.rsplit('/').nth(1)
        {
            keys.push(dir.to_owned());
        }
        let mut out = Vec::new();
        for key in &keys {
            for inc in self.store.includes_by_key(key, false, filter)? {
                let Some((provenance, note)) =
                    classify_include(&inc.path, &inc.spec, target, &same_name)
                else {
                    continue;
                };
                out.push(Hit {
                    path: inc.path,
                    line: inc.line,
                    provenance,
                    detail: format!("#include {}", inc.spec),
                    note: join_note(note, damaged_note(inc.file_damaged)),
                    snippet: None,
                    oid: inc.oid,
                });
            }
        }
        out.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        out.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        Ok(out)
    }

    /// Files importing module or namespace `module` (`crate::store::Store`, `System.Linq`,
    /// `react`), matched by trailing path segments. Syntactic: re-exports are not followed.
    pub fn importers(&self, module: &str, filter: Filter<'_>) -> Result<Vec<Hit>> {
        let wanted = segments(module);
        let Some(key) = wanted.last() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for inc in self.store.includes_by_key(key, true, filter)? {
            if !segments(&inc.spec).ends_with(&wanted) {
                continue;
            }
            out.push(Hit {
                path: inc.path,
                line: inc.line,
                provenance: Provenance::Syntactic,
                detail: format!("import {}", inc.spec),
                note: damaged_note(inc.file_damaged),
                snippet: None,
                oid: inc.oid,
            });
        }
        Ok(out)
    }

    /// Classes that declare or define method `method`, one hit per class.
    ///
    /// With `base`, only classes deriving from it (transitively) are kept, and the base itself is
    /// left out. Inheritance is followed by qualified name: `class X : public URLRequest::Delegate`
    /// matches base `net::URLRequest::Delegate`, while an unrelated `Delegate` does not. With
    /// `signature`, only methods whose parameter list matches (namespace qualifiers ignored).
    pub fn overrides(
        &self,
        method: &str,
        base: Option<&str>,
        signature: Option<&str>,
        filter: Filter<'_>,
    ) -> Result<Vec<Hit>> {
        let bases = base.map(|b| self.resolve_class(b)).transpose()?;
        let derived = bases
            .as_ref()
            .map(|b| self.derived_closure(b))
            .transpose()?;
        let wanted_sig = signature.map(normalize_signature);
        let mut by_class: BTreeMap<(String, String), Vec<SymbolHit>> = BTreeMap::new();
        for s in self.store.symbols(method, &[SymbolKind::Method], filter)? {
            if s.scope.is_empty()
                || wanted_sig
                    .as_ref()
                    .is_some_and(|w| normalize_signature(&s.signature) != *w)
            {
                continue;
            }
            if let (Some(bases), Some(derived)) = (&bases, &derived) {
                let in_closure = derived.iter().any(|d| same_class(d, &s.scope));
                let is_base = bases.iter().any(|b| same_class(b, &s.scope));
                if !in_closure || is_base {
                    continue;
                }
            }
            // A C++ class in a named scope is declared in a header and defined in a .cc: one
            // class across files. Elsewhere (other languages, anonymous namespaces) the same
            // qualified name in two files is two classes.
            let per_file = s.lang != Some(Lang::Cpp) || s.scope.contains("(anonymous)");
            let file_key = if per_file {
                s.path.clone()
            } else {
                String::new()
            };
            by_class
                .entry((s.scope.clone(), file_key))
                .or_default()
                .push(s);
        }
        Ok(by_class
            .into_values()
            .filter_map(|mut rows| {
                // Prefer the in-class declaration; fall back to the out-of-line definition.
                rows.sort_by_key(|s| (s.is_definition, s.path.clone(), s.line));
                let first = rows.first()?;
                let mut hit = symbol_hit(first);
                hit.detail = format!("{} {}", first.scope, first.signature);
                let defs = rows.iter().filter(|s| s.is_definition).count();
                let status = match (first.is_definition, defs) {
                    (true, _) => "defined",
                    (false, 0) => "declared",
                    (false, _) => "declared+defined",
                };
                hit.note = join_note(
                    Some(status.to_owned()),
                    damaged_note(rows.iter().any(|s| s.file_damaged)),
                );
                Some(hit)
            })
            .collect())
    }

    /// Qualified names of the classes `name` may refer to (`URLRequest::Delegate` →
    /// `net::URLRequest::Delegate`). Falls back to `name` itself if no class is indexed under it.
    fn resolve_class(&self, name: &str) -> Result<Vec<String>> {
        let name = name.trim_start_matches("::");
        let (scope, base) = split_qualified(name);
        let kinds = [SymbolKind::Class, SymbolKind::Struct, SymbolKind::Union];
        let mut found: Vec<String> = self
            .store
            .symbols(base, &kinds, Filter::default())?
            .into_iter()
            .filter(|s| scope.is_none_or(|q| scope_matches(&s.scope, q)))
            .map(|s| qualify(&s.scope, base))
            .collect();
        found.sort();
        found.dedup();
        if found.is_empty() {
            found.push(name.to_owned());
        }
        Ok(found)
    }

    /// Qualified names of every class deriving from any of `bases`, directly or indirectly.
    fn derived_closure(&self, bases: &[String]) -> Result<HashSet<String>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = bases.iter().cloned().collect();
        while let Some(class) = queue.pop_front() {
            let last = class.rsplit("::").next().unwrap_or(&class);
            for edge in self.store.derived_of(last)? {
                if !base_edge_names(&class, &edge.base_written, &edge.derived_scope) {
                    continue;
                }
                let derived = qualify(&edge.derived_scope, &edge.derived);
                if seen.insert(derived.clone()) {
                    queue.push_back(derived);
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
            let Blob::Data(blob) = reader.read(&oid, SNIPPET_MAX_BLOB)? else {
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
        note: damaged_note(s.file_damaged),
        snippet: None,
        oid: s.oid.clone(),
    }
}

/// Note for hits from files with declaration-level syntax errors.
fn damaged_note(damaged: bool) -> Option<String> {
    damaged.then(|| "parse-errors".to_owned())
}

/// Joins two optional notes as `a+b`.
fn join_note(note: Option<String>, extra: Option<String>) -> Option<String> {
    match (note, extra) {
        (Some(a), Some(b)) => Some(format!("{a}+{b}")),
        (a, b) => a.or(b),
    }
}

fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}::{name}")
    }
}

/// Whether two qualified class names can denote the same class: equal, or one is the other with
/// outer scopes omitted (`net::Foo` vs `Foo`), compared at `::` boundaries.
fn same_class(a: &str, b: &str) -> bool {
    a == b || scope_matches(a, b) || scope_matches(b, a)
}

/// Whether a base written as `written`, in a class defined in scope `derived_scope`, can name the
/// class `class` (fully qualified). An unqualified or partly qualified spelling only counts where
/// name lookup could reach it: the derived class must live inside the scope it was omitted from.
fn base_edge_names(class: &str, written: &str, derived_scope: &str) -> bool {
    let written = written.trim_start_matches("::");
    if class == written || scope_matches(written, class) {
        return true;
    }
    if !scope_matches(class, written) {
        return false;
    }
    let omitted = &class[..class.len() - written.len() - 2];
    derived_scope == omitted || derived_scope.starts_with(&format!("{omitted}::"))
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

/// Extensions a bare JS/TS import path may omit.
const SCRIPT_EXTENSIONS: &[&str] = &["ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs", "mts", "cts"];

/// How a file include `spec` written in `includer` relates to `target`; `None` if it names
/// some other file.
fn classify_include(
    includer: &str,
    spec: &str,
    target: &str,
    same_name: &[String],
) -> Option<(Provenance, Option<String>)> {
    let relative = relative_target(includer, spec);
    let ts_importer = is_typescript(includer);
    if relative
        .as_deref()
        .is_some_and(|r| names_file(r, target, ts_importer))
    {
        return Some((Provenance::Resolved, None));
    }
    // An explicitly relative spec that doesn't resolve to the target names another file.
    if spec.starts_with("./") || spec.starts_with("../") {
        return None;
    }
    let candidates: Vec<&String> = same_name
        .iter()
        .filter(|p| path_ends_with(p, spec))
        .collect();
    if candidates.iter().any(|p| *p == target) {
        return Some(if candidates.len() == 1 {
            (Provenance::Resolved, None)
        } else {
            (
                Provenance::Syntactic,
                Some(format!("ambiguous:{}", candidates.len())),
            )
        });
    }
    let spec_file = spec.rsplit('/').next().unwrap_or(spec);
    let target_file = target.rsplit('/').next().unwrap_or(target);
    let same_file_name = spec_file == target_file;
    if same_file_name
        && candidates.is_empty()
        && relative.as_ref().is_none_or(|r| !same_name.contains(r))
    {
        return Some((Provenance::Syntactic, Some("basename".to_owned())));
    }
    None
}

fn is_typescript(path: &str) -> bool {
    [".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Whether the resolved path `resolved` (possibly extension-less) names `target`. With
/// `ts_importer`, an emitted-file spec also names its TypeScript source.
fn names_file(resolved: &str, target: &str, ts_importer: bool) -> bool {
    if resolved == target {
        return true;
    }
    // TypeScript ESM imports name the emitted file: in a .ts file, `./glob.js` means `glob.ts`.
    // A .js file importing `./glob.js` means the .js file itself.
    if !ts_importer {
        return names_extensionless(resolved, target);
    }
    for (js, ts) in [
        (".js", &[".ts", ".tsx", ".d.ts"][..]),
        (".mjs", &[".mts"]),
        (".cjs", &[".cts"]),
        (".jsx", &[".tsx"]),
    ] {
        if let Some(stem) = resolved.strip_suffix(js)
            && ts.iter().any(|ext| target.strip_prefix(stem) == Some(ext))
        {
            return true;
        }
    }
    names_extensionless(resolved, target)
}

/// `resolved` names `target` once a script extension (or `/index.<ext>`) is added.
fn names_extensionless(resolved: &str, target: &str) -> bool {
    let Some(rest) = target.strip_prefix(resolved) else {
        return false;
    };
    let ext_of = |s: &str| {
        s.strip_prefix('.')
            .is_some_and(|e| SCRIPT_EXTENSIONS.contains(&e))
    };
    ext_of(rest) || rest.strip_prefix("/index").is_some_and(ext_of)
}

/// `a::b.c/d` → `["a", "b", "c", "d"]`.
fn segments(s: &str) -> Vec<&str> {
    s.split([':', '.', '/']).filter(|p| !p.is_empty()).collect()
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

/// Whitespace-free parameter list with namespace qualifiers dropped, so `(net::URLRequest*, int)`
/// and `(URLRequest *, int)` compare equal. Syntactic: it cannot tell two same-named types apart.
fn normalize_signature(sig: &str) -> String {
    // Whitespace is removed only at the end, so `const ::T` keeps `const` apart from the qualifier.
    let mut out = String::with_capacity(sig.len());
    let mut ident = String::new();
    let mut chars = sig.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            ident.push(c);
            continue;
        }
        if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            ident.clear(); // drop the qualifier
            continue;
        }
        out.push_str(&ident);
        ident.clear();
        out.push(c);
    }
    out.push_str(&ident);
    out.retain(|c| !c.is_whitespace());
    out
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

    fn hit(provenance: Provenance, note: Option<&str>) -> Hit {
        Hit {
            path: "a.rs".to_owned(),
            line: 1,
            provenance,
            detail: String::new(),
            note: note.map(str::to_owned),
            snippet: None,
            oid: String::new(),
        }
    }

    #[test]
    fn label_classes() {
        use LabelClass::*;
        let r = Provenance::Resolved;
        assert_eq!(hit(r, None).label_class(), Resolved);
        assert_eq!(hit(r, Some("ambiguous:3")).label_class(), Ambiguous);
        assert_eq!(
            hit(Provenance::Syntactic, Some("basename")).label_class(),
            Ambiguous
        );
        assert_eq!(hit(r, Some("parse-errors")).label_class(), ParseErrors);
        assert_eq!(
            hit(r, Some("ambiguous:2+parse-errors")).label_class(),
            ParseErrors
        );
        assert_eq!(hit(Provenance::Syntactic, None).label_class(), Syntactic);
        assert_eq!(hit(Provenance::Compiler, None).label_class(), Compiler);
    }

    #[test]
    fn script_imports_resolve_without_extension() {
        assert!(names_file("src/a/b", "src/a/b.ts", true));
        assert!(names_file("src/a/b", "src/a/b/index.tsx", false));
        assert!(names_file("src/a/b", "src/a/b.d.ts", true));
        assert!(!names_file("src/a/b", "src/a/bc.ts", true));
        assert!(!names_file("src/a/b", "src/a/b.rs", true));
        assert!(names_file("src/a/glob.js", "src/a/glob.ts", true));
        assert!(names_file("src/a/view.jsx", "src/a/view.tsx", true));
        assert!(!names_file("src/a/glob.js", "src/a/globs.ts", true));
        // A JavaScript file importing `./foo.js` means foo.js, not a sibling foo.ts.
        assert!(!names_file("src/a/foo.js", "src/a/foo.ts", false));
        assert_eq!(
            classify_include("src/a/b.js", "./foo.js", "src/a/foo.ts", &[]),
            None
        );
        let same = vec!["src/x/foo.ts".to_owned()];
        assert_eq!(
            classify_include("src/x/main.ts", "./foo", "src/x/foo.ts", &same),
            Some((Provenance::Resolved, None))
        );
        assert_eq!(
            classify_include("src/y/main.ts", "./foo", "src/x/foo.ts", &same),
            None
        );
        // `foo.hpp` is not `foo.h`, even though the stems agree.
        assert_eq!(classify_include("a.cc", "foo.hpp", "lib/foo.h", &[]), None);
    }

    #[test]
    fn module_segments() {
        assert!(segments("crate::store::Store").ends_with(&segments("store::Store")));
        assert!(segments("System.Linq").ends_with(&segments("Linq")));
        assert!(!segments("System.Linq").ends_with(&segments("inq")));
    }

    #[test]
    fn sanitize_escapes_controls() {
        assert!(matches!(sanitize("plain::name"), Cow::Borrowed(_)));
        assert_eq!(sanitize("x\nFAKE.c:1"), "x\\u{a}FAKE.c:1");
        assert_eq!(sanitize("\u{1b}]0;pwn\u{7}"), "\\u{1b}]0;pwn\\u{7}");
        assert_eq!(sanitize("a\u{9b}b\u{7f}"), "a\\u{9b}b\\u{7f}");
        assert_eq!(sanitize("tab\there é"), "tab\\u{9}here é");
    }

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
    fn signatures_ignore_qualifiers_and_spacing() {
        assert_eq!(
            normalize_signature("(net::URLRequest* request_, int)"),
            normalize_signature("(URLRequest *request_, int)")
        );
        assert_eq!(normalize_signature("(const ::a::b::T&)"), "(constT&)");
        assert_ne!(
            normalize_signature("(URLRequest*)"),
            normalize_signature("(URLRequest*, int)")
        );
    }

    #[test]
    fn base_edges_respect_lookup_scope() {
        let base = "net::URLRequest::Delegate";
        assert!(base_edge_names(base, "URLRequest::Delegate", "net"));
        assert!(base_edge_names(
            base,
            "net::URLRequest::Delegate",
            "network"
        ));
        assert!(base_edge_names(base, "::net::URLRequest::Delegate", ""));
        // A bare `Delegate` elsewhere is some other class.
        assert!(!base_edge_names(base, "Delegate", "net"));
        assert!(base_edge_names(base, "Delegate", "net::URLRequest"));
        assert!(!base_edge_names(base, "Other::Delegate", "net"));
        assert!(same_class("net::Foo", "Foo") && !same_class("net::Foo", "net::XFoo"));
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
