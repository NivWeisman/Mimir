//! Smart selection ranges from the SV syntax tree.
//!
//! `textDocument/selectionRange` powers an editor's "expand selection"
//! command (VS Code `Shift+Alt+→`, Emacs `expand-region`-style): from the
//! token under the cursor, each keypress grows the selection to the next
//! enclosing syntactic construct — identifier → expression → statement →
//! block → function → class, and so on up to the whole file.
//!
//! We produce this by walking the parse tree from the leaf node at the
//! cursor up to the root, emitting each ancestor's range. Consecutive
//! ancestors that share the exact same span (common in tree-sitter, where a
//! node may wrap a single child of equal extent) are collapsed so the user
//! doesn't have to press the key twice for no visible change.
//!
//! ## Why mirror the LSP shape locally?
//!
//! Same reason as [`crate::folding::FoldRange`] and [`crate::Symbol`]:
//! `mimir-syntax` stays free of `tower-lsp` / `lsp_types`. We return a plain
//! `Vec<Range>` (innermost first); the server boundary links it into the
//! `lsp_types::SelectionRange` parent-chain.

use ropey::Rope;

use mimir_core::{Position, Range};

use crate::symbols::node_range;
use crate::SyntaxTree;

/// Return the chain of nested syntactic ranges at `pos`, **innermost first**.
///
/// Each range strictly contains the previous one. The first entry is the
/// smallest node covering the cursor; the last is the whole file. Returns an
/// empty vector when `pos` is out of bounds or the tree is empty.
///
/// The server links these into an `lsp_types::SelectionRange` where entry
/// `i`'s `parent` is entry `i + 1`.
#[must_use]
pub fn selection_ranges_at(tree: &SyntaxTree, rope: &Rope, pos: Position) -> Vec<Range> {
    let Ok(byte) = pos.to_byte_offset(rope) else {
        return Vec::new();
    };
    let root = tree.tree.root_node();
    let Some(leaf) = root.descendant_for_byte_range(byte, byte) else {
        return Vec::new();
    };

    let mut ranges: Vec<Range> = Vec::new();
    let mut cur = Some(leaf);
    while let Some(node) = cur {
        let r = node_range(node, rope);
        // Collapse zero-width steps: a parent that spans exactly the same
        // bytes as its child would make "expand selection" a no-op keypress.
        if ranges.last() != Some(&r) {
            ranges.push(r);
        }
        cur = node.parent();
    }
    ranges
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxParser;
    use mimir_core::logging::init_for_tests;

    fn ranges_at(src: &str, line: u32, col: u32) -> Vec<Range> {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let rope = Rope::from_str(src);
        selection_ranges_at(&tree, &rope, Position::new(line, col))
    }

    /// The chain is innermost-first and strictly nesting: each range
    /// contains the previous, and the last covers (at least) the whole
    /// construct up to the file root.
    #[test]
    fn ranges_nest_outward_from_cursor() {
        let src = "module m;\n  initial begin\n    x = a + b;\n  end\nendmodule\n";
        // Cursor on `a` in `x = a + b;` (line 2). `    x = a + b;`:
        // 4 spaces, x=4, ' '=5, '='=6, ' '=7, a=8.
        let chain = ranges_at(src, 2, 8);
        assert!(chain.len() >= 2, "expected a nesting chain, got {chain:?}");

        // Innermost covers just `a` (or a small span around it on line 2).
        assert_eq!(chain[0].start.line, 2);

        // Strictly non-shrinking outward: every step contains the prior.
        for w in chain.windows(2) {
            let inner = &w[0];
            let outer = &w[1];
            let inner_start = (inner.start.line, inner.start.character);
            let inner_end = (inner.end.line, inner.end.character);
            let outer_start = (outer.start.line, outer.start.character);
            let outer_end = (outer.end.line, outer.end.character);
            assert!(
                outer_start <= inner_start && outer_end >= inner_end,
                "range {outer:?} must contain {inner:?}",
            );
        }

        // The outermost spans the whole module (starts at line 0).
        let last = chain.last().unwrap();
        assert_eq!(last.start.line, 0);
    }

    /// No duplicate consecutive ranges — tree-sitter wrapper nodes that
    /// share a child's exact span are collapsed.
    #[test]
    fn consecutive_duplicate_ranges_are_collapsed() {
        let src = "module m;\n  logic x;\nendmodule\n";
        let chain = ranges_at(src, 1, 8); // on `x`
        for w in chain.windows(2) {
            assert_ne!(w[0], w[1], "consecutive ranges must differ: {chain:?}");
        }
    }

    /// Out-of-bounds cursor yields an empty chain rather than panicking.
    #[test]
    fn out_of_bounds_returns_empty() {
        let src = "module m;\nendmodule\n";
        assert!(ranges_at(src, 999, 0).is_empty());
    }
}
