//! Walk a [`SyntaxTree`] and emit one [`Diagnostic`] per parse error.
//!
//! tree-sitter has two kinds of "things went wrong" markers in the tree:
//!
//! * `ERROR` — a span of source the parser couldn't fit into the grammar.
//! * `MISSING` — a token the grammar required but wasn't present (e.g. the
//!   `;` after `module foo`). These have zero width.
//!
//! Both turn into LSP `Diagnostic`s of severity `Error`. We give them
//! distinct messages so users can tell which is which.

use mimir_core::{Position, Range};
use ropey::Rope;
use tracing::trace;
use tree_sitter::Node;

use crate::SyntaxTree;

/// LSP-compatible diagnostic severity. We mirror the LSP enum locally so
/// `mimir-syntax` doesn't have to depend on `tower-lsp` / `lsp-types`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Hard error — code won't compile / elaborate.
    Error,
    /// Soft warning — likely problem, won't block compile.
    Warning,
    /// Informational — style hint, etc.
    Information,
    /// Editor hint — usually rendered as faded text.
    Hint,
}

/// A single diagnostic. The server crate maps these to `lsp_types::Diagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Range in LSP coordinates (UTF-16 columns).
    pub range: Range,
    /// Human-readable message.
    pub message: String,
    /// Severity. Parse errors are always `Error`.
    pub severity: DiagnosticSeverity,
    /// Stable code (e.g. `"syntax"`) for grouping in the editor UI.
    pub code: &'static str,
}

/// Walk the tree and collect all parse-error diagnostics.
///
/// `rope` is needed to convert tree-sitter byte offsets back into LSP
/// (line, UTF-16 column) positions. The rope must reflect the same source
/// the tree was parsed from.
#[must_use]
pub fn collect(tree: &SyntaxTree, rope: &Rope) -> Vec<Diagnostic> {
    let root = tree.tree.root_node();

    // Fast path: tree-sitter sets a flag on the root if anything in the tree
    // is an error. If it's clean, skip the walk entirely.
    if !root.has_error() {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    walk(root, rope, tree.source(), &mut diagnostics);
    trace!(count = diagnostics.len(), "collected diagnostics");
    diagnostics
}

/// Recursive walk. Pre-order: visit the node, then descend into children.
///
/// Iteration uses `walk()` cursors instead of `node.child(i)` because
/// cursors are a single allocation amortized over the whole walk.
fn walk(node: Node<'_>, rope: &Rope, source: &str, out: &mut Vec<Diagnostic>) {
    if node.is_missing() {
        out.push(make_missing_diagnostic(node, rope));
        return;
    }

    if node.is_error() {
        // Prefer narrower nested ERROR/MISSING descendants when they exist:
        // a parser unwind can produce one ERROR that swallows an entire
        // class or module, but the inner ERRORs sit much closer to where
        // parsing actually went off the rails. Fall back to the outer span
        // (capped) only if there's nothing narrower to point at.
        let before = out.len();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_error() || child.is_missing() || child.has_error() {
                walk(child, rope, source, out);
            }
        }
        if out.len() == before {
            out.push(make_error_diagnostic(node, rope, source));
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Optimization: skip subtrees with no errors anywhere underneath.
        // `has_error()` is O(1) (it's a precomputed bit on the node).
        if child.has_error() || child.is_error() || child.is_missing() {
            walk(child, rope, source, out);
        }
    }
}

/// Build a diagnostic for an `ERROR` node. We include a snippet of the
/// offending text in the message because "syntax error" with no context is
/// a bad UX.
///
/// The visible range is capped to the start line: tree-sitter ERROR spans
/// can swallow an entire class or module after one bad token, and a
/// diagnostic that paints the whole file red is worse than useless.
fn make_error_diagnostic(node: Node<'_>, rope: &Rope, source: &str) -> Diagnostic {
    let range = cap_range_to_start_line(node_range(node, rope), rope);
    let snippet = source
        .get(node.byte_range())
        .map(truncate_for_message)
        .unwrap_or_else(|| "<invalid utf-8 in error span>".to_string());

    Diagnostic {
        range,
        message: format!("syntax error near `{snippet}`"),
        severity: DiagnosticSeverity::Error,
        code: "syntax",
    }
}

/// Clamp a multi-line range down to its starting line so a single confused
/// parse doesn't paint dozens of lines red. Single-line ranges are returned
/// unchanged.
fn cap_range_to_start_line(range: Range, rope: &Rope) -> Range {
    if range.end.line == range.start.line {
        return range;
    }
    let line_idx = range.start.line as usize;
    if line_idx >= rope.len_lines() {
        return range;
    }
    let line = rope.line(line_idx);
    let line_str: String = line.into();
    let trimmed = line_str.trim_end_matches('\n').trim_end_matches('\r');
    let utf16_cols = trimmed.encode_utf16().count() as u32;
    Range::new(range.start, Position::new(range.start.line, utf16_cols))
}

/// Build a zero-width diagnostic for a `MISSING` node — a token the grammar
/// required but wasn't found.
fn make_missing_diagnostic(node: Node<'_>, rope: &Rope) -> Diagnostic {
    let range = node_range(node, rope);
    // tree-sitter's `kind()` on a MISSING node tells us *what* was missing,
    // e.g. `;` or `endmodule`. Show that to the user — it's actionable.
    Diagnostic {
        range,
        message: format!("missing `{}`", node.kind()),
        severity: DiagnosticSeverity::Error,
        code: "syntax",
    }
}

/// Convert a tree-sitter node's byte span to an LSP range.
fn node_range(node: Node<'_>, rope: &Rope) -> Range {
    let start = Position::from_byte_offset(rope, node.start_byte());
    let end = Position::from_byte_offset(rope, node.end_byte());
    Range::new(start, end)
}

/// Trim a snippet to keep diagnostic messages short. tree-sitter ERROR
/// spans can be huge if the parser unwinds a long subtree — we only want
/// the first ~40 chars, single-line.
fn truncate_for_message(snippet: &str) -> String {
    const MAX: usize = 40;
    let first_line = snippet.lines().next().unwrap_or("");
    if first_line.len() <= MAX {
        first_line.to_string()
    } else {
        format!("{}…", &first_line[..MAX])
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxParser;
    use mimir_core::logging::init_for_tests;

    /// Helper: parse `src` and return the diagnostics produced.
    fn diags(src: &str) -> Vec<Diagnostic> {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        collect(&tree, &Rope::from_str(src))
    }

    /// Valid code → no diagnostics, fast path returns immediately.
    #[test]
    fn no_errors_means_no_diagnostics() {
        assert!(diags("module foo;\nendmodule\n").is_empty());
    }

    /// Missing semicolon should produce at least one diagnostic.
    #[test]
    fn missing_semicolon_produces_diagnostic() {
        let d = diags("module foo\nendmodule\n");
        assert!(!d.is_empty(), "expected at least one diagnostic");
        // All diagnostics from this module are syntax errors.
        assert!(d.iter().all(|x| x.severity == DiagnosticSeverity::Error));
        assert!(d.iter().all(|x| x.code == "syntax"));
    }

    /// Garbage input produces an ERROR-node diagnostic with a useful snippet
    /// in the message.
    #[test]
    fn garbage_input_produces_error_node_diag() {
        let d = diags("@@@ not valid sv @@@\n");
        assert!(!d.is_empty());
        assert!(
            d.iter().any(|x| x.message.starts_with("syntax error")),
            "diagnostics: {d:#?}",
        );
    }

    /// A parser unwind that would otherwise produce one ERROR spanning many
    /// lines must instead surface narrower nested ERRORs. Regression for
    /// the case where a UVM file's parse failure painted the whole file
    /// red, with the snippet starting at the leading comment.
    #[test]
    fn nested_errors_are_preferred_over_outer_span() {
        let src = "\
// header comment
class c extends base;
   function void f();
      x::type_id::create(\"y\", this);
   endfunction
endclass
";
        let d = diags(src);
        assert!(!d.is_empty(), "expected at least one diagnostic");
        // No diagnostic should start on the comment line — that was the bug.
        for diag in &d {
            assert!(
                diag.range.start.line > 0,
                "diagnostic anchored to header comment: {diag:?}",
            );
        }
        // No diagnostic should span more than its starting line.
        for diag in &d {
            assert_eq!(
                diag.range.start.line, diag.range.end.line,
                "diagnostic spans multiple lines: {diag:?}",
            );
        }
    }

    /// Diagnostic ranges must point inside the source — line/character must
    /// be plausible.
    #[test]
    fn diagnostic_range_is_inside_source() {
        let src = "module foo\nendmodule\n";
        let d = diags(src);
        for diag in &d {
            assert!(diag.range.start.line < 3, "line out of range: {diag:?}");
            assert!(diag.range.end >= diag.range.start);
        }
    }
}
