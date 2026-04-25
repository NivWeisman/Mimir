//! Thin wrapper around `tree_sitter::Parser` for SystemVerilog.
//!
//! ## Lifecycle
//!
//! ```text
//! SyntaxParser::new()  ──►  parser owns a tree_sitter::Parser
//!                            with the SV language loaded
//!
//! parser.parse(text, prev=None)        ──► full parse, returns SyntaxTree
//! parser.parse(text, prev=Some(tree))  ──► incremental reparse
//! ```
//!
//! `tree-sitter` is **not** thread-safe per-parser — a `Parser` instance
//! cannot be shared across threads. The LSP server keeps one `SyntaxParser`
//! per worker task and serializes parse calls per document.

use thiserror::Error;
use tracing::{debug, instrument, trace};
use tree_sitter::{Parser, Tree};

/// Errors from constructing or driving the parser.
#[derive(Debug, Error)]
pub enum SyntaxParserError {
    /// Failed to load the SystemVerilog grammar into the underlying
    /// `tree_sitter::Parser`. This means the bundled grammar is
    /// ABI-incompatible with the `tree-sitter` runtime crate version we use
    /// — it's a build-config bug, not a user-input problem.
    #[error("failed to set tree-sitter language: {0}")]
    SetLanguage(#[from] tree_sitter::LanguageError),

    /// `Parser::parse` returned `None`. The tree-sitter docs say this only
    /// happens if a timeout/cancellation flag is set; we don't set one, so
    /// in practice this is unreachable. We model it as an error anyway so
    /// we never silently swallow it.
    #[error("tree-sitter returned no parse tree (timeout or cancellation)")]
    NoTree,
}

/// A parsed syntax tree plus the source text it was parsed from.
///
/// We keep the source alongside because tree-sitter `Node`s store byte
/// offsets and you usually want to slice the source to extract the
/// underlying text.
#[derive(Debug)]
pub struct SyntaxTree {
    /// The tree-sitter parse tree. Cheap to clone (it's an `Arc` internally).
    pub tree: Tree,
    /// The exact source we parsed. Stored as `String` so we can hand out
    /// `&str` slices for any byte range a caller asks about.
    pub source: String,
}

impl SyntaxTree {
    /// Returns true if any node in the tree is an `ERROR` node or a
    /// `MISSING` node. Cheap — tree-sitter tracks this on the root.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.tree.root_node().has_error()
    }

    /// Borrow the source text. Used by diagnostic collection to render the
    /// problematic span.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Owns a `tree_sitter::Parser` configured for SystemVerilog.
pub struct SyntaxParser {
    parser: Parser,
}

impl SyntaxParser {
    /// Construct a new parser with the SV grammar loaded.
    ///
    /// Returns an error only if the grammar's ABI doesn't match the
    /// `tree-sitter` runtime — a build issue, not a runtime one.
    pub fn new() -> Result<Self, SyntaxParserError> {
        let mut parser = Parser::new();
        // `tree_sitter_verilog::LANGUAGE` is a `LanguageFn` (the newer
        // tree-sitter 0.22+ API). Convert it into the heavier `Language`
        // handle that `set_language` consumes. The same grammar covers
        // both Verilog and SystemVerilog.
        let language: tree_sitter::Language = tree_sitter_verilog::LANGUAGE.into();
        parser.set_language(&language)?;
        Ok(Self { parser })
    }

    /// Parse `source`, optionally seeding with a previous tree for
    /// incremental reparse.
    ///
    /// Pass `previous = Some(&old_tree.tree)` after applying `tree.edit(..)`
    /// for each text change you've made — tree-sitter will reuse subtrees
    /// that didn't change.
    ///
    /// If `previous` is `None` you get a full parse.
    #[instrument(level = "debug", skip(self, source, previous), fields(bytes = source.len()))]
    pub fn parse(
        &mut self,
        source: &str,
        previous: Option<&Tree>,
    ) -> Result<SyntaxTree, SyntaxParserError> {
        let tree = self
            .parser
            .parse(source, previous)
            .ok_or(SyntaxParserError::NoTree)?;

        debug!(
            has_error = tree.root_node().has_error(),
            node_count = tree.root_node().descendant_count(),
            "parse complete",
        );
        trace!(s_expr = %tree.root_node().to_sexp(), "tree dump");

        Ok(SyntaxTree {
            tree,
            source: source.to_owned(),
        })
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::logging::init_for_tests;

    /// A minimal valid module should parse with no errors.
    #[test]
    fn parses_minimal_module() {
        init_for_tests();
        let mut parser = SyntaxParser::new().expect("grammar must load");
        let tree = parser
            .parse("module foo;\nendmodule\n", None)
            .expect("parse must succeed");
        assert!(!tree.has_errors(), "valid module had errors:\n{}", tree.tree.root_node().to_sexp());
    }

    /// A clearly broken module produces `has_errors() == true`. (We assert
    /// only the boolean here; specific node types are checked in the
    /// `diagnostics` tests, where we actually consume them.)
    #[test]
    fn flags_syntax_error() {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        // Missing `;` after the module header.
        let tree = parser
            .parse("module foo\nendmodule\n", None)
            .unwrap();
        assert!(tree.has_errors());
    }

    /// Re-parsing the same source with a prior tree should still produce a
    /// correct tree. We don't assert "it was faster" — that's a perf test —
    /// but we do verify it returns the same root structure.
    #[test]
    fn incremental_reparse_returns_equivalent_tree() {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let src = "module foo;\nendmodule\n";
        let first = parser.parse(src, None).unwrap();
        let second = parser.parse(src, Some(&first.tree)).unwrap();
        assert_eq!(
            first.tree.root_node().to_sexp(),
            second.tree.root_node().to_sexp(),
        );
    }
}
