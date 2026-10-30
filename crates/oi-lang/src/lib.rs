//! Language extractors: turn one file's bytes into [`FileFacts`].
//!
//! Each language implements [`Extractor`]; [`Registry`] maps paths to extractors. Extractors are
//! stateless and shared across threads; per-thread parser state lives inside the implementation.

mod cpp;
mod csharp;
mod rust;
mod script;
mod walk;

use oi_model::{FileFacts, Lang};

pub use cpp::CppExtractor;
pub use csharp::CSharpExtractor;
pub use rust::RustExtractor;
pub use script::ScriptExtractor;

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("tree-sitter rejected the {0} grammar")]
    Grammar(&'static str),
    #[error("parser produced no tree")]
    NoTree,
}

/// Extracts syntactic facts from a source file of one language.
pub trait Extractor: Send + Sync {
    fn lang(&self) -> Lang;

    /// Whether this extractor handles `path`, judged by its name alone.
    fn handles(&self, path: &str) -> bool;

    fn extract(&self, source: &[u8]) -> Result<FileFacts, ExtractError>;

    /// Like [`Extractor::extract`], for extractors whose dialect depends on the file name
    /// (`.ts` vs `.tsx`). The indexer always calls this one.
    fn extract_path(&self, _path: &str, source: &[u8]) -> Result<FileFacts, ExtractError> {
        self.extract(source)
    }

    /// Diagnostics: where the parser hit syntax errors. Defaults to none.
    fn syntax_errors(&self, _source: &[u8]) -> Result<Vec<SyntaxError>, ExtractError> {
        Ok(Vec::new())
    }

    /// Path-aware [`Extractor::syntax_errors`].
    fn syntax_errors_path(
        &self,
        _path: &str,
        source: &[u8],
    ) -> Result<Vec<SyntaxError>, ExtractError> {
        self.syntax_errors(source)
    }
}

/// A syntax-error site, for diagnosing extractor quality.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyntaxError {
    pub line: u32,
    /// Kind of the nearest non-error ancestor, e.g. `field_declaration_list`.
    pub context: String,
    /// The start of the erroneous text.
    pub text: String,
}

/// The set of extractors in use, consulted in order.
pub struct Registry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl Registry {
    /// All built-in extractors.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            extractors: vec![
                Box::new(CppExtractor::default()),
                Box::new(RustExtractor),
                Box::new(ScriptExtractor::javascript()),
                Box::new(ScriptExtractor::typescript()),
                Box::new(CSharpExtractor),
            ],
        }
    }

    #[must_use]
    pub fn for_path(&self, path: &str) -> Option<&dyn Extractor> {
        self.extractors
            .iter()
            .find(|e| e.handles(path))
            .map(AsRef::as_ref)
    }
}

/// Lowercased extension of `path`, if any.
pub(crate) fn extension(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let (_, ext) = file.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}
