//! C and C++ (and Objective-C++ on a best-effort basis) via tree-sitter-cpp.

use oi_model::{FileFacts, Lang, RefKind, Symbol, SymbolKind};
use tree_sitter::{Node, TreeCursor};

use crate::walk::{self, ParserSlot, State, Visit, collapse_ws, join_scope, line_of, qualify};
use crate::{ExtractError, Extractor, SyntaxError, extension};

mod macros;

const EXTENSIONS: &[&str] = &[
    "c", "h", "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "inl", "ipp", "tcc", "mm",
];

/// Return-type macros that declare a virtual method (`NS_IMETHOD` expands to `virtual nsresult`).
const DEFAULT_VIRTUAL_MACROS: &[&str] = &["NS_IMETHOD", "NS_IMETHOD_", "STDMETHOD", "STDMETHOD_"];

thread_local! {
    static PARSER: ParserSlot = const { ParserSlot::new(None) };
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

fn parse(source: &[u8]) -> Result<tree_sitter::Tree, ExtractError> {
    walk::parse(&PARSER, &tree_sitter_cpp::LANGUAGE.into(), "cpp", source)
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
        Ok(walk::syntax_errors(&parse(source)?, source))
    }

    fn extract(&self, source: &[u8]) -> Result<FileFacts, ExtractError> {
        let (rewritten, blanked) = macros::neutralize(source);
        let source = rewritten.as_deref().unwrap_or(source);
        let tree = parse(source)?;
        let mut walker = Walker {
            st: State::new(source),
            virtual_macros: &self.virtual_macros,
        };
        walk::walk(&mut walker, tree.root_node());
        for b in blanked {
            walker.st.push_ref(&b.name, RefKind::Use, b.line);
        }
        Ok(walker.st.facts)
    }
}

struct Walker<'a> {
    st: State<'a>,
    virtual_macros: &'a [String],
}

impl<'a> Visit<'a> for Walker<'a> {
    fn state(&mut self) -> &mut State<'a> {
        &mut self.st
    }

    fn enter(&mut self, node: Node<'a>, cursor: &TreeCursor<'a>) {
        match node.kind() {
            "preproc_include" => self.include(node),
            "preproc_def" | "preproc_function_def" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.st.symbol(name, "", SymbolKind::Macro);
                }
            }
            "namespace_definition" => self.namespace(node),
            "class_specifier" | "struct_specifier" | "union_specifier" => self.record(node),
            "enum_specifier" => {
                if let Some(name) = node.child_by_field_name("name")
                    && let Some(body) = node.child_by_field_name("body")
                {
                    let scope = self.st.scope();
                    self.st.symbol(name, &scope, SymbolKind::Enum);
                    // `enum class E { A }` scopes A as E::A; a plain enum's enumerators live in
                    // the enclosing scope.
                    let mut cursor = node.walk();
                    let scoped = node
                        .children(&mut cursor)
                        .any(|c| matches!(c.kind(), "class" | "struct"));
                    if scoped {
                        let name = self.st.text(name).to_owned();
                        self.st.push_scope(body, name);
                    }
                }
            }
            "enumerator" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let scope = self.st.scope();
                    self.st.symbol(name, &scope, SymbolKind::Enumerator);
                }
            }
            "function_definition" => self.function(node, true),
            "field_declaration" | "declaration" => self.declaration(node),
            "type_definition" => self.typedef(node),
            "alias_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let scope = self.st.scope();
                    self.st.symbol(name, &scope, SymbolKind::Typedef);
                }
            }
            "call_expression" => self.call(node),
            "type_identifier" => self.st.reference(node, RefKind::Type),
            "identifier" | "field_identifier" | "namespace_identifier" => {
                // Inside error recovery (e.g. Objective-C in a .mm file) no call_expression is
                // built, but `name(` is still a call.
                let called = self.st.parent().is_some_and(|p| p.is_error())
                    && walk::next_siblings(cursor)
                        .next()
                        .is_some_and(|s| matches!(s.kind(), "(" | "argument_list"));
                self.st
                    .reference(node, if called { RefKind::Call } else { RefKind::Use });
            }
            _ => {}
        }
    }
}

impl<'a> Walker<'a> {
    fn include(&mut self, node: Node<'_>) {
        let Some(path) = node.child_by_field_name("path") else {
            return;
        };
        let system = path.kind() == "system_lib_string";
        let spec = self
            .st
            .text(path)
            .trim_matches(|c| c == '"' || c == '<' || c == '>')
            .to_owned();
        self.st.include(spec, system, line_of(node));
    }

    fn namespace(&mut self, node: Node<'a>) {
        // Anonymous namespaces open a scope too, so their members are not mistaken for globals.
        let name = node
            .child_by_field_name("name")
            .map_or_else(|| "(anonymous)".to_owned(), |n| self.st.text(n).to_owned());
        if let Some(n) = node.child_by_field_name("name") {
            let scope = self.st.scope();
            self.st
                .symbol_named(n, name.clone(), &scope, SymbolKind::Namespace);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.st.push_scope(body, name);
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
        let scope = join_scope(&self.st.scope(), &qualifiers);
        // `class Foo;` (no body) is a forward declaration; `struct Foo x;` is a type use.
        if body.is_some() {
            self.st.symbol_named(name_node, name.clone(), &scope, kind);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "base_class_clause" {
                self.bases(child, &name, &scope);
            }
        }
        if let Some(body) = body {
            self.st
                .push_scope(body, join_scope(&qualifiers.join("::"), &[name]));
        }
    }

    fn bases(&mut self, clause: Node<'_>, derived: &str, derived_scope: &str) {
        let mut cursor = clause.walk();
        for child in clause.named_children(&mut cursor) {
            if matches!(
                child.kind(),
                "type_identifier" | "qualified_identifier" | "template_type"
            ) {
                let (qualifiers, base) = self.split_name(child, false);
                let written = join_scope(&qualifiers.join("::"), &[base]);
                self.st
                    .base(derived, derived_scope, &written, line_of(child));
            }
        }
    }

    fn typedef(&mut self, node: Node<'a>) {
        let mut cursor = node.walk();
        let declarators: Vec<_> = node
            .children_by_field_name("declarator", &mut cursor)
            .collect();
        for decl in declarators {
            if let Some(id) = innermost_name(decl) {
                let scope = self.st.scope();
                self.st.symbol(id, &scope, SymbolKind::Typedef);
            }
        }
    }

    /// `declaration` (namespace/block scope) or `field_declaration` (class body).
    fn declaration(&mut self, node: Node<'a>) {
        let in_class = node.kind() == "field_declaration";
        // Block-scope declarations are locals; `Foo bar(baz);` there is a constructor call.
        if !in_class && !self.at_namespace_scope() {
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
                let scope = self.st.scope();
                self.st.symbol(id, &scope, member_kind);
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
        // `owner` is the node being entered, so the walker's parent is its parent.
        // Constructors, destructors and member templates arrive as `declaration` or
        // `function_definition`, possibly wrapped in `template_declaration`: what makes them
        // members is that the nearest non-template ancestor is a class body.
        let in_class = owner.kind() == "field_declaration"
            || self
                .st
                .ancestors()
                .find(|p| p.kind() != "template_declaration")
                .is_some_and(|p| p.kind() == "field_declaration_list");
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
        let scope = join_scope(&self.st.scope(), &qualifiers);
        if is_definition {
            self.st.push_function(owner, qualify(&scope, &name));
        }
        self.st.push_symbol(Symbol {
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
            let ty = self.st.text(t);
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
                out.push_str(self.st.text(child));
                out.push(' ');
            }
        }
        if let Some(ty) = param.child_by_field_name("type") {
            out.push_str(&collapse_ws(self.st.text(ty)));
        }
        let mut decl = param.child_by_field_name("declarator");
        while let Some(d) = decl {
            match d.kind() {
                "pointer_declarator" | "abstract_pointer_declarator" => out.push('*'),
                "reference_declarator" | "abstract_reference_declarator" => {
                    out.push_str(if self.st.text(d).starts_with("&&") {
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
            let name = self.st.text(target);
            self.st.push_ref(name, RefKind::Call, line_of(target));
            // The identifier itself is visited next; don't count it again as a plain use.
            self.st.mark_name(target);
        }
    }

    /// Locals are not symbols: a declaration (the node being entered) counts only outside any
    /// function body.
    fn at_namespace_scope(&self) -> bool {
        !self
            .st
            .ancestors()
            .any(|p| matches!(p.kind(), "compound_statement" | "lambda_expression"))
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
                                .map_or("", |s| self.st.text(s)),
                            _ => self.st.text(scope),
                        };
                        if !scope_name.is_empty() {
                            qualifiers.push(scope_name.to_owned());
                        }
                        if defining {
                            self.st.mark_name(scope);
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
                        self.st.mark_name(n);
                    }
                    return (qualifiers, collapse_ws(self.st.text(n)));
                }
            }
        }
    }
}

/// Unwraps pointer/reference/parenthesized declarators to reach a `function_declarator`.
fn find_function_declarator(decl: Node<'_>) -> Option<Node<'_>> {
    let mut cur = Some(decl);
    while let Some(d) = cur {
        match d.kind() {
            "function_declarator" => return Some(d),
            "pointer_declarator" | "reference_declarator" | "init_declarator" => {
                cur = d
                    .child_by_field_name("declarator")
                    .or_else(|| d.named_child(0));
            }
            // Includes `parenthesized_declarator`: a pointer to function (`void (*f)()`) is a
            // variable, not a function.
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
