//! C# via tree-sitter-c-sharp.
//!
//! Mapping: namespaces (block and file-scoped) open scopes; classes, records, structs, interfaces
//! and enums are symbols with base edges from their base list; methods, constructors, local
//! functions, properties and fields are members; `using` directives are module imports.

use oi_model::{FileFacts, Lang, RefKind, Symbol, SymbolKind};
use tree_sitter::Node;

use crate::walk::{self, ParserSlot, State, Visit, collapse_ws, line_of, qualify};
use crate::{ExtractError, Extractor, SyntaxError, extension};

thread_local! {
    static PARSER: ParserSlot = const { ParserSlot::new(None) };
}

/// Extracts definitions, references, `using` imports and base lists from C# sources.
#[derive(Default)]
pub struct CSharpExtractor;

fn parse(source: &[u8]) -> Result<tree_sitter::Tree, ExtractError> {
    walk::parse(
        &PARSER,
        &tree_sitter_c_sharp::LANGUAGE.into(),
        "c-sharp",
        source,
    )
}

impl Extractor for CSharpExtractor {
    fn lang(&self) -> Lang {
        Lang::CSharp
    }

    fn handles(&self, path: &str) -> bool {
        extension(path).as_deref() == Some("cs")
    }

    fn syntax_errors(&self, source: &[u8]) -> Result<Vec<SyntaxError>, ExtractError> {
        Ok(walk::syntax_errors(&parse(source)?, source))
    }

    fn extract(&self, source: &[u8]) -> Result<FileFacts, ExtractError> {
        let tree = parse(source)?;
        let mut walker = Walker {
            st: State::new(source),
        };
        walk::walk(&mut walker, tree.root_node());
        Ok(walker.st.facts)
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
            "using_directive" => self.using(node),
            "namespace_declaration" => self.namespace(node, node.child_by_field_name("body")),
            // A file-scoped namespace covers the rest of the file: close it with the root.
            "file_scoped_namespace_declaration" => {
                let mut root = node;
                while let Some(p) = root.parent() {
                    root = p;
                }
                self.namespace(node, Some(root));
            }
            "class_declaration" | "record_declaration" => self.record(node, SymbolKind::Class),
            "struct_declaration" | "record_struct_declaration" => {
                self.record(node, SymbolKind::Struct);
            }
            "interface_declaration" => self.record(node, SymbolKind::Interface),
            "enum_declaration" => self.record(node, SymbolKind::Enum),
            "enum_member_declaration" => self.named(node, SymbolKind::Enumerator),
            "delegate_declaration" => self.named(node, SymbolKind::Typedef),
            "method_declaration" | "constructor_declaration" | "destructor_declaration" => {
                self.method(node, SymbolKind::Method);
            }
            "local_function_statement" => self.method(node, SymbolKind::Function),
            "property_declaration" | "event_declaration" | "indexer_declaration" => {
                self.named(node, SymbolKind::Property);
            }
            "field_declaration" | "event_field_declaration" => self.fields(node),
            "invocation_expression" => self.call(node),
            "object_creation_expression" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    let target = last_name(ty);
                    let name = self.st.text(target);
                    self.st.push_ref(name, RefKind::Call, line_of(target));
                    self.st.mark_name(target);
                }
            }
            // The C# grammar uses `identifier` for type names too, so all uses are plain uses.
            "identifier" => self.st.reference(node, RefKind::Use),
            _ => {}
        }
    }
}

impl<'a> Walker<'a> {
    fn using(&mut self, node: Node<'_>) {
        // `using X = A.B;` names the alias in `name`; the imported namespace is the other child.
        let alias = node.child_by_field_name("name");
        let mut cursor = node.walk();
        let target = node
            .named_children(&mut cursor)
            .filter(|c| Some(*c) != alias)
            .find(|c| {
                matches!(
                    c.kind(),
                    "qualified_name" | "identifier" | "generic_name" | "alias_qualified_name"
                )
            });
        if let Some(t) = target {
            let spec: String = self.st.text(t).split_whitespace().collect();
            self.st.import_module(spec, line_of(node));
            self.st.mark_name(t);
            let mut sub = t.walk();
            for part in t.named_children(&mut sub) {
                self.st.mark_name(part);
            }
        }
    }

    fn namespace(&mut self, node: Node<'a>, until: Option<Node<'a>>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let text: String = self.st.text(name).split_whitespace().collect();
        let scope = self.st.scope();
        let last = text.rsplit('.').next().unwrap_or(&text).to_owned();
        let outer = text.rsplit_once('.').map(|(o, _)| o.replace('.', "::"));
        self.st.symbol_named(
            name,
            last,
            &qualify_opt(&scope, outer.as_deref()),
            SymbolKind::Namespace,
        );
        if let Some(until) = until {
            self.st.push_scope(until, text.replace('.', "::"));
        }
    }

    fn record(&mut self, node: Node<'a>, kind: SymbolKind) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.st.text(name_node).to_owned();
        let scope = self.st.scope();
        self.st.symbol(name_node, &scope, kind);
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "base_list" {
                let mut inner = child.walk();
                for base in child.named_children(&mut inner) {
                    // `record R(int X) : Base(X)` wraps the type with its arguments.
                    let ty = if base.kind() == "primary_constructor_base_type" {
                        base.named_child(0).unwrap_or(base)
                    } else {
                        base
                    };
                    if ty.kind() == "argument_list" {
                        continue;
                    }
                    let written = type_path(&self.st, ty);
                    self.st.base(&name, &scope, &written, line_of(ty));
                }
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.st.push_scope(body, name);
        }
    }

    fn named(&mut self, node: Node<'a>, kind: SymbolKind) {
        if let Some(name) = node.child_by_field_name("name") {
            let scope = self.st.scope();
            self.st.symbol(name, &scope, kind);
        }
    }

    fn fields(&mut self, node: Node<'a>) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "variable_declarator" {
                if let Some(name) = n.child_by_field_name("name") {
                    let scope = self.st.scope();
                    self.st.symbol(name, &scope, SymbolKind::Field);
                }
                continue;
            }
            let mut cursor = n.walk();
            stack.extend(n.named_children(&mut cursor));
        }
    }

    fn method(&mut self, node: Node<'a>, kind: SymbolKind) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.st.text(name_node).to_owned();
        let scope = self.st.scope();
        let is_definition = node.child_by_field_name("body").is_some();
        let mut cursor = node.walk();
        let modifiers: Vec<&str> = node
            .children(&mut cursor)
            .filter(|c| c.kind() == "modifier")
            .map(|c| self.st.text(c))
            .collect();
        let in_interface = node
            .parent()
            .and_then(|b| b.parent())
            .is_some_and(|p| p.kind() == "interface_declaration");
        let is_virtual = in_interface
            || modifiers
                .iter()
                .any(|m| matches!(*m, "virtual" | "override" | "abstract"));
        let signature = node
            .child_by_field_name("parameters")
            .map(|p| self.signature(p))
            .unwrap_or_default();
        self.st.mark_name(name_node);
        if is_definition {
            self.st.push_function(node, qualify(&scope, &name));
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

    /// Parameter types, e.g. `(string, ref int, params object[])`.
    fn signature(&self, params: Node<'_>) -> String {
        let mut cursor = params.walk();
        let parts: Vec<String> = params
            .named_children(&mut cursor)
            .filter(|p| p.kind() == "parameter")
            .map(|p| {
                let mut c = p.walk();
                let mods: Vec<&str> = p
                    .children(&mut c)
                    .filter(|m| m.kind() == "modifier")
                    .map(|m| self.st.text(m))
                    .collect();
                let ty = p
                    .child_by_field_name("type")
                    .map_or_else(|| "_".to_owned(), |t| collapse_ws(self.st.text(t)));
                if mods.is_empty() {
                    ty
                } else {
                    format!("{} {ty}", mods.join(" "))
                }
            })
            .collect();
        format!("({})", parts.join(", "))
    }

    fn call(&mut self, node: Node<'a>) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = match func.kind() {
            "member_access_expression" => func.child_by_field_name("name").map(last_name),
            "identifier" | "generic_name" => Some(last_name(func)),
            _ => None,
        };
        if let Some(target) = target.filter(|t| t.kind() == "identifier") {
            let name = self.st.text(target);
            self.st.push_ref(name, RefKind::Call, line_of(target));
            self.st.mark_name(target);
        }
    }
}

/// The identifier naming a type or callee: `A.B<T>` → `B`, `Foo<T>` → `Foo`.
fn last_name(node: Node<'_>) -> Node<'_> {
    let mut n = node;
    loop {
        match n.kind() {
            "qualified_name" => match n.child_by_field_name("name") {
                Some(inner) => n = inner,
                None => return n,
            },
            "generic_name" => match n.named_child(0) {
                Some(inner) => n = inner,
                None => return n,
            },
            _ => return n,
        }
    }
}

/// A base type as written, generics stripped, `.` as `::`: `System.IDisposable` →
/// `System::IDisposable`, `List<T>` → `List`.
fn type_path(st: &State<'_>, node: Node<'_>) -> String {
    let text: String = st.text(node).split_whitespace().collect();
    let mut out = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.replace('.', "::")
}

fn qualify_opt(scope: &str, extra: Option<&str>) -> String {
    match extra {
        Some(e) => qualify(scope, e),
        None => scope.to_owned(),
    }
}
