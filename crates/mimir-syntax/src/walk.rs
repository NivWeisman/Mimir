//! Stack-safe traversal of a tree-sitter subtree.
//!
//! Every walker in this crate used to be a recursive function — one native
//! stack frame per tree level. That is fine for ordinary code, but a parse
//! tree is as deep as the source is nested, and SystemVerilog nests without
//! bound: a generated `assign x = a | b | c | …` with a few thousand terms
//! is a left-leaning chain of `expression` nodes thousands of levels deep.
//! Recursing over that overflows the thread stack, and a stack overflow is
//! an *abort* — no panic hook, no unwinding, the language server just dies.
//!
//! [`walk`] drives a [`tree_sitter::TreeCursor`] instead, so the traversal
//! state lives in the cursor's heap-allocated stack and the native stack
//! depth stays constant no matter how deep the tree is.

use tree_sitter::Node;

/// What the visitor wants the traversal to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Walk {
    /// Visit this node's children (the default for interior nodes).
    Descend,
    /// Don't visit this node's children; carry on with its next sibling.
    /// No [`Visit::Leave`] is delivered for a skipped node.
    Skip,
    /// Abort the whole traversal immediately.
    Stop,
}

/// One traversal event handed to the visitor closure.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Visit<'a> {
    /// Pre-order: the node is reached, before any of its children.
    Enter(Node<'a>),
    /// Post-order: all of the node's children have been visited. Delivered
    /// only for nodes whose `Enter` returned [`Walk::Descend`] (leaves
    /// included), so an enter/leave pair can bracket per-subtree state.
    Leave(Node<'a>),
}

/// Walk the subtree rooted at `root` (inclusive) in document order, visiting
/// named *and* anonymous nodes, without recursion.
///
/// The closure's return value steers the walk after an `Enter`; after a
/// `Leave` only [`Walk::Stop`] is honoured. Returns `false` when the visitor
/// stopped the walk early, `true` when the whole subtree was visited.
pub(crate) fn walk<'a>(root: Node<'a>, mut visit: impl FnMut(Visit<'a>) -> Walk) -> bool {
    let mut cursor = root.walk();
    // Depth below `root`. The cursor itself refuses to climb above the node
    // it was created on, but tracking depth makes the termination condition
    // explicit and independent of that guarantee.
    let mut depth: usize = 0;
    loop {
        let node = cursor.node();
        match visit(Visit::Enter(node)) {
            Walk::Stop => return false,
            Walk::Skip => {}
            Walk::Descend => {
                if cursor.goto_first_child() {
                    depth += 1;
                    continue;
                }
                // A leaf: its subtree is already complete.
                if visit(Visit::Leave(node)) == Walk::Stop {
                    return false;
                }
            }
        }
        // Advance to the next node in document order, leaving every
        // ancestor whose subtree we have now finished.
        loop {
            if depth == 0 {
                return true;
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return true;
            }
            depth -= 1;
            if visit(Visit::Leave(cursor.node())) == Walk::Stop {
                return false;
            }
        }
    }
}

/// Pre-order-only convenience over [`walk`] for visitors that need no
/// `Leave` events.
pub(crate) fn preorder<'a>(root: Node<'a>, mut enter: impl FnMut(Node<'a>) -> Walk) -> bool {
    walk(root, |event| match event {
        Visit::Enter(node) => enter(node),
        Visit::Leave(_) => Walk::Descend,
    })
}

/// First node of kind `kind` in the subtree rooted at `root` (inclusive),
/// in document order.
pub(crate) fn first_descendant_of_kind<'a>(root: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut found = None;
    preorder(root, |node| {
        if node.kind() == kind {
            found = Some(node);
            Walk::Stop
        } else {
            Walk::Descend
        }
    });
    found
}

/// `node` followed by each of its ancestors, innermost first (so index 0 is
/// `node` itself and the last entry is `root`).
///
/// Use this instead of looping on [`Node::parent`]: tree-sitter nodes carry
/// no parent pointer, so every `parent()` call re-descends from the tree
/// root. Climbing N levels that way costs O(N²) — several seconds for a
/// cursor inside a pathologically deep expression. This helper makes the
/// same descent exactly once and records the whole chain.
pub(crate) fn self_and_ancestors<'a>(root: Node<'a>, node: Node<'a>) -> Vec<Node<'a>> {
    let mut chain = Vec::new();
    let mut current = root;
    while current.id() != node.id() {
        chain.push(current);
        match current.child_with_descendant(node) {
            Some(next) => current = next,
            // `node` isn't under `root` (different tree / stale node):
            // return what we have rather than loop.
            None => break,
        }
    }
    chain.push(node);
    chain.reverse();
    chain
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxParser;

    fn kinds(src: &str, steer: impl Fn(&str) -> Walk) -> (Vec<String>, Vec<String>) {
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut entered = Vec::new();
        let mut left = Vec::new();
        walk(tree.tree.root_node(), |event| match event {
            Visit::Enter(n) => {
                entered.push(n.kind().to_string());
                steer(n.kind())
            }
            Visit::Leave(n) => {
                left.push(n.kind().to_string());
                Walk::Descend
            }
        });
        (entered, left)
    }

    /// Every entered node is left exactly once, root last.
    #[test]
    fn enter_and_leave_are_balanced() {
        let (entered, left) = kinds("module m;\n  int x;\nendmodule\n", |_| Walk::Descend);
        assert_eq!(entered.len(), left.len());
        assert_eq!(entered.first().map(String::as_str), Some("source_file"));
        assert_eq!(left.last().map(String::as_str), Some("source_file"));
        assert!(entered.iter().any(|k| k == "simple_identifier"));
    }

    /// `Skip` prunes the subtree and suppresses the node's `Leave`.
    #[test]
    fn skip_prunes_subtree_without_leave() {
        let (entered, left) = kinds("module m;\n  int x;\nendmodule\n", |k| {
            if k == "module_declaration" {
                Walk::Skip
            } else {
                Walk::Descend
            }
        });
        assert_eq!(entered, vec!["source_file", "module_declaration"]);
        assert_eq!(left, vec!["source_file"]);
    }

    /// `Stop` ends the walk at once.
    #[test]
    fn stop_aborts_immediately() {
        let (entered, left) = kinds("module m;\n  int x;\nendmodule\n", |k| {
            if k == "module_declaration" {
                Walk::Stop
            } else {
                Walk::Descend
            }
        });
        assert_eq!(entered, vec!["source_file", "module_declaration"]);
        assert!(left.is_empty());
    }

    /// The walk never escapes the subtree it was started on.
    #[test]
    fn walk_is_confined_to_its_root() {
        let src = "module a;\n  int x;\nendmodule\nmodule b;\n  int y;\nendmodule\n";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let first_module = tree.tree.root_node().named_child(0).unwrap();
        let mut idents = Vec::new();
        preorder(first_module, |n| {
            if n.kind() == "simple_identifier" {
                idents.push(n.utf8_text(src.as_bytes()).unwrap().to_string());
            }
            Walk::Descend
        });
        assert_eq!(idents, vec!["a", "x"]);
    }

    #[test]
    fn first_descendant_finds_in_document_order() {
        let src = "module m;\n  int x;\nendmodule\n";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let id = first_descendant_of_kind(tree.tree.root_node(), "simple_identifier").unwrap();
        assert_eq!(id.utf8_text(src.as_bytes()).unwrap(), "m");
        assert!(first_descendant_of_kind(tree.tree.root_node(), "class_declaration").is_none());
    }

    /// The chain matches what repeated `parent()` calls would produce.
    #[test]
    fn self_and_ancestors_matches_parent_chain() {
        let src = "module m;\n  initial begin\n    x = a + b;\n  end\nendmodule\n";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.tree.root_node();
        let byte = src.find("b;").unwrap();
        let leaf = root.descendant_for_byte_range(byte, byte).unwrap();
        let chain = self_and_ancestors(root, leaf);
        let mut expected = vec![leaf];
        let mut cur = leaf;
        while let Some(p) = cur.parent() {
            expected.push(p);
            cur = p;
        }
        assert_eq!(
            chain.iter().map(Node::id).collect::<Vec<_>>(),
            expected.iter().map(Node::id).collect::<Vec<_>>(),
        );
        assert_eq!(chain.last().map(Node::id), Some(root.id()));
        // Degenerate case: the root is its own (only) chain entry.
        assert_eq!(self_and_ancestors(root, root).len(), 1);
    }
}
