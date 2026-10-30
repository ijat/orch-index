//! Fact model shared by every orch-index crate.
//!
//! Extractors produce [`FileFacts`]; the store persists them; queries return them with their
//! [`Provenance`] so callers know how far to trust each answer.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How a fact was obtained. Ordered from least to most trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// Read from the syntax tree alone; names are not resolved.
    Syntactic,
    /// Resolved with project knowledge (paths, build files), but not by a compiler.
    Resolved,
    /// Produced by a compiler-backed indexer (SCIP, Kythe).
    Compiler,
}

impl Provenance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syntactic => "syntactic",
            Self::Resolved => "resolved",
            Self::Compiler => "compiler",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "syntactic" => Some(Self::Syntactic),
            "resolved" => Some(Self::Resolved),
            "compiler" => Some(Self::Compiler),
            _ => None,
        }
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Source language of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Cpp,
    Rust,
    JavaScript,
    TypeScript,
    CSharp,
}

impl Lang {
    pub const ALL: [Self; 5] = [
        Self::Cpp,
        Self::Rust,
        Self::JavaScript,
        Self::TypeScript,
        Self::CSharp,
    ];

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_str() == s)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpp => "cpp",
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::CSharp => "csharp",
        }
    }
}

/// What a symbol definition or declaration is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Namespace,
    Class,
    /// Interface, protocol or Rust trait.
    Interface,
    Struct,
    Union,
    Enum,
    Enumerator,
    Function,
    Method,
    Field,
    /// C# / TypeScript property or accessor.
    Property,
    Typedef,
    Macro,
    Variable,
}

impl SymbolKind {
    pub const ALL: [Self; 14] = [
        Self::Namespace,
        Self::Class,
        Self::Interface,
        Self::Struct,
        Self::Union,
        Self::Enum,
        Self::Enumerator,
        Self::Function,
        Self::Method,
        Self::Field,
        Self::Property,
        Self::Typedef,
        Self::Macro,
        Self::Variable,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Namespace => "namespace",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Struct => "struct",
            Self::Union => "union",
            Self::Enum => "enum",
            Self::Enumerator => "enumerator",
            Self::Function => "function",
            Self::Method => "method",
            Self::Field => "field",
            Self::Property => "property",
            Self::Typedef => "typedef",
            Self::Macro => "macro",
            Self::Variable => "variable",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether this kind names a type that other classes can derive from.
    #[must_use]
    pub const fn is_record(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Struct | Self::Union | Self::Enum
        )
    }
}

/// A definition or declaration of a named entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// Unqualified name, e.g. `OnStartRequest`.
    pub name: String,
    /// Enclosing scope as written or inferred, e.g. `mozilla::net::nsHttpChannel`. Empty at file scope.
    pub scope: String,
    pub kind: SymbolKind,
    /// 1-based line of the name.
    pub line: u32,
    /// True for a body-carrying definition, false for a declaration only.
    pub is_definition: bool,
    /// Normalized parameter-type list for functions, e.g. `(nsIRequest*)`. Empty otherwise.
    pub signature: String,
    /// Method is marked `virtual`, `override` or `final`, or uses a virtual-declaring macro.
    pub is_virtual: bool,
}

/// How a name is used at a reference site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    /// Called as a function or method.
    Call,
    /// Used as a type.
    Type,
    /// Any other identifier use.
    Use,
}

impl RefKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Type => "type",
            Self::Use => "use",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "call" => Some(Self::Call),
            "type" => Some(Self::Type),
            "use" => Some(Self::Use),
            _ => None,
        }
    }
}

/// A use of a name. References are syntactic: the target is the name, not a resolved symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub kind: RefKind,
    pub line: u32,
    /// Qualified name of the enclosing function definition, e.g. `ns::Class::Method`.
    pub container: Option<String>,
}

/// An `#include` or import directive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Include {
    /// The path or module as written, without quotes or angle brackets.
    pub spec: String,
    /// `#include <...>` rather than `#include "..."`.
    pub system: bool,
    /// The spec names a module or namespace (`crate::a::B`, `System.Linq`, `react`), not a file.
    pub module: bool,
    pub line: u32,
}

impl Include {
    /// Lookup key: the file stem for file specs (`a/b/c.h` → `c`), the last segment for modules
    /// (`crate::a::B` → `B`, `System.Linq` → `Linq`, `@scope/pkg` → `pkg`).
    #[must_use]
    pub fn key(&self) -> &str {
        key_of(&self.spec, self.module)
    }
}

/// See [`Include::key`]. Also used for the target side of a lookup.
#[must_use]
pub fn key_of(spec: &str, module: bool) -> &str {
    if module {
        spec.rsplit(['/', '.', ':'])
            .find(|s| !s.is_empty())
            .unwrap_or(spec)
    } else if spec.bytes().all(|b| b == b'.' || b == b'/') {
        // `import x from ".."` names a directory, i.e. its index file.
        "index"
    } else {
        let file = spec.rsplit('/').next().unwrap_or(spec);
        match file.split_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem,
            _ => file,
        }
    }
}

/// A base-class edge: `class derived : base`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseClass {
    /// Name of the derived class (unqualified).
    pub derived: String,
    /// Scope the derived class is defined in, e.g. `mozilla::net`.
    pub derived_scope: String,
    /// Base, last path segment only (template arguments stripped).
    pub base: String,
    /// Base as written, qualifiers kept, template arguments stripped, e.g. `URLRequest::Delegate`.
    pub base_written: String,
    pub line: u32,
}

/// Everything extracted from one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFacts {
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    pub includes: Vec<Include>,
    pub bases: Vec<BaseClass>,
    /// Syntax-error nodes in the parse.
    pub parse_errors: u32,
    /// Errors at file, namespace or class level (not inside a function body). These can
    /// mis-scope or hide later definitions, so facts from such files deserve a check.
    pub structural_errors: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip() {
        for k in SymbolKind::ALL {
            assert_eq!(SymbolKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn include_keys() {
        assert_eq!(key_of("a/b/c.h", false), "c");
        assert_eq!(key_of("./widgets", false), "widgets");
        assert_eq!(key_of("..", false), "index");
        assert_eq!(key_of("./", false), "index");
        assert_eq!(key_of("crate::a::B", true), "B");
        assert_eq!(key_of("System.Linq", true), "Linq");
        assert_eq!(key_of("@scope/pkg", true), "pkg");
    }

    #[test]
    fn provenance_orders_by_trust() {
        assert!(Provenance::Syntactic < Provenance::Resolved);
        assert!(Provenance::Resolved < Provenance::Compiler);
    }
}
