//! Rust via tree-sitter-rust.
//!
//! Mapping: `mod` → namespace, `trait` → interface, `impl Trait for Type` → base edge
//! `Type : Trait`, functions inside `impl`/`trait` → methods scoped to the type/trait, `use` →
//! module imports (one per leaf of a use tree), `macro_rules!` → macro, `foo!(…)` → call of `foo`.

use oi_model::{FileFacts, Lang, RefKind, Symbol, SymbolKind};
use tree_sitter::{Node, TreeCursor};

use crate::walk::{self, ParserSlot, State, Visit, collapse_ws, line_of, qualify};
use crate::{ExtractError, Extractor, SyntaxError, extension};

thread_local! {
    static PARSER: ParserSlot = const { ParserSlot::new(None) };
}

/// Extracts definitions, references, `use` imports and trait impls from Rust sources.
#[derive(Default)]
pub struct RustExtractor;

fn parse(source: &[u8]) -> Result<tree_sitter::Tree, ExtractError> {
    walk::parse(&PARSER, &tree_sitter_rust::LANGUAGE.into(), "rust", source)
}

impl Extractor for RustExtractor {
    fn lang(&self) -> Lang {
        Lang::Rust
    }

    fn handles(&self, path: &str) -> bool {
        extension(path).as_deref() == Some("rs")
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

    fn enter(&mut self, node: Node<'a>, cursor: &TreeCursor<'a>) {
        match node.kind() {
            "mod_item" => self.container(node, SymbolKind::Namespace),
            "trait_item" => self.container(node, SymbolKind::Interface),
            "struct_item" => self.container(node, SymbolKind::Struct),
            "union_item" => self.container(node, SymbolKind::Union),
            "enum_item" => self.container(node, SymbolKind::Enum),
            "impl_item" => self.impl_block(node),
            "function_item" => self.function(node, true),
            "function_signature_item" => self.function(node, false),
            "enum_variant" => self.named(node, SymbolKind::Enumerator),
            "field_declaration" => self.named(node, SymbolKind::Field),
            "type_item" => self.named(node, SymbolKind::Typedef),
            "const_item" | "static_item" => self.named(node, SymbolKind::Variable),
            "macro_definition" => self.named(node, SymbolKind::Macro),
            "use_declaration" => {
                if let Some(arg) = node.child_by_field_name("argument") {
                    let mut paths = Vec::new();
                    self.use_paths(arg, &mut paths);
                    for p in paths {
                        self.st.import_module(p, line_of(node));
                    }
                }
            }
            "call_expression" => self.call(node),
            "macro_invocation" => {
                if let Some(m) = node.child_by_field_name("macro") {
                    let target = last_segment(m);
                    let name = self.st.text(target);
                    self.st.push_ref(name, RefKind::Call, line_of(target));
                    self.st.mark_name(target);
                }
            }
            "type_identifier" => self.st.reference(node, RefKind::Type),
            "identifier" | "field_identifier" => {
                // Macro arguments are unparsed token trees; recover `f(…)`, `x.f::<T>(…)` and
                // `m!(…)` there from the token sequence.
                let called = self
                    .st
                    .parent()
                    .is_some_and(|p| matches!(p.kind(), "token_tree" | "token_repetition"))
                    && token_call(&self.st, cursor);
                self.st
                    .reference(node, if called { RefKind::Call } else { RefKind::Use });
            }
            _ => {}
        }
    }
}

impl<'a> Walker<'a> {
    /// A named item with a body that opens a scope (module, trait, struct, enum, union).
    fn container(&mut self, node: Node<'a>, kind: SymbolKind) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let scope = self.st.scope();
        self.st.symbol(name, &scope, kind);
        if let Some(body) = node.child_by_field_name("body") {
            let name = self.st.text(name).to_owned();
            self.st.push_scope(body, name);
        }
    }

    fn named(&mut self, node: Node<'a>, kind: SymbolKind) {
        if let Some(name) = node.child_by_field_name("name") {
            let scope = self.st.scope();
            self.st.symbol(name, &scope, kind);
        }
    }

    /// `impl Type` or `impl Trait for Type`: methods are scoped to `Type`, and a trait impl is
    /// recorded as the base edge `Type : Trait`.
    fn impl_block(&mut self, node: Node<'a>) {
        let Some(ty) = node.child_by_field_name("type") else {
            return;
        };
        let type_name = type_name(&self.st, ty);
        if type_name.is_empty() {
            return;
        }
        let scope = self.st.scope();
        if let Some(tr) = node.child_by_field_name("trait") {
            let written = path_text(&self.st, tr);
            self.st.base(&type_name, &scope, &written, line_of(tr));
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.st.push_scope(body, type_name);
        }
    }

    fn function(&mut self, node: Node<'a>, is_definition: bool) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.st.text(name_node).to_owned();
        let scope = self.st.scope();
        // `node` is being entered: its parent is the body, its grandparent the impl/trait.
        let owner = self.st.ancestor(2).map(|p| p.kind());
        let in_impl_or_trait = matches!(owner, Some("impl_item" | "trait_item"));
        let in_trait = owner == Some("trait_item");
        let in_trait_impl = owner == Some("impl_item")
            && self
                .st
                .ancestor(2)
                .is_some_and(|p| p.child_by_field_name("trait").is_some());
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
            kind: if in_impl_or_trait {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            },
            line: line_of(name_node),
            is_definition,
            signature,
            // Trait methods dispatch dynamically or through the trait; that is Rust's "virtual".
            is_virtual: in_trait || in_trait_impl,
        });
    }

    /// Parameter types, e.g. `(&self, &str, Option<u32>)`.
    fn signature(&self, params: Node<'_>) -> String {
        let mut cursor = params.walk();
        let parts: Vec<String> = params
            .named_children(&mut cursor)
            .filter_map(|p| match p.kind() {
                "self_parameter" => Some(collapse_ws(self.st.text(p))),
                "parameter" => p
                    .child_by_field_name("type")
                    .map(|t| collapse_ws(self.st.text(t))),
                "variadic_parameter" => Some("...".to_owned()),
                _ => None,
            })
            .collect();
        format!("({})", parts.join(", "))
    }

    fn call(&mut self, node: Node<'a>) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if let Some(target) = callee(func) {
            let name = self.st.text(target);
            self.st.push_ref(name, RefKind::Call, line_of(target));
            self.st.mark_name(target);
        }
    }

    /// Expands a use tree into full paths: `a::{b, c::d}` → `a::b`, `a::c::d`. Iterative:
    /// nesting depth comes from the file and must not grow the stack.
    fn use_paths(&self, root: Node<'_>, out: &mut Vec<String>) {
        let join = |p: &str, s: &str| {
            if p.is_empty() {
                s.to_owned()
            } else {
                format!("{p}::{s}")
            }
        };
        let mut stack = vec![(root, String::new())];
        while let Some((node, prefix)) = stack.pop() {
            match node.kind() {
                "scoped_use_list" => {
                    let path = node.child_by_field_name("path").map_or_else(
                        || prefix.clone(),
                        |p| join(&prefix, &path_text(&self.st, p)),
                    );
                    if let Some(list) = node.child_by_field_name("list") {
                        stack.push((list, path));
                    }
                }
                "use_list" => {
                    let mut cursor = node.walk();
                    let children: Vec<_> = node.named_children(&mut cursor).collect();
                    // Reversed so paths come out in source order.
                    stack.extend(children.into_iter().rev().map(|c| (c, prefix.clone())));
                }
                "use_as_clause" => {
                    if let Some(p) = node.child_by_field_name("path") {
                        out.push(use_leaf(&prefix, &path_text(&self.st, p)));
                    }
                }
                "use_wildcard" => {
                    let text = path_text(&self.st, node);
                    out.push(join(&prefix, text.trim_end_matches("::*")));
                }
                _ => out.push(use_leaf(&prefix, &path_text(&self.st, node))),
            }
        }
    }
}

/// `prefix::leaf`, where a `self` leaf (`use std::io::{self, Write}`) names the prefix itself.
fn use_leaf(prefix: &str, leaf: &str) -> String {
    match (prefix.is_empty(), leaf) {
        (false, "self") => prefix.to_owned(),
        (true, _) => leaf.to_owned(),
        (false, _) => format!("{prefix}::{leaf}"),
    }
}

/// The type an `impl` is for, without generics or references: `Foo<T>` → `Foo`, `&mut a::B` → `B`.
fn type_name(st: &State<'_>, ty: Node<'_>) -> String {
    let mut node = ty;
    loop {
        match node.kind() {
            "generic_type" | "reference_type" | "pointer_type" => {
                match node.child_by_field_name("type") {
                    Some(inner) => node = inner,
                    None => return String::new(),
                }
            }
            "scoped_type_identifier" => match node.child_by_field_name("name") {
                Some(inner) => node = inner,
                None => return String::new(),
            },
            _ => return collapse_ws(st.text(node)),
        }
    }
}

/// A path as written, generics stripped: `std::fmt::Display`, `Iterator<Item = u8>` → `Iterator`.
fn path_text(st: &State<'_>, node: Node<'_>) -> String {
    let node = if node.kind() == "generic_type" {
        node.child_by_field_name("type").unwrap_or(node)
    } else {
        node
    };
    st.text(node).split_whitespace().collect()
}

/// Inside a token tree: is the identifier being entered followed by `(…)`, `::<…>(…)` or `!`?
fn token_call(st: &State<'_>, cursor: &TreeCursor<'_>) -> bool {
    // `fn f(..)` / `struct W(..)` in a macro body define, `$f(..)` is a metavariable.
    if st
        .prev_sibling()
        .is_some_and(|p| matches!(p.kind(), "fn" | "struct" | "enum" | "union" | "trait" | "$"))
    {
        return false;
    }
    // Attribute arguments (`#[cfg(any(unix))]`) are not calls.
    if st.ancestors().any(|p| {
        matches!(
            p.kind(),
            "attribute_item" | "inner_attribute_item" | "attribute"
        )
    }) {
        return false;
    }
    let mut next = walk::next_siblings(cursor);
    let Some(mut tok) = next.next() else {
        return false;
    };
    if tok.kind() == "!" {
        return true;
    }
    // Skip a turbofish `::<…>`, tracking angle-bracket depth across tokens (bounded, so a huge
    // token tree cannot make this quadratic).
    if tok.kind() == "::" {
        let mut depth = 0usize;
        for (seen, n) in next.by_ref().enumerate() {
            match n.kind() {
                "<" => depth += 1,
                // `::<<T as Tr>::A>` lexes the first two brackets as one token.
                "<<" => depth += 2,
                ">" => depth = depth.saturating_sub(1),
                ">>" => depth = depth.saturating_sub(2),
                _ if seen == 0 => return false, // `a::b(` is a path, handled elsewhere
                _ => {}
            }
            if depth == 0 || seen > 64 {
                break;
            }
        }
        match next.next() {
            Some(n) => tok = n,
            None => return false,
        }
    }
    tok.kind() == "token_tree" && tok.child(0).is_some_and(|open| open.kind() == "(")
}

/// The name node a call expression's `function` calls: `f`, `a::f`, `x.f`, and the same with
/// turbofish generics (`x.f::<T>`).
fn callee(func: Node<'_>) -> Option<Node<'_>> {
    match func.kind() {
        "identifier" => Some(func),
        "field_expression" => func.child_by_field_name("field"),
        "scoped_identifier" => func.child_by_field_name("name"),
        "generic_function" => callee(func.child_by_field_name("function")?),
        _ => None,
    }
}

/// The last name in a possibly scoped path node.
fn last_segment(node: Node<'_>) -> Node<'_> {
    let mut n = node;
    while let Some(name) = n.child_by_field_name("name") {
        n = name;
    }
    n
}
