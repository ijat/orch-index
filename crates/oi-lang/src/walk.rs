//! Language-independent extraction machinery: the iterative tree walk, scope tracking and fact
//! collection that every tree-sitter extractor shares.

use std::cell::RefCell;
use std::collections::HashSet;
use std::thread::LocalKey;

use oi_model::{BaseClass, FileFacts, Include, RefKind, Reference, Symbol, SymbolKind};
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, Parser, Tree, TreeCursor};

use crate::ExtractError;

/// Per-thread parser slot; each extractor declares one with `thread_local!`.
pub(crate) type ParserSlot = RefCell<Option<Parser>>;

/// Parses `source` with this thread's parser for `language`, creating it on first use.
pub(crate) fn parse(
    slot: &'static LocalKey<ParserSlot>,
    language: &Language,
    name: &'static str,
    source: &[u8],
) -> Result<Tree, ExtractError> {
    slot.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            parser
                .set_language(language)
                .map_err(|_| ExtractError::Grammar(name))?;
            *slot = Some(parser);
        }
        slot.as_mut()
            .expect("parser initialized above")
            .parse(source, None)
            .ok_or(ExtractError::NoTree)
    })
}

/// An open scope and the syntax node that closes it when left.
struct Scope {
    node_id: usize,
    name: String,
}

/// Facts collected so far plus the walk's scope state.
pub(crate) struct State<'a> {
    pub src: &'a [u8],
    pub facts: FileFacts,
    scopes: Vec<Scope>,
    /// Enclosing function definitions, innermost last; references record the innermost.
    functions: Vec<Scope>,
    /// Byte offsets of identifiers that name a definition; these are not references.
    name_nodes: HashSet<usize>,
    /// (name, kind, line) already recorded, so a line mentioning a name twice yields one ref.
    seen_refs: HashSet<(String, RefKind, u32)>,
    /// Ancestors of the node being entered, root first.
    ancestors: Vec<Node<'a>>,
    /// The previous sibling of the node being entered.
    prev: Option<Node<'a>>,
    /// The walk skipped part of the tree (depth cap or time budget).
    truncated: bool,
}

impl<'a> State<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            facts: FileFacts::default(),
            scopes: Vec::new(),
            functions: Vec::new(),
            name_nodes: HashSet::new(),
            seen_refs: HashSet::new(),
            ancestors: Vec::new(),
            prev: None,
            truncated: false,
        }
    }

    /// Parent of the node being entered.
    pub fn parent(&self) -> Option<Node<'a>> {
        self.ancestors.last().copied()
    }

    /// Ancestors of the node being entered, innermost first.
    pub fn ancestors(&self) -> impl Iterator<Item = Node<'a>> + '_ {
        self.ancestors.iter().rev().copied()
    }

    /// The `n`-th ancestor of the node being entered (1 = parent).
    pub fn ancestor(&self, n: usize) -> Option<Node<'a>> {
        self.ancestors
            .len()
            .checked_sub(n)
            .map(|i| self.ancestors[i])
    }

    /// The previous sibling of the node being entered.
    pub fn prev_sibling(&self) -> Option<Node<'a>> {
        self.prev
    }

    /// Whether an error node being entered sits at declaration level rather than inside a
    /// function body.
    fn is_structural_error(&self) -> bool {
        self.ancestors()
            .find(|p| !p.is_error())
            .is_none_or(|p| DECLARATION_CONTAINERS.contains(&p.kind()))
    }

    /// Records that the walk stopped early: facts from this file are incomplete.
    fn truncate(&mut self) {
        self.truncated = true;
        self.facts.structural_errors += 1;
    }

    fn truncate_once(&mut self) {
        if !self.truncated {
            self.truncate();
        }
    }

    pub fn text(&self, node: Node<'_>) -> &'a str {
        std::str::from_utf8(&self.src[node.byte_range()]).unwrap_or("")
    }

    /// The current scope, `::`-joined.
    pub fn scope(&self) -> String {
        self.scopes
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::")
    }

    /// Opens a scope that closes when the walk leaves `until`.
    pub fn push_scope(&mut self, until: Node<'_>, name: String) {
        self.scopes.push(Scope {
            node_id: until.id(),
            name,
        });
    }

    /// Marks `until` as a function body: references inside it record `qualified` as container.
    pub fn push_function(&mut self, until: Node<'_>, qualified: String) {
        self.functions.push(Scope {
            node_id: until.id(),
            name: qualified,
        });
    }

    fn leave(&mut self, node: Node<'_>) {
        if self.scopes.last().is_some_and(|s| s.node_id == node.id()) {
            self.scopes.pop();
        }
        if self
            .functions
            .last()
            .is_some_and(|s| s.node_id == node.id())
        {
            self.functions.pop();
        }
    }

    /// Records that `node` is a definition's name, so it is not also counted as a reference.
    pub fn mark_name(&mut self, node: Node<'_>) {
        self.name_nodes.insert(node.start_byte());
    }

    pub fn is_name(&self, node: Node<'_>) -> bool {
        self.name_nodes.contains(&node.start_byte())
    }

    pub fn push_ref(&mut self, name: &str, kind: RefKind, line: u32) {
        if name.is_empty() {
            return;
        }
        if self.seen_refs.insert((name.to_owned(), kind, line)) {
            self.facts.references.push(Reference {
                name: name.to_owned(),
                kind,
                line,
                container: self.functions.last().map(|f| f.name.clone()),
            });
        }
    }

    /// Records `node` as a reference of `kind` unless it names a definition.
    pub fn reference(&mut self, node: Node<'_>, kind: RefKind) {
        if !self.is_name(node) {
            let name = self.text(node);
            self.push_ref(name, kind, line_of(node));
        }
    }

    /// Records a plain (non-function) symbol whose name is `name_node`'s text.
    pub fn symbol(&mut self, name_node: Node<'_>, scope: &str, kind: SymbolKind) {
        let name = collapse_ws(self.text(name_node));
        self.symbol_named(name_node, name, scope, kind);
    }

    /// Records a plain symbol with an explicit name (e.g. a qualifier-stripped one).
    pub fn symbol_named(
        &mut self,
        name_node: Node<'_>,
        name: String,
        scope: &str,
        kind: SymbolKind,
    ) {
        if name.is_empty() {
            return;
        }
        self.mark_name(name_node);
        self.facts.symbols.push(Symbol {
            name,
            scope: scope.to_owned(),
            kind,
            line: line_of(name_node),
            is_definition: true,
            signature: String::new(),
            is_virtual: false,
        });
    }

    pub fn push_symbol(&mut self, symbol: Symbol) {
        self.facts.symbols.push(symbol);
    }

    pub fn include(&mut self, spec: String, system: bool, line: u32) {
        self.import(spec, system, false, line);
    }

    /// Records an import of a module or namespace rather than a file.
    pub fn import_module(&mut self, spec: String, line: u32) {
        self.import(spec, false, true, line);
    }

    fn import(&mut self, spec: String, system: bool, module: bool, line: u32) {
        if !spec.is_empty() {
            self.facts.includes.push(Include {
                spec,
                system,
                module,
                line,
            });
        }
    }

    pub fn base(&mut self, derived: &str, derived_scope: &str, written: &str, line: u32) {
        let written = written.trim_start_matches("::");
        let base = written.rsplit("::").next().unwrap_or(written);
        if derived.is_empty() || base.is_empty() {
            return;
        }
        self.facts.bases.push(BaseClass {
            derived: derived.to_owned(),
            derived_scope: derived_scope.to_owned(),
            base: base.to_owned(),
            base_written: written.to_owned(),
            line,
        });
    }
}

/// Node kinds that hold declarations (file, namespace, module and class bodies) across the
/// supported grammars. A syntax error directly inside one of these is structural.
const DECLARATION_CONTAINERS: &[&str] = &[
    "translation_unit",
    "source_file",
    "program",
    "compilation_unit",
    "declaration_list",
    "field_declaration_list",
    "class_body",
    "interface_body",
    "enum_body",
    "enum_variant_list",
    "object_type",
];

/// Deeper nodes are not visited. Real code stays far below this; hostile or generated input
/// (thousands of nested namespaces or parentheses) would otherwise cost time and memory
/// quadratic in depth.
const MAX_DEPTH: usize = 1024;

/// Wall-clock budget for walking one file. Past it the walk stops and the file is reported as
/// damaged, so one pathological file cannot stall an index run.
const TIME_BUDGET: Duration = Duration::from_secs(5);

/// A language's node handler, driven by [`walk`].
pub(crate) trait Visit<'a> {
    fn state(&mut self) -> &mut State<'a>;
    /// Called for each node in pre-order. `cursor` points at `node`; use [`next_siblings`] to
    /// look ahead cheaply, and [`State::parent`] / [`State::ancestors`] /
    /// [`State::prev_sibling`] instead of the O(depth) or O(siblings) tree-sitter equivalents.
    fn enter(&mut self, node: Node<'a>, cursor: &TreeCursor<'a>);
}

/// The siblings after the cursor's node, in order. Each step is O(1), unlike
/// `Node::next_sibling`, which rescans the parent's children.
pub(crate) fn next_siblings<'a>(cursor: &TreeCursor<'a>) -> impl Iterator<Item = Node<'a>> {
    let mut c = cursor.clone();
    std::iter::from_fn(move || c.goto_next_sibling().then(|| c.node()))
}

/// Iterative pre-order walk; deep expression trees in generated code would overflow a recursive
/// one. Counts syntax-error nodes, tracks ancestors, and closes scopes as their nodes are left.
pub(crate) fn walk<'a, V: Visit<'a>>(visitor: &mut V, root: Node<'a>) {
    let start = Instant::now();
    let mut visited: u64 = 0;
    let mut cursor = root.walk();
    visitor.state().ancestors.clear();
    visitor.state().prev = None;
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            let structural = visitor.state().is_structural_error();
            let facts = &mut visitor.state().facts;
            facts.parse_errors += 1;
            if structural {
                facts.structural_errors += 1;
            }
        }
        visitor.enter(node, &cursor);
        visited += 1;
        if visited.is_multiple_of(4096) && start.elapsed() > TIME_BUDGET {
            visitor.state().truncate();
            return;
        }
        let st = visitor.state();
        if st.ancestors.len() < MAX_DEPTH {
            if cursor.goto_first_child() {
                st.ancestors.push(node);
                st.prev = None;
                continue;
            }
        } else if node.child_count() > 0 {
            st.truncate_once();
        }
        loop {
            let left = cursor.node();
            visitor.state().leave(left);
            if cursor.goto_next_sibling() {
                visitor.state().prev = Some(left);
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
            let st = visitor.state();
            st.ancestors.pop();
            st.prev = None; // unknown for the parent; only read on entering a node
        }
    }
}

/// Syntax-error sites in a parse, for diagnostics.
pub(crate) fn syntax_errors(tree: &Tree, source: &[u8]) -> Vec<crate::SyntaxError> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            let mut ctx = node.parent();
            while let Some(p) = ctx.filter(Node::is_error) {
                ctx = p.parent();
            }
            let end = node.end_byte().min(node.start_byte() + 80);
            out.push(crate::SyntaxError {
                line: line_of(node),
                context: ctx.map_or("", |p| p.kind()).to_owned(),
                text: collapse_ws(&String::from_utf8_lossy(&source[node.start_byte()..end])),
            });
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out.sort_by_key(|e| e.line);
    out
}

pub(crate) fn line_of(node: Node<'_>) -> u32 {
    u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX)
}

/// `enclosing::q1::q2`, skipping empty parts.
pub(crate) fn join_scope(enclosing: &str, qualifiers: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !enclosing.is_empty() {
        parts.push(enclosing);
    }
    parts.extend(
        qualifiers
            .iter()
            .map(String::as_str)
            .filter(|q| !q.is_empty()),
    );
    parts.join("::")
}

pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Qualified name of a definition: `scope::name`, or just `name` at the top level.
pub(crate) fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}::{name}")
    }
}
