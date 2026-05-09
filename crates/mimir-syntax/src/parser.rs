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

use std::borrow::Cow;

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

/// Replace whole-line backtick tokens with spaces of the same byte length.
///
/// tree-sitter-verilog has two categories of backtick lines that collapse the
/// parse tree into a single ERROR root, killing every structural LSP feature:
///
/// 1. **Compiler guard directives** (`\`ifdef`, `\`ifndef`, `\`endif`,
///    `\`include`, `\`timescale`, …) — tree-sitter doesn't model conditional
///    compilation as a top-level construct, so a guarded file wraps entirely in
///    an ERROR node.
///
/// 2. **Standalone macro calls** (`\`uvm_fatal`, `\`uvm_info`,
///    `\`uvm_component_utils_begin`, …) — when one of these appears inside an
///    `if-begin-end` block inside a class method the grammar completely loses
///    its parse context and produces an ERROR root.
///
/// **Exception**: `\`define` lines are left intact. Macro definitions carry
/// symbol names (`MY_MACRO`, `ANOTHER_MACRO`, …) that the symbol indexer reads
/// from `SyntaxTree::source`; blanking them would make those symbols invisible.
///
/// Because we only replace characters *within* a line — keeping every newline
/// in place and maintaining the exact byte count — every byte offset and line
/// number that tree-sitter emits maps verbatim back to the original source.
///
/// Inline backtick references (`x = \`MY_MACRO + y;`) are NOT blanked: the
/// first non-whitespace character of those lines is not a backtick.
///
/// Returns `Cow::Borrowed(source)` when the source contains no backtick
/// characters at all (fast path).
fn blank_backtick_lines(source: &str) -> Cow<'_, str> {
    if !source.contains('`') {
        return Cow::Borrowed(source);
    }

    let mut out = String::with_capacity(source.len());
    let mut rest = source;

    while !rest.is_empty() {
        let (line, nl, tail) = match rest.find('\n') {
            Some(p) => (&rest[..p], "\n", &rest[p + 1..]),
            None => (rest, "", ""),
        };

        let trimmed = line.trim_start();
        // Blank every backtick line EXCEPT `define — macro definitions carry
        // symbol names that the symbol indexer reads from SyntaxTree::source.
        let should_blank = trimmed.starts_with('`') && {
            let after = &trimmed[1..];
            let kw = after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            kw != "define"
        };
        if should_blank {
            // Spaces of identical byte length so all byte offsets are preserved.
            for _ in 0..line.len() {
                out.push(' ');
            }
        } else {
            out.push_str(line);
        }
        out.push_str(nl);
        rest = tail;
    }

    Cow::Owned(out)
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
    ///
    /// Before handing the text to tree-sitter, whole-line backtick tokens are
    /// replaced with spaces of the same byte length (see
    /// `blank_backtick_lines`). This prevents compiler directives and
    /// standalone macro calls from collapsing the file into a single ERROR
    /// root node. `\`define` lines are left intact so the symbol indexer can
    /// still find macro names. All byte offsets and line numbers are preserved.
    #[instrument(level = "debug", skip(self, source, previous), fields(bytes = source.len()))]
    pub fn parse(
        &mut self,
        source: &str,
        previous: Option<&Tree>,
    ) -> Result<SyntaxTree, SyntaxParserError> {
        let preprocessed = blank_backtick_lines(source);
        let src = preprocessed.as_ref();

        let tree = self
            .parser
            .parse(src, previous)
            .ok_or(SyntaxParserError::NoTree)?;

        debug!(
            has_error = tree.root_node().has_error(),
            node_count = tree.root_node().descendant_count(),
            "parse complete",
        );
        trace!(s_expr = %tree.root_node().to_sexp(), "tree dump");

        Ok(SyntaxTree {
            tree,
            source: preprocessed.into_owned(),
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

    /// Guard directives (`ifdef/`ifndef/`endif) and standalone UVM macro calls
    /// on their own lines are blanked so the parser can see clean SV structure.
    /// `define lines are left intact so symbol names remain accessible.
    #[test]
    fn backtick_lines_blanked_except_define() {
        init_for_tests();
        // File with: an include guard, a flag-define, a UVM macro call, and a class.
        // After preprocessing, the ifndef and uvm_fatal lines become spaces;
        // the `define line stays. The class must be parseable.
        let src = "\
`ifndef GUARD_SV
`define GUARD_SV
class C;
  function void f();
    `uvm_fatal(\"TAG\", \"msg\")
  endfunction
endclass
`endif
";
        let mut parser = SyntaxParser::new().expect("grammar must load");
        let tree = parser.parse(src, None).expect("parse");
        // Root must be source_file, not ERROR.
        assert_eq!(
            tree.tree.root_node().kind(),
            "source_file",
            "expected source_file root, got {}",
            tree.tree.root_node().kind()
        );
        // The `define line is kept verbatim so its byte range is readable.
        let define_line = tree.source.lines().nth(1).unwrap_or("");
        assert!(
            define_line.starts_with('`'),
            "`define line should be preserved in source: {:?}",
            define_line
        );
    }
}
