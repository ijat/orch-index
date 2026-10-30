//! JavaScript and TypeScript (including JSX/TSX) via tree-sitter-javascript/-typescript.
//!
//! Mapping: classes, interfaces, enums, type aliases and TS namespaces are symbols; methods are
//! scoped to their class; `extends`/`implements` are base edges; `const f = () => …` at module
//! level is a function. Relative `import`/`require`/`import()` specs are file includes, bare
//! package specs (`react`, `@scope/pkg`) are module imports.

use oi_model::{FileFacts, Lang, RefKind, Symbol, SymbolKind};
use tree_sitter::Node;

use crate::walk::{self, ParserSlot, State, Visit, collapse_ws, line_of, qualify};
use crate::{ExtractError, Extractor, SyntaxError, extension};

thread_local! {
    static JS: ParserSlot = const { ParserSlot::new(None) };
    static TS: ParserSlot = const { ParserSlot::new(None) };
    static TSX: ParserSlot = const { ParserSlot::new(None) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    JavaScript,
    TypeScript,
    Tsx,
}

impl Dialect {
    fn of(path: &str) -> Option<Self> {
        // `.d.ts` is TypeScript; the last extension decides.
        match extension(path)?.as_str() {
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn parse(self, source: &[u8]) -> Result<tree_sitter::Tree, ExtractError> {
        match self {
            Self::JavaScript => walk::parse(
                &JS,
                &tree_sitter_javascript::LANGUAGE.into(),
                "javascript",
                source,
            ),
            Self::TypeScript => walk::parse(
                &TS,
                &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "typescript",
                source,
            ),
            Self::Tsx => walk::parse(
                &TSX,
                &tree_sitter_typescript::LANGUAGE_TSX.into(),
                "tsx",
                source,
            ),
        }
    }
}

/// Extracts JavaScript or TypeScript facts; one instance per language so `lang()` is exact.
pub struct ScriptExtractor {
    typescript: bool,
}

impl ScriptExtractor {
    #[must_use]
    pub fn javascript() -> Self {
        Self { typescript: false }
    }

    #[must_use]
    pub fn typescript() -> Self {
        Self { typescript: true }
    }

    fn extract_as(dialect: Dialect, source: &[u8]) -> Result<FileFacts, ExtractError> {
        let tree = dialect.parse(source)?;
        let mut walker = Walker {
            st: State::new(source),
        };
        walk::walk(&mut walker, tree.root_node());
        Ok(walker.st.facts)
    }
}

impl Extractor for ScriptExtractor {
    fn lang(&self) -> Lang {
        if self.typescript {
            Lang::TypeScript
        } else {
            Lang::JavaScript
        }
    }

    fn handles(&self, path: &str) -> bool {
        Dialect::of(path).is_some_and(|d| (d != Dialect::JavaScript) == self.typescript)
    }

    fn extract(&self, source: &[u8]) -> Result<FileFacts, ExtractError> {
        // Without a path, `.tsx` vs `.ts` is unknown; TSX parses a superset except for `<T>x`
        // casts, so use the precise dialect when called through `extract_path`.
        let dialect = if self.typescript {
            Dialect::TypeScript
        } else {
            Dialect::JavaScript
        };
        Self::extract_as(dialect, source)
    }

    fn extract_path(&self, path: &str, source: &[u8]) -> Result<FileFacts, ExtractError> {
        match Dialect::of(path) {
            Some(d) => Self::extract_as(d, source),
            None => self.extract(source),
        }
    }

    fn syntax_errors_path(
        &self,
        path: &str,
        source: &[u8],
    ) -> Result<Vec<SyntaxError>, ExtractError> {
        let dialect = Dialect::of(path).unwrap_or(Dialect::JavaScript);
        Ok(walk::syntax_errors(&dialect.parse(source)?, source))
    }
}

struct Walker<'a> {
    st: State<'a>,
}

impl<'a> Visit<'a> for Walker<'a> {
    fn state(&mut self) -> &mut State<'a> {
        &mut self.st
    }

    fn enter(&mut self, node: Node<'a>) {
        match node.kind() {
            "import_statement" | "export_statement" => {
                if let Some(src) = node.child_by_field_name("source") {
                    self.import(src, line_of(node));
                }
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                self.class(node, SymbolKind::Class);
            }
            "interface_declaration" => self.class(node, SymbolKind::Interface),
            "enum_declaration" => self.container(node, SymbolKind::Enum),
            "internal_module" | "module" => self.container(node, SymbolKind::Namespace),
            "type_alias_declaration" => self.named(node, SymbolKind::Typedef),
            "function_declaration" | "generator_function_declaration" => {
                self.function(node, node, SymbolKind::Function, true);
            }
            "method_definition" => self.function(node, node, SymbolKind::Method, true),
            "method_signature" | "abstract_method_signature" => {
                self.function(node, node, SymbolKind::Method, false);
            }
            "public_field_definition" | "field_definition" | "property_signature" => {
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("property"));
                if let Some(name) = name {
                    let scope = self.st.scope();
                    self.st.symbol(name, &scope, SymbolKind::Property);
                }
            }
            "variable_declarator" => self.variable(node),
            "call_expression" => self.call(node),
            "new_expression" => {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    let target = member_name(ctor);
                    let name = self.st.text(target);
                    self.st.push_ref(name, RefKind::Call, line_of(target));
                    self.st.mark_name(target);
                }
            }
            "type_identifier" => self.st.reference(node, RefKind::Type),
            "identifier" | "property_identifier" | "shorthand_property_identifier" => {
                self.st.reference(node, RefKind::Use);
            }
            _ => {}
        }
    }
}

impl<'a> Walker<'a> {
    fn import(&mut self, spec_node: Node<'_>, line: u32) {
        let spec = unquote(self.st.text(spec_node));
        if spec.is_empty() {
            return;
        }
        if spec.starts_with('.') || spec.starts_with('/') {
            self.st.include(spec.to_owned(), false, line);
        } else {
            self.st.import_module(spec.to_owned(), line);
        }
    }

    fn named(&mut self, node: Node<'a>, kind: SymbolKind) {
        if let Some(name) = node.child_by_field_name("name") {
            let scope = self.st.scope();
            self.st.symbol(name, &scope, kind);
        }
    }

    fn container(&mut self, node: Node<'a>, kind: SymbolKind) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let scope = self.st.scope();
        self.st.symbol(name, &scope, kind);
        if let Some(body) = node.child_by_field_name("body") {
            let name = self.st.text(name).replace('.', "::");
            self.st.push_scope(body, name);
        }
    }

    /// Class or interface: symbol, base edges, and a scope for its members.
    fn class(&mut self, node: Node<'a>, kind: SymbolKind) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return; // anonymous class expression
        };
        let name = self.st.text(name_node).to_owned();
        let scope = self.st.scope();
        self.st.symbol(name_node, &scope, kind);
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                // JS: `class_heritage` holds the extended expression directly; TS wraps it in
                // `extends_clause` / `implements_clause`.
                "class_heritage" => {
                    let mut inner = child.walk();
                    for part in child.named_children(&mut inner) {
                        match part.kind() {
                            "extends_clause" => {
                                if let Some(v) = part.child_by_field_name("value") {
                                    self.base_edge(&name, &scope, v);
                                }
                            }
                            "implements_clause" => self.base_list(&name, &scope, part),
                            _ => self.base_edge(&name, &scope, part),
                        }
                    }
                }
                "extends_type_clause" => self.base_list(&name, &scope, child),
                _ => {}
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.st.push_scope(body, name);
        }
    }

    fn base_list(&mut self, derived: &str, scope: &str, list: Node<'_>) {
        let mut cursor = list.walk();
        for ty in list.named_children(&mut cursor) {
            self.base_edge(derived, scope, ty);
        }
    }

    fn base_edge(&mut self, derived: &str, scope: &str, ty: Node<'_>) {
        let ty = if ty.kind() == "generic_type" {
            ty.child_by_field_name("name").unwrap_or(ty)
        } else {
            ty
        };
        let written: String = self.st.text(ty).split_whitespace().collect();
        let written = written.split('<').next().unwrap_or("").replace('.', "::");
        self.st.base(derived, scope, &written, line_of(ty));
    }

    fn function(&mut self, node: Node<'a>, owner: Node<'a>, kind: SymbolKind, is_definition: bool) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = unquote(self.st.text(name_node)).to_owned();
        let scope = self.st.scope();
        self.st.mark_name(name_node);
        if is_definition {
            self.st.push_function(owner, qualify(&scope, &name));
        }
        let signature = node
            .child_by_field_name("parameters")
            .map(|p| self.signature(p))
            .unwrap_or_default();
        let is_virtual = kind == SymbolKind::Method;
        self.st.push_symbol(Symbol {
            name,
            scope,
            kind,
            line: line_of(name_node),
            is_definition,
            signature,
            // Every JS/TS method can be overridden.
            is_virtual,
        });
    }

    /// Module-level `const x = …`: a function if bound to a function expression, else a variable.
    fn variable(&mut self, node: Node<'a>) {
        let Some(name) = node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
        else {
            return;
        };
        if !at_module_level(node) {
            return;
        }
        let value = node.child_by_field_name("value");
        if let Some(v) = value.filter(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function" | "generator_function"
            )
        }) {
            let qualified = qualify(&self.st.scope(), self.st.text(name));
            self.st.push_function(v, qualified);
            let signature = v
                .child_by_field_name("parameters")
                .map_or_else(|| "(_)".to_owned(), |p| self.signature(p)); // `x => …`
            let scope = self.st.scope();
            self.st.mark_name(name);
            self.st.push_symbol(Symbol {
                name: self.st.text(name).to_owned(),
                scope,
                kind: SymbolKind::Function,
                line: line_of(name),
                is_definition: true,
                signature,
                is_virtual: false,
            });
        } else {
            let scope = self.st.scope();
            self.st.symbol(name, &scope, SymbolKind::Variable);
        }
    }

    /// Parameter types where annotated (`(string, number)`), `_` where not.
    fn signature(&self, params: Node<'_>) -> String {
        let mut cursor = params.walk();
        let parts: Vec<String> = params
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .map(|p| {
                p.child_by_field_name("type").map_or_else(
                    || "_".to_owned(),
                    |t| collapse_ws(self.st.text(t).trim_start_matches(':').trim()),
                )
            })
            .collect();
        format!("({})", parts.join(", "))
    }

    fn call(&mut self, node: Node<'a>) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        // `require("./x")` and `import("./x")` are includes as well as calls.
        if matches!(func.kind(), "identifier" | "import")
            && matches!(self.st.text(func), "require" | "import")
            && let Some(args) = node.child_by_field_name("arguments")
            && let Some(first) = args.named_child(0).filter(|a| a.kind() == "string")
        {
            self.import(first, line_of(node));
        }
        let target = member_name(func);
        if matches!(
            target.kind(),
            "identifier" | "property_identifier" | "private_property_identifier"
        ) {
            let name = self.st.text(target);
            self.st.push_ref(name, RefKind::Call, line_of(target));
            self.st.mark_name(target);
        }
    }
}

/// The called or constructed name: `a.b.c` → `c`, `f` → `f`.
fn member_name(node: Node<'_>) -> Node<'_> {
    match node.kind() {
        "member_expression" => node.child_by_field_name("property").unwrap_or(node),
        _ => node,
    }
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
}

/// Outside any function, method or block body.
fn at_module_level(node: Node<'_>) -> bool {
    let mut cur = node.parent();
    while let Some(p) = cur {
        match p.kind() {
            "statement_block"
            | "class_body"
            | "arrow_function"
            | "function_expression"
            | "function"
            | "method_definition" => {
                // A TS `namespace X { … }` body is a statement_block but still module level.
                if p.kind() == "statement_block"
                    && p.parent()
                        .is_some_and(|g| matches!(g.kind(), "internal_module" | "module"))
                {
                    cur = p.parent();
                    continue;
                }
                return false;
            }
            _ => cur = p.parent(),
        }
    }
    true
}
