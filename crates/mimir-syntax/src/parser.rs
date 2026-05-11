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

/// Replace `IDENT#(...)::IDENT` parameterized-scope glue with `_`
/// characters of identical byte length.
///
/// UVM code is full of expressions like `uvm_config_db#(apb_vif)::get(this)`.
/// tree-sitter-verilog's grammar can't handle this inside a method body
/// — it derails into an `(ERROR ...)` root and drags the enclosing
/// `class_declaration` down with it. Once the class envelope vanishes,
/// the symbol indexer, folder, and scope-aware highlighter all lose
/// most of the class's methods.
///
/// We rewrite the `#(...)::` middle into underscores so the parser sees
/// a single fused identifier (`uvm_config_db_______________get`). That
/// parses cleanly as a `tf_call` token; the surrounding method body
/// parses normally and the class structure is preserved.
///
/// **Why fuse instead of keeping `::`?** Tried it; tree-sitter-verilog
/// reaches for `let_expression` (which takes formal args) when it sees
/// `IDENT::IDENT(args)` in an expression slot, and the actual call args
/// land in an ERROR subtree. The fused `IDENT(args)` shape produces a
/// clean `tf_call` with no error, at the cost of the call's symbol
/// resolution: the mangled name doesn't match any declaration so
/// tree-sitter signature_help on the call site returns None. Slang
/// handles those calls correctly when configured.
///
/// **Byte preservation**: identical to `blank_backtick_lines`. We only
/// substitute characters within a single line and replace each byte 1:1
/// with `_`, so every byte offset, line number, and column tree-sitter
/// emits maps verbatim back to the original source. Multi-line `#(...)`
/// is intentionally skipped — too rare in practice to be worth the
/// brace-counting complexity, and refusing to rewrite is always safe.
///
/// Returns `Cow::Borrowed(source)` when no `#(` appears (fast path).
fn blank_parameterized_scopes(source: &str) -> Cow<'_, str> {
    if !source.contains("#(") {
        return Cow::Borrowed(source);
    }

    // Scan first; only allocate if we actually find a rewrite. Each
    // hit is `(scope_start_byte, scope_end_byte_exclusive)` covering
    // `#(...)::` inclusive — same byte length we'll blank with `_`.
    let bytes = source.as_bytes();
    let mut hits: Vec<(usize, usize)> = Vec::new();

    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'#' || bytes[i + 1] != b'(' {
            i += 1;
            continue;
        }
        // Must be preceded by an identifier char (otherwise this is e.g.
        // a literal `#5` delay or `#1ns` timing, not a parameterized
        // scope).
        if i == 0 || !is_ident_byte(bytes[i - 1]) {
            i += 1;
            continue;
        }
        // Walk to the matching `)` on the same line. Refuse multi-line
        // — too rare to bother with newline/comment tracking yet.
        let scope_start = i;
        let mut depth: usize = 1;
        let mut j = i + 2;
        while j < bytes.len() {
            match bytes[j] {
                b'\n' => {
                    depth = usize::MAX;
                    break;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if depth != 0 {
            // Bailed (newline or EOF) — advance past this `#(` and keep
            // scanning. Don't claim it as a rewrite.
            i += 2;
            continue;
        }
        // `j` is the closing `)`. Must be followed by `::IDENT` for
        // this to be the parameterized-scope pattern (vs an instance's
        // parameter-value assignment like `fifo #(.WIDTH(8)) u_fifo(...)`).
        if j + 3 >= bytes.len()
            || bytes[j + 1] != b':'
            || bytes[j + 2] != b':'
            || !is_ident_start_byte(bytes[j + 3])
        {
            i = j + 1;
            continue;
        }
        let scope_end = j + 3; // first byte AFTER the `::`
        // Bail on any non-ASCII byte in the rewrite range. Replacing a
        // UTF-8 multi-byte sequence with single-byte underscores would
        // shift UTF-16 column counts and break LSP position math.
        if bytes[scope_start..scope_end].iter().any(|b| !b.is_ascii()) {
            i = scope_end;
            continue;
        }
        hits.push((scope_start, scope_end));
        i = scope_end;
    }

    if hits.is_empty() {
        return Cow::Borrowed(source);
    }

    // Build the output by copying source slices and emitting `_`
    // characters of identical byte length for each hit. No `unsafe`,
    // no per-byte indexing into a mutable buffer.
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end) in hits {
        out.push_str(&source[cursor..start]);
        for _ in start..end {
            out.push('_');
        }
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    debug_assert_eq!(out.len(), source.len(), "byte-preservation invariant broken");
    Cow::Owned(out)
}

#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn is_ident_start_byte(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
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
    /// Two byte-preserving rewrites run before tree-sitter sees the text:
    ///
    /// 1. `blank_backtick_lines` — whole-line backtick tokens (`` `ifdef ``,
    ///    `` `uvm_fatal `` etc.) are replaced with spaces so compiler
    ///    directives and standalone macro calls don't collapse the file into
    ///    a single ERROR root. `` `define `` lines are left intact so the
    ///    symbol indexer can still find macro names.
    /// 2. `blank_parameterized_scopes` — `IDENT#(...)::IDENT` (UVM
    ///    parameterized scope access) is rewritten to a single fused
    ///    identifier so an expression like `uvm_config_db#(T)::get(this)`
    ///    parses as a normal call instead of derailing the enclosing
    ///    `class_declaration`.
    ///
    /// Both passes preserve every byte offset and line number, so anything
    /// tree-sitter emits maps verbatim back to the original source.
    #[instrument(level = "debug", skip(self, source, previous), fields(bytes = source.len()))]
    pub fn parse(
        &mut self,
        source: &str,
        previous: Option<&Tree>,
    ) -> Result<SyntaxTree, SyntaxParserError> {
        let blanked = blank_backtick_lines(source);
        // Second pass over the (possibly already-blanked) text. Collapse
        // the two Cows: only allocate again if the second pass actually
        // rewrote something the first pass didn't.
        let preprocessed: Cow<'_, str> = match blank_parameterized_scopes(blanked.as_ref()) {
            Cow::Borrowed(_) => blanked,
            Cow::Owned(s) => Cow::Owned(s),
        };
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

    /// `blank_parameterized_scopes` rewrites `IDENT#(...)::IDENT` while
    /// preserving every byte offset.
    #[test]
    fn blank_parameterized_scopes_rewrites_uvm_call() {
        init_for_tests();
        let src = "x = uvm_config_db#(apb_vif)::get(this);";
        let out = blank_parameterized_scopes(src);
        assert_eq!(out.len(), src.len(), "byte length must be preserved");
        // The `#(apb_vif)::` glue (12 bytes) becomes 12 underscores; the
        // bracketing identifiers (`uvm_config_db`, `get`) are untouched.
        assert!(out.contains("uvm_config_db____________get"), "got: {out}");
        // No `#(` should remain in the rewritten slice.
        assert!(!out.contains("#("), "got: {out}");
    }

    /// Fast path: no `#(` → no allocation.
    #[test]
    fn blank_parameterized_scopes_no_op_without_pattern() {
        let src = "module foo; endmodule\n";
        let out = blank_parameterized_scopes(src);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    /// `#5` delays and `#1ns` timing controls aren't preceded by an
    /// identifier char, so they must not be rewritten.
    #[test]
    fn blank_parameterized_scopes_skips_delay_control() {
        let src = "always @(*) #5 a = b;";
        let out = blank_parameterized_scopes(src);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    /// Multi-line `#(...)` is intentionally left alone in v1.
    #[test]
    fn blank_parameterized_scopes_skips_multiline() {
        let src = "x = klass#(\n  int\n)::method();";
        let out = blank_parameterized_scopes(src);
        // Either Borrowed (no rewrite) or unchanged byte content.
        assert_eq!(out.as_ref(), src);
    }

    /// `#(...)` without a trailing `::IDENT` (just a parameter-value
    /// assignment on an instance) must not be rewritten.
    #[test]
    fn blank_parameterized_scopes_skips_instance_params() {
        let src = "fifo #(.WIDTH(8), .DEPTH(16)) u_fifo (clk, rst);";
        let out = blank_parameterized_scopes(src);
        assert_eq!(out.as_ref(), src);
    }

    /// End-to-end: a class whose method body contains a parameterized
    /// scope call must parse with a `source_file` root (not ERROR) and
    /// surface all of its methods to the symbol indexer. This is the
    /// shape that motivated the rewrite.
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
