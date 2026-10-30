//! C and C++ (and Objective-C++ on a best-effort basis) via tree-sitter-cpp.

use std::cell::RefCell;
use std::collections::HashSet;

use oi_model::{BaseClass, FileFacts, Include, Lang, RefKind, Reference, Symbol, SymbolKind};
use tree_sitter::{Node, Parser, Tree};

use crate::{ExtractError, Extractor, SyntaxError, extension};

mod macros;

const EXTENSIONS: &[&str] = &[
    "c", "h", "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "inl", "ipp", "tcc", "mm",
];

/// Return-type macros that declare a virtual method (`NS_IMETHOD` expands to `virtual nsresult`).
const DEFAULT_VIRTUAL_MACROS: &[&str] = &["NS_IMETHOD", "NS_IMETHOD_", "STDMETHOD", "STDMETHOD_"];

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

/// Extracts definitions, references, includes and base classes from C/C++ sources.
pub struct CppExtractor {
    virtual_macros: Vec<String>,
}

impl Default for CppExtractor {
    fn default() -> Self {
        Self {
            virtual_macros: DEFAULT_VIRTUAL_MACROS
                .iter()
                .map(|&s| s.to_owned())
                .collect(),
        }
    }
}

impl Extractor for CppExtractor {
    fn lang(&self) -> Lang {
        Lang::Cpp
    }

    fn handles(&self, path: &str) -> bool {
        extension(path).is_some_and(|e| EXTENSIONS.contains(&e.as_str()))
    }

    fn syntax_errors(&self, source: &[u8]) -> Result<Vec<SyntaxError>, ExtractError> {
        let (rewritten, _) = macros::neutralize(source);
        let source = rewritten.as_deref().unwrap_or(source);
        let tree = parse(source)?;
        let mut out = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.is_error() || node.is_missing() {
                let mut ctx = node.parent();
                while let Some(p) = ctx.filter(Node::is_error) {
                    ctx = p.parent();
                }
                let end = node.end_byte().min(node.start_byte() + 80);
                out.push(SyntaxError {
                    line: line_of(node),
                    context: ctx.map_or("", |p| p.kind()).to_owned(),
                    text: String::from_utf8_lossy(&source[node.start_byte()..end])
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                });
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        out.sort_by_key(|e| e.line);
        Ok(out)
    }

    fn extract(&self, source: &[u8]) -> Result<FileFacts, ExtractError> {
        let (rewritten, blanked) = macros::neutralize(source);
        let source = rewritten.as_deref().unwrap_or(source);
        let tree = parse(source)?;
        let mut walker = Walker {
            src: source,
            virtual_macros: &self.virtual_macros,
            facts: FileFacts::default(),
            scopes: Vec::new(),
            functions: Vec::new(),
            name_nodes: HashSet::new(),
            seen_refs: HashSet::new(),
        };
        walker.walk(tree.root_node());
        for b in blanked {
            walker.push_ref(&b.name, RefKind::Use, b.line);
        }
        Ok(walker.facts)
    }
}

/// Parses with this thread's cached parser.
fn parse(source: &[u8]) -> Result<Tree, ExtractError> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_cpp::LANGUAGE.into())
                .map_err(|_| ExtractError::Grammar("cpp"))?;
            *slot = Some(parser);
        }
        slot.as_mut()
            .expect("parser initialized above")
            .parse(source, None)
            .ok_or(ExtractError::NoTree)
    })
}

/// An open scope and the syntax node that opened it.
struct Scope {
    node_id: usize,
    name: String,
}

struct Walker<'a> {
    src: &'a [u8],
    virtual_macros: &'a [String],
    facts: FileFacts,
    scopes: Vec<Scope>,
    /// Enclosing function definitions, innermost last; references record the innermost.
    functions: Vec<Scope>,
    /// Byte offsets of identifiers that name a definition; these are not references.
    name_nodes: HashSet<usize>,
    /// (name, kind, line) already recorded, so a line mentioning a name twice yields one ref.
    seen_refs: HashSet<(String, RefKind, u32)>,
}

impl<'a> Walker<'a> {
    /// Iterative pre-order walk; deep expression trees in generated code would overflow a
    /// recursive one.
    fn walk(&mut self, root: Node<'a>) {
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            self.enter(node);
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                self.leave(cursor.node());
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        std::str::from_utf8(&self.src[node.byte_range()]).unwrap_or("")
    }

    fn scope(&self) -> String {
        self.scopes
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::")
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

    fn enter(&mut self, node: Node<'a>) {
        if node.is_error() || node.is_missing() {
            self.facts.parse_errors += 1;
        }
        match node.kind() {
            "preproc_include" => self.include(node),
            "preproc_def" | "preproc_function_def" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.symbol(
                        name,
                        self.text(name).to_owned(),
                        "",
                        SymbolKind::Macro,
                        true,
                    );
                }
            }
            "namespace_definition" => self.namespace(node),
            "class_specifier" | "struct_specifier" | "union_specifier" => self.record(node),
            "enum_specifier" => self.enumeration(node),
            "enumerator" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let scope = self.scope();
                    self.symbol(
                        name,
                        self.text(name).to_owned(),
                        &scope,
                        SymbolKind::Enumerator,
                        true,
                    );
                }
            }
            "function_definition" => self.function(node, true),
            "field_declaration" | "declaration" => self.declaration(node),
            "type_definition" => self.typedef(node),
            "alias_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let scope = self.scope();
                    self.symbol(
                        name,
                        self.text(name).to_owned(),
                        &scope,
                        SymbolKind::Typedef,
                        true,
                    );
                }
            }
            "call_expression" => self.call(node),
            "type_identifier" => self.reference(node, RefKind::Type),
            "identifier" | "field_identifier" | "namespace_identifier" => {
                // Inside error recovery (e.g. Objective-C in a .mm file) no call_expression is
                // built, but `name(` is still a call.
                let called = node.parent().is_some_and(|p| p.is_error())
                    && node
                        .next_sibling()
                        .is_some_and(|s| matches!(s.kind(), "(" | "argument_list"));
                self.reference(node, if called { RefKind::Call } else { RefKind::Use });
            }
            _ => {}
        }
    }

    fn include(&mut self, node: Node<'_>) {
        let Some(path) = node.child_by_field_name("path") else {
            return;
        };
        let raw = self.text(path);
        let system = path.kind() == "system_lib_string";
        let spec = raw
            .trim_matches(|c| c == '"' || c == '<' || c == '>')
            .to_owned();
        self.facts.includes.push(Include {
            spec,
            system,
            line: line_of(node),
        });
    }

    fn namespace(&mut self, node: Node<'a>) {
        // Anonymous namespaces open a scope too, so their members are not mistaken for globals.
        let name = node
            .child_by_field_name("name")
            .map_or_else(|| "(anonymous)".to_owned(), |n| self.text(n).to_owned());
        if let Some(n) = node.child_by_field_name("name") {
            let scope = self.scope();
            self.symbol(n, name.clone(), &scope, SymbolKind::Namespace, true);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.scopes.push(Scope {
                node_id: body.id(),
                name,
            });
        }
    }

    fn record(&mut self, node: Node<'a>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let (qualifiers, name) = self.split_name(name_node, true);
        let kind = match node.kind() {
            "class_specifier" => SymbolKind::Class,
            "struct_specifier" => SymbolKind::Struct,
            _ => SymbolKind::Union,
        };
        let body = node.child_by_field_name("body");
        let scope = join_scope(&self.scope(), &qualifiers);
        // `class Foo;` (no body) is a forward declaration; `struct Foo x;` is a type use.
        if body.is_some() {
            self.symbol(name_node, name.clone(), &scope, kind, true);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "base_class_clause" {
                self.bases(child, &name, &scope);
            }
        }
        if let Some(body) = body {
            self.scopes.push(Scope {
                node_id: body.id(),
                name: if qualifiers.is_empty() {
                    name
                } else {
                    format!("{}::{name}", qualifiers.join("::"))
                },
            });
        }
    }

    fn bases(&mut self, clause: Node<'_>, derived: &str, derived_scope: &str) {
        let mut cursor = clause.walk();
        for child in clause.named_children(&mut cursor) {
            let (qualifiers, base) = match child.kind() {
                "type_identifier" | "qualified_identifier" | "template_type" => {
                    self.split_name(child, false)
                }
                _ => continue,
            };
            let base_written = join_scope(&qualifiers.join("::"), std::slice::from_ref(&base));
            self.facts.bases.push(BaseClass {
                derived: derived.to_owned(),
                derived_scope: derived_scope.to_owned(),
                base,
                base_written,
                line: line_of(child),
            });
        }
    }

    fn enumeration(&mut self, node: Node<'a>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        if node.child_by_field_name("body").is_some() {
            let scope = self.scope();
            self.symbol(
                name,
                self.text(name).to_owned(),
                &scope,
                SymbolKind::Enum,
                true,
            );
        }
    }

    fn typedef(&mut self, node: Node<'a>) {
        let mut cursor = node.walk();
        let declarators: Vec<_> = node
            .children_by_field_name("declarator", &mut cursor)
            .collect();
        for decl in declarators {
            if let Some(id) = innermost_name(decl) {
                let scope = self.scope();
                self.symbol(
                    id,
                    self.text(id).to_owned(),
                    &scope,
                    SymbolKind::Typedef,
                    true,
                );
            }
        }
    }

    /// `declaration` (namespace/block scope) or `field_declaration` (class body).
    fn declaration(&mut self, node: Node<'a>) {
        let in_class = node.kind() == "field_declaration";
        // Block-scope declarations are locals; `Foo bar(baz);` there is a constructor call.
        if !in_class && !at_namespace_scope(node) {
            return;
        }
        let mut cursor = node.walk();
        let declarators: Vec<_> = node
            .children_by_field_name("declarator", &mut cursor)
            .collect();
        let member_kind = if in_class {
            SymbolKind::Field
        } else {
            SymbolKind::Variable
        };
        for decl in declarators {
            if let Some(func) = find_function_declarator(decl) {
                self.function_like(node, func, false);
            } else if let Some(id) = innermost_name(decl) {
                let scope = self.scope();
                self.symbol(id, self.text(id).to_owned(), &scope, member_kind, true);
            }
        }
    }

    fn function(&mut self, node: Node<'a>, is_definition: bool) {
        if let Some(decl) = node.child_by_field_name("declarator")
            && let Some(func) = find_function_declarator(decl)
        {
            self.function_like(node, func, is_definition);
        }
    }

    fn function_like(&mut self, owner: Node<'a>, func: Node<'a>, is_definition: bool) {
        let Some(name_node) = func.child_by_field_name("declarator") else {
            return;
        };
        let (qualifiers, name) = self.split_name(name_node, true);
        if name.is_empty() {
            return;
        }
        let enclosing = self.scope();
        let in_class = owner.kind() == "field_declaration"
            || (owner.kind() == "function_definition"
                && owner
                    .parent()
                    .is_some_and(|p| p.kind() == "field_declaration_list"));
        let kind = if in_class || !qualifiers.is_empty() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let signature = func
            .child_by_field_name("parameters")
            .map(|p| self.signature(p))
            .unwrap_or_default();
        let is_virtual = self.is_virtual(owner, func);
        let scope = join_scope(&enclosing, &qualifiers);
        if is_definition {
            let qualified = if scope.is_empty() {
                name.clone()
            } else {
                format!("{scope}::{name}")
            };
            self.functions.push(Scope {
                node_id: owner.id(),
                name: qualified,
            });
        }
        self.facts.symbols.push(Symbol {
            name,
            scope,
            kind,
            line: line_of(name_node),
            is_definition,
            signature,
            is_virtual,
        });
    }

    fn is_virtual(&self, owner: Node<'_>, func: Node<'_>) -> bool {
        let mut cursor = owner.walk();
        let virtual_kw = owner.children(&mut cursor).any(|c| c.kind() == "virtual");
        let mut cursor = func.walk();
        let specifier = func
            .children(&mut cursor)
            .any(|c| c.kind() == "virtual_specifier");
        let macro_type = owner.child_by_field_name("type").is_some_and(|t| {
            let ty = self.text(t);
            self.virtual_macros
                .iter()
                .any(|m| ty == m || ty.starts_with(&format!("{m}(")))
        });
        virtual_kw || specifier || macro_type
    }

    /// Normalized parameter types, e.g. `(nsIRequest*, const nsACString&)`.
    fn signature(&self, params: Node<'_>) -> String {
        let mut cursor = params.walk();
        let parts: Vec<String> = params
            .named_children(&mut cursor)
            .filter_map(|p| match p.kind() {
                "parameter_declaration" | "optional_parameter_declaration" => {
                    Some(self.param_type(p))
                }
                "variadic_parameter_declaration" | "variadic_parameter" => Some("...".to_owned()),
                _ => None,
            })
            .collect();
        format!("({})", parts.join(", "))
    }

    fn param_type(&self, param: Node<'_>) -> String {
        let mut out = String::new();
        let mut cursor = param.walk();
        for child in param.children(&mut cursor) {
            if child.kind() == "type_qualifier" {
                out.push_str(self.text(child));
                out.push(' ');
            }
        }
        if let Some(ty) = param.child_by_field_name("type") {
            out.push_str(&collapse_ws(self.text(ty)));
        }
        let mut decl = param.child_by_field_name("declarator");
        while let Some(d) = decl {
            match d.kind() {
                "pointer_declarator" | "abstract_pointer_declarator" => out.push('*'),
                "reference_declarator" | "abstract_reference_declarator" => {
                    out.push_str(if self.text(d).starts_with("&&") {
                        "&&"
                    } else {
                        "&"
                    });
                }
                "array_declarator" | "abstract_array_declarator" => out.push_str("[]"),
                _ => {}
            }
            decl = d.child_by_field_name("declarator").or_else(|| {
                d.named_child(0)
                    .filter(|c| c.kind().ends_with("declarator"))
            });
        }
        out
    }

    fn call(&mut self, node: Node<'a>) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = match func.kind() {
            "identifier" => Some(func),
            "field_expression" => func.child_by_field_name("field"),
            "qualified_identifier" | "template_function" => {
                let mut n = func;
                while let Some(inner) = n.child_by_field_name("name") {
                    n = inner;
                }
                Some(n)
            }
            _ => None,
        };
        if let Some(target) = target {
            let name = self.text(target);
            if !name.is_empty() {
                self.push_ref(name, RefKind::Call, line_of(target));
                // The identifier itself is visited next; don't count it again as a plain use.
                self.name_nodes.insert(target.start_byte());
            }
        }
    }

    fn reference(&mut self, node: Node<'_>, kind: RefKind) {
        if self.name_nodes.contains(&node.start_byte()) {
            return;
        }
        let name = self.text(node);
        if !name.is_empty() {
            self.push_ref(name, kind, line_of(node));
        }
    }

    fn push_ref(&mut self, name: &str, kind: RefKind, line: u32) {
        if self.seen_refs.insert((name.to_owned(), kind, line)) {
            self.facts.references.push(Reference {
                name: name.to_owned(),
                kind,
                line,
                container: self.functions.last().map(|f| f.name.clone()),
            });
        }
    }

    fn symbol(
        &mut self,
        name_node: Node<'_>,
        name: String,
        scope: &str,
        kind: SymbolKind,
        is_definition: bool,
    ) {
        if name.is_empty() {
            return;
        }
        self.name_nodes.insert(name_node.start_byte());
        self.facts.symbols.push(Symbol {
            name,
            scope: scope.to_owned(),
            kind,
            line: line_of(name_node),
            is_definition,
            signature: String::new(),
            is_virtual: false,
        });
    }

    /// Splits `a::b::Name<T>` into (`["a", "b"]`, `"Name"`). Template arguments are dropped.
    /// When `defining`, the name parts are recorded as definition sites rather than references.
    fn split_name(&mut self, node: Node<'_>, defining: bool) -> (Vec<String>, String) {
        let mut qualifiers = Vec::new();
        let mut n = node;
        loop {
            match n.kind() {
                "qualified_identifier" => {
                    if let Some(scope) = n.child_by_field_name("scope") {
                        let scope_name = match scope.kind() {
                            "template_type" => scope
                                .child_by_field_name("name")
                                .map_or("", |s| self.text(s)),
                            _ => self.text(scope),
                        };
                        if !scope_name.is_empty() {
                            qualifiers.push(scope_name.to_owned());
                        }
                        if defining {
                            self.name_nodes.insert(scope.start_byte());
                        }
                    }
                    match n.child_by_field_name("name") {
                        Some(inner) => n = inner,
                        None => return (qualifiers, String::new()),
                    }
                }
                "template_type" | "template_function" => match n.child_by_field_name("name") {
                    Some(inner) => n = inner,
                    None => return (qualifiers, String::new()),
                },
                _ => {
                    if defining {
                        self.name_nodes.insert(n.start_byte());
                    }
                    return (qualifiers, collapse_ws(self.text(n)));
                }
            }
        }
    }
}

fn line_of(node: Node<'_>) -> u32 {
    u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX)
}

fn join_scope(enclosing: &str, qualifiers: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !enclosing.is_empty() {
        parts.push(enclosing);
    }
    parts.extend(qualifiers.iter().map(String::as_str));
    parts.join("::")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unwraps pointer/reference/parenthesized declarators to reach a `function_declarator`.
fn find_function_declarator(decl: Node<'_>) -> Option<Node<'_>> {
    let mut cur = Some(decl);
    while let Some(d) = cur {
        match d.kind() {
            "function_declarator" => return Some(d),
            "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator"
            | "init_declarator" => {
                // A pointer to function (`void (*f)()`) is a variable, not a function.
                if d.kind() == "parenthesized_declarator" {
                    return None;
                }
                cur = d
                    .child_by_field_name("declarator")
                    .or_else(|| d.named_child(0));
            }
            _ => return None,
        }
    }
    None
}

/// The identifier a (possibly nested) declarator ultimately declares.
fn innermost_name(decl: Node<'_>) -> Option<Node<'_>> {
    let mut cur = decl;
    loop {
        match cur.kind() {
            "identifier" | "field_identifier" | "type_identifier" => return Some(cur),
            _ => {
                cur = cur
                    .child_by_field_name("declarator")
                    .or_else(|| cur.named_child(0))?;
            }
        }
    }
}

/// Locals are not symbols: a declaration counts only outside any function body.
fn at_namespace_scope(node: Node<'_>) -> bool {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if matches!(p.kind(), "compound_statement" | "lambda_expression") {
            return false;
        }
        cur = p.parent();
    }
    true
}
