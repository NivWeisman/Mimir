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
///
/// Cloning is cheap: `tree_sitter::Tree` is `Arc`-internal, and the source
/// `String` is the only owned-heap allocation copied. Callers cache a
/// `SyntaxTree` and hand out clones per LSP request.
#[derive(Debug, Clone)]
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
/// Two categories of backtick lines collapse the parse tree into a single
/// ERROR root, killing every structural LSP feature:
///
/// 1. **Compiler guard directives** (`\`ifdef`, `\`ifndef`, `\`endif`,
///    `\`include`, `\`timescale`, …) — the grammar doesn't model conditional
///    compilation as a top-level construct, so a guarded file wraps entirely
///    in an ERROR node.
///
/// 2. **Standalone macro calls** (`\`uvm_fatal`, `\`uvm_info`,
///    `\`uvm_component_utils_begin`, …) — when one of these appears inside an
///    `if-begin-end` block inside a class method the grammar loses its parse
///    context and produces an ERROR root.
///
/// **Allowlist of directives to blank** (IEEE 1800-2023 §22) — compiler
/// directives that aren't real SV syntax and that the new
/// `tree-sitter-systemverilog 0.3.1` grammar still doesn't model. `\`define`
/// is deliberately **excluded**: macro definitions carry symbol names that
/// the symbol indexer reads from `SyntaxTree::source`, so blanking them
/// would make those symbols invisible.
///
/// User-defined macro **invocations** (`` `uvm_fatal(...) ``,
/// `` `uvm_object_utils(...) ``, …) are not directives and are not blanked —
/// the new grammar handles them as `text_macro_usage` nodes inside the
/// surrounding statement context, which is what AST-backed inlay hints and
/// goto-def need to see.
///
/// Because we only replace characters *within* a line — keeping every newline
/// in place and maintaining the exact byte count — every byte offset and line
/// number that tree-sitter emits maps verbatim back to the original source.
///
/// Inline backtick references on a line that does not *start* with a
/// backtick (`x = \`MY_MACRO + y;`) are NOT considered here: the first
/// non-whitespace character of those lines is a regular SV token.
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
        let should_blank = trimmed.starts_with('`') && {
            let after = &trimmed[1..];
            let kw = after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            is_compiler_directive(kw)
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

/// Compiler directives that don't represent SV syntax the parser knows about.
/// Listed against IEEE 1800-2023 §22; `define` is intentionally NOT here —
/// we keep `\`define` lines intact so the symbol indexer can read macro names.
fn is_compiler_directive(kw: &str) -> bool {
    matches!(
        kw,
        "ifdef"
            | "ifndef"
            | "elsif"
            | "else"
            | "endif"
            | "include"
            | "timescale"
            | "default_nettype"
            | "celldefine"
            | "endcelldefine"
            | "resetall"
            | "line"
            | "undef"
            | "undefineall"
            | "pragma"
            | "begin_keywords"
            | "end_keywords"
            | "unconnected_drive"
            | "nounconnected_drive"
            | "__FILE__"
            | "__LINE__"
    )
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
        // `tree_sitter_systemverilog::LANGUAGE` is a `LanguageFn` (tree-sitter
        // 0.22+ API). Convert it into the `Language` handle `set_language` needs.
        let language: tree_sitter::Language = tree_sitter_systemverilog::LANGUAGE.into();
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
    /// One byte-preserving rewrite runs before tree-sitter sees the text:
    ///
    /// - `blank_backtick_lines` — whole-line backtick tokens (`` `ifdef ``,
    ///   `` `uvm_fatal `` etc.) are replaced with spaces so compiler
    ///   directives and standalone macro calls don't collapse the file into
    ///   a single ERROR root. `` `define `` lines are left intact so the
    ///   symbol indexer can still find macro names.
    ///
    /// The pass preserves every byte offset and line number, so anything
    /// tree-sitter emits maps verbatim back to the original source.
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

    /// Compiler directives (`` `ifdef ``, `` `ifndef ``, `` `endif ``,
    /// `` `include ``, etc.) are blanked so the parser sees clean SV
    /// structure. `` `define `` lines and user-macro call lines
    /// (`` `uvm_fatal(...) ``) are **kept** intact under the
    /// tree-sitter-systemverilog 0.3.1 allowlist policy — both carry real
    /// information the AST-backed features need.
    #[test]
    fn compiler_directives_blanked_but_define_and_macro_calls_kept() {
        init_for_tests();
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
        let lines: Vec<&str> = tree.source.lines().collect();
        // Compiler directive lines are blanked.
        assert!(
            lines[0].trim().is_empty(),
            "`ifndef line should be blanked: {:?}",
            lines[0]
        );
        assert!(
            lines[7].trim().is_empty(),
            "`endif line should be blanked: {:?}",
            lines[7]
        );
        // `define` line and the user-macro call line are preserved.
        assert!(
            lines[1].starts_with('`'),
            "`define line should be preserved: {:?}",
            lines[1]
        );
        assert!(
            lines[4].contains("uvm_fatal"),
            "user-macro call line should be preserved: {:?}",
            lines[4]
        );
    }

    /// Regression lock-in: under tree-sitter-systemverilog 0.3.1 the
    /// parameterized scope call `IDENT#(T)::method(args)` parses with the
    /// `::` and the scope operand *fused* into the call's function name
    /// expression — there is no longer a separate `package_scope` /
    /// `class_scope` / `::` child the way the old `tree-sitter-verilog`
    /// grammar (and the now-removed `blank_parameterized_scopes` rewrite)
    /// exposed. That fusing is the price we pay for the body parsing
    /// cleanly, and tree-sitter-only scope resolution for these calls is
    /// the capability we lost.
    ///
    /// This test pins the new shape so a future grammar bump can't quietly
    /// reintroduce a separate `::` node without us noticing and updating
    /// the slang-fallback paths.
    #[test]
    fn fused_parameterized_scope_call_has_no_separate_double_colon_node() {
        init_for_tests();
        let src = "\
class apb_monitor;
   virtual function void build_phase(int phase);
      int tmp;
      if (!uvm_config_db#(apb_vif)::get(this, \"\", \"vif\", tmp)) begin
      end
   endfunction
endclass
";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).expect("parse");
        let sexp = tree.tree.root_node().to_sexp();
        assert!(
            !sexp.contains("(package_scope") && !sexp.contains("(class_scope"),
            "expected fused parameterized scope, got a separate scope node:\n{}",
            sexp,
        );
    }

    /// A `` `uvm_fatal(...) `` statement that occupies a whole line (the
    /// apb_monitor style) survives [`blank_backtick_lines`] under the
    /// 0.3.1 allowlist policy and parses as a `text_macro_usage` node in
    /// the AST. This is the hook the §3 macro-arg inlay hints and the
    /// syntax goto-def macro-callsite path key off.
    #[test]
    fn whole_line_macro_call_yields_text_macro_usage_in_ast() {
        init_for_tests();
        let src = "\
class C;
  function void f();
    `uvm_fatal(\"TAG\", \"msg\")
  endfunction
endclass
";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).expect("parse");
        let macro_line = tree.source.lines().nth(2).unwrap_or("");
        assert!(
            macro_line.contains("uvm_fatal"),
            "macro callsite line should be preserved, got: {:?}",
            macro_line,
        );
        let sexp = tree.tree.root_node().to_sexp();
        assert!(
            sexp.contains("text_macro_usage"),
            "expected text_macro_usage node in the AST; got:\n{}",
            sexp,
        );
    }

    /// A class whose method body contains a parameterized scope call must
    /// parse with a `source_file` root (not ERROR). The new grammar handles
    /// `IDENT#(T)::method(args)` natively without any preprocessor rewrite.
    #[test]
    fn class_with_parameterized_scope_call_parses_cleanly() {
        init_for_tests();
        let src = "\
class apb_monitor;
   virtual function void build_phase(int phase);
      int tmp;
      if (!uvm_config_db#(apb_vif)::get(this, \"\", \"vif\", tmp)) begin
      end
   endfunction
   virtual task run_phase(int phase);
   endtask
endclass
";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).expect("parse");
        assert_eq!(
            tree.tree.root_node().kind(),
            "source_file",
            "expected source_file, got {}: {}",
            tree.tree.root_node().kind(),
            tree.tree.root_node().to_sexp(),
        );
        assert!(
            !tree.has_errors(),
            "tree must be error-free after rewrite, got: {}",
            tree.tree.root_node().to_sexp(),
        );
    }
}
