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
    if node.is_error() {
        out.push(make_error_diagnostic(node, rope, source));
        // Don't recurse into ERROR — its children are usually noisy and
        // would produce duplicate messages for the same span.
        return;
    }
    if node.is_missing() {
        out.push(make_missing_diagnostic(node, rope));
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
fn make_error_diagnostic(node: Node<'_>, rope: &Rope, source: &str) -> Diagnostic {
    let range = node_range(node, rope);
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
