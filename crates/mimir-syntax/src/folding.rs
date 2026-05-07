//! Foldable code regions extracted from the SV syntax tree.
//!
//! `textDocument/foldingRange` returns the line ranges an editor can collapse
//! behind a fold marker. We emit one [`FoldRange`] per top-level SV
//! construct (modules, classes, functions, tasks, packages, interfaces,
//! programs, properties, sequences, covergroups), recursing so a class's
//! methods are foldable inside the class's own fold.
//!
//! ## Why mirror the LSP shape locally?
//!
//! Same reason as [`crate::diagnostics::Diagnostic`] and [`crate::Symbol`]:
//! `mimir-syntax` doesn't depend on `tower-lsp`/`lsp_types`, so the parser
//! stays runtime-free and unit tests can run without a tokio reactor. The
//! conversion to `lsp_types::FoldingRange` lives at the server boundary in
//! `mimir-server::backend`.
//!
//! ## What we deliberately don't fold
//!
//! Comments are invisible to tree-sitter — they're stripped before the tree
//! is built. Folding comment blocks would need a separate text scan; that's
//! deferred until someone actually asks for it.
//!
//! Single-line constructs (`module m; endmodule` on one line) are filtered
//! out — there's nothing to collapse.

use tracing::trace;
use tree_sitter::Node;

use crate::SyntaxTree;

/// One foldable region in the source, in LSP line coordinates.
///
/// Whole-line folds — there's no `start_character`/`end_character` because
/// the editor decides exact column placement. Mirrors the subset of
/// `lsp_types::FoldingRange` we need; the server crate maps these onto the
/// wire type with `kind: Region`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldRange {
    /// Zero-based start line (matches LSP `line`).
    pub start_line: u32,
    /// Zero-based end line.
    pub end_line: u32,
}

/// Tree-sitter node kinds we treat as foldable. These are all the
/// declaration forms whose name + body span more than one line in
/// well-formatted code.
///
/// `function_body_declaration` / `task_body_declaration` are the
/// body-bearing forms — their parents (`function_declaration`,
/// `task_declaration`) just wrap them. We match only the body form so a
/// single function doesn't produce two folds, mirroring the same
/// disambiguation in [`crate::symbols`].
const FOLDABLE_KINDS: &[&str] = &[
    "module_declaration",
    "class_declaration",
    "function_body_declaration",
    "task_body_declaration",
    "package_declaration",
    "interface_declaration",
    "program_declaration",
    "property_declaration",
    "sequence_declaration",
    "covergroup_declaration",
];

/// Walk `tree` and return one [`FoldRange`] per foldable construct.
///
/// Output order is pre-order DFS — outer constructs precede inner ones.
/// One-line constructs (where `end_line == start_line`) are filtered out;
/// there's nothing to collapse.
///
/// We use tree-sitter `Node::start_position().row` directly (not via the
/// rope) because LSP `line` is a count of `\n`-separated lines and matches
/// tree-sitter's row regardless of UTF-8/UTF-16 column encoding.
#[must_use]
pub fn folding_ranges(tree: &SyntaxTree) -> Vec<FoldRange> {
    let mut out = Vec::new();
    walk(tree.tree.root_node(), &mut out);
    trace!(count = out.len(), "collected folding ranges");
    out
}

/// Recursive walker. Always descends — nested foldables (a class's methods,
/// a package's classes) need their own ranges.
fn walk(node: Node<'_>, out: &mut Vec<FoldRange>) {
    if FOLDABLE_KINDS.contains(&node.kind()) {
        let start_line = node.start_position().row as u32;
        let end_line = node.end_position().row as u32;
        if end_line > start_line {
            out.push(FoldRange {
                start_line,
                end_line,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, out);
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

    fn fold(src: &str) -> Vec<FoldRange> {
        let mut parser = SyntaxParser::new().expect("grammar must load");
        let tree = parser.parse(src, None).expect("parse must succeed");
        folding_ranges(&tree)
    }

    #[test]
    fn module_yields_one_fold() {
        init_for_tests();
        let src = "module foo;\n  logic x;\nendmodule\n";
        let ranges = fold(src);
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2,
            }]
        );
    }

    #[test]
    fn single_line_module_skipped() {
        init_for_tests();
        let src = "module m; endmodule\n";
        assert!(fold(src).is_empty(), "one-line module shouldn't fold");
    }

    #[test]
    fn class_with_method_yields_two_folds() {
        init_for_tests();
        // class on lines 0..4, method on lines 1..3. Pre-order: outer first.
        let src = "class C;\n  function void f();\n    return;\n  endfunction\nendclass\n";
        let ranges = fold(src);
        assert_eq!(ranges.len(), 2, "expected outer class + inner method");
        assert_eq!(ranges[0].start_line, 0, "class fold first (preorder)");
        assert!(
            ranges[1].start_line > ranges[0].start_line
                && ranges[1].end_line < ranges[0].end_line,
            "method fold should be nested inside class fold: got {:?}",
            ranges
        );
    }

    #[test]
    fn package_class_method_three_folds() {
        init_for_tests();
        let src = "\
package p;
  class C;
    function void f();
      return;
    endfunction
  endclass
endpackage
";
        let ranges = fold(src);
        assert_eq!(ranges.len(), 3, "expected package + class + method");
        // Pre-order: outermost first.
        assert!(ranges[0].start_line < ranges[1].start_line);
        assert!(ranges[1].start_line < ranges[2].start_line);
        assert!(ranges[2].end_line < ranges[1].end_line);
        assert!(ranges[1].end_line < ranges[0].end_line);
    }

    #[test]
    fn function_body_only_not_function_declaration() {
        init_for_tests();
        // One function in a class. If we matched both `function_declaration`
        // and `function_body_declaration` we'd emit two ranges; we don't.
        let src = "class C;\n  function void f();\n    return;\n  endfunction\nendclass\n";
        let ranges = fold(src);
        // class + one function = 2, not 3.
        assert_eq!(ranges.len(), 2);
    }

    #[test]
    fn task_body_yields_a_fold() {
        init_for_tests();
        let src = "module m;\n  task automatic t();\n    $display(\"hi\");\n  endtask\nendmodule\n";
        let ranges = fold(src);
        // module + task body
        assert_eq!(ranges.len(), 2);
    }

    #[test]
    fn property_sequence_covergroup_each_yield_a_fold() {
        init_for_tests();
        let src = "\
module m(input logic clk);
  property p_req;
    @(posedge clk) 1;
  endproperty
  sequence s_req;
    @(posedge clk) 1;
  endsequence
  covergroup cg @(posedge clk);
    cp: coverpoint clk;
  endgroup
endmodule
";
        let ranges = fold(src);
        // module + property + sequence + covergroup = 4
        assert_eq!(
            ranges.len(),
            4,
            "expected one fold each for module/property/sequence/covergroup, got {:?}",
            ranges
        );
    }

    #[test]
    fn empty_source_yields_no_folds() {
        init_for_tests();
        assert!(fold("").is_empty());
    }
}
