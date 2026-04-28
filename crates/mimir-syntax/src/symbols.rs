//! Symbol extraction and identifier-at-position lookup.
//!
//! Two responsibilities, both fed by the same tree-sitter parse tree:
//!
//! 1. [`index`] walks the tree and emits one [`Symbol`] per declaration
//!    we can recognise (modules, classes, tasks, functions, typedefs,
//!    parameters, variables, ports, properties, sequences, covergroups).
//!    Used by `mimir-server` to power `textDocument/documentSymbol` and
//!    same-file `textDocument/definition`.
//!
//! 2. [`identifier_at`] returns the identifier text under a given LSP
//!    position, or `None` if the cursor is on whitespace, a comment, or
//!    a non-identifier token. Used as the first step of go-to-definition
//!    — the server takes that name and looks it up in the index.
//!
//! ## Why mirror LSP shapes instead of using `lsp_types`?
//!
//! Same reason `mimir-syntax::Diagnostic` does: this crate doesn't depend
//! on `tower-lsp`/`lsp_types`, so the parser stays runtime-free and unit
//! tests don't need a tokio reactor. The server boundary in
//! `mimir-server::backend` does the conversion.
//!
//! ## Coverage notes
//!
//! Tree-sitter is a syntactic recogniser; without scope rules we can't
//! distinguish a `var x` shadowing a class field `x`. The server's
//! resolver returns *all* matches by name in that case — VS Code shows a
//! peek list, which is the right UX for a syntactic backend. Stage 3 of
//! the go-to-definition plan replaces this path with slang's semantic
//! resolver when the sidecar is configured.

use mimir_core::{Position, Range};
use ropey::Rope;
use tracing::trace;
use tree_sitter::Node;

use crate::SyntaxTree;

/// Kind of declaration a [`Symbol`] represents.
///
/// Mirrors the subset of `lsp_types::SymbolKind` we actually emit. The
/// server crate maps these onto the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    /// `module foo; … endmodule`.
    Module,
    /// `interface foo; … endinterface`.
    Interface,
    /// `program foo; … endprogram`.
    Program,
    /// `package foo; … endpackage`.
    Package,
    /// `class foo; … endclass`.
    Class,
    /// `task foo(); … endtask`.
    Task,
    /// `function … foo(); … endfunction`.
    Function,
    /// `typedef … foo;` (struct, enum, alias).
    Typedef,
    /// `parameter int W = 8;` or `param_assignment` inside a port list.
    Parameter,
    /// `logic [7:0] x;`, `bit b;`, etc. — entries in a
    /// `list_of_variable_decl_assignments`.
    Variable,
    /// `input clk`, `output q` — an ANSI port declaration's name token.
    Port,
    /// SVA `property p; … endproperty`.
    Property,
    /// SVA `sequence s; … endsequence`.
    Sequence,
    /// `covergroup cg @(…); … endgroup`.
    Covergroup,
}

/// One declared name in the source file.
///
/// `name_range` is the span of the *identifier token* — that's what
/// go-to-definition jumps to. `full_range` is the whole declaration —
/// what `documentSymbol` outlines hand to the editor for selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The identifier text, e.g. `"my_module"`.
    pub name: String,
    /// What kind of declaration this is.
    pub kind: SymbolKind,
    /// LSP range of the identifier token. Go-to-definition lands here.
    pub name_range: Range,
    /// LSP range of the entire declaration (e.g. the whole
    /// `module … endmodule` span). Used as `documentSymbol`'s
    /// `selectionRange`/`range`.
    pub full_range: Range,
}

/// Walk `tree` and return every declaration we can recognise.
///
/// Order is the depth-first traversal order — i.e. roughly source order,
/// with nested declarations appearing right after their enclosing one.
/// `mimir-server` re-uses this order for `documentSymbol`.
///
/// `rope` must reflect the same source the tree was parsed from.
#[must_use]
pub fn index(tree: &SyntaxTree, rope: &Rope) -> Vec<Symbol> {
    let mut out = Vec::new();
    walk_for_symbols(tree.tree.root_node(), tree.source(), rope, &mut out);
    trace!(count = out.len(), "indexed symbols");
    out
}

/// Recursive walker. We always descend, even after emitting a symbol —
/// a `class` contains methods, a `module` contains parameters and
/// instances, etc.
fn walk_for_symbols(node: Node<'_>, source: &str, rope: &Rope, out: &mut Vec<Symbol>) {
    if let Some(symbol) = symbol_for(node, source, rope) {
        out.push(symbol);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_symbols(child, source, rope, out);
    }
}

/// If `node` is a declaration we recognise, build a `Symbol` for it.
fn symbol_for(node: Node<'_>, source: &str, rope: &Rope) -> Option<Symbol> {
    let kind = SymbolKind::from_node_kind(node.kind())?;
    let name_node = name_node_of(node)?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_string();
    Some(Symbol {
        name,
        kind,
        name_range: node_range(name_node, rope),
        full_range: node_range(node, rope),
    })
}

impl SymbolKind {
    /// Map a tree-sitter node kind name to a `SymbolKind`. Returns
    /// `None` for nodes that aren't declarations we track.
    fn from_node_kind(kind: &str) -> Option<Self> {
        // `function_body_declaration` and `task_body_declaration` are
        // the bearers of the name. Their parents (`function_declaration`,
        // `task_declaration`) just wrap them, so matching only the
        // body-form avoids emitting two symbols for one declaration.
        Some(match kind {
            "module_declaration" => SymbolKind::Module,
            "interface_declaration" => SymbolKind::Interface,
            "program_declaration" => SymbolKind::Program,
            "package_declaration" => SymbolKind::Package,
            "class_declaration" => SymbolKind::Class,
            "function_body_declaration" => SymbolKind::Function,
            "task_body_declaration" => SymbolKind::Task,
            "type_declaration" => SymbolKind::Typedef,
            "param_assignment" => SymbolKind::Parameter,
            "variable_decl_assignment" => SymbolKind::Variable,
            "ansi_port_declaration" => SymbolKind::Port,
            "property_declaration" => SymbolKind::Property,
            "sequence_declaration" => SymbolKind::Sequence,
            "covergroup_declaration" => SymbolKind::Covergroup,
            _ => return None,
        })
    }
}

/// Find the `simple_identifier` node carrying the *name* of a
/// declaration `decl`.
///
/// Each declaration kind has its own structure in the SV grammar — the
/// header lives under different child node kinds, and for some kinds
/// (`type_declaration`, `variable_decl_assignment`) the simplest
/// approach is to take the *direct* `simple_identifier` child rather
/// than recursing, because the body subtree contains unrelated
/// identifiers (struct field names, init-expression references, …).
fn name_node_of<'a>(decl: Node<'a>) -> Option<Node<'a>> {
    match decl.kind() {
        "module_declaration" => header_name(
            decl,
            &[
                "module_header",
                "module_ansi_header",
                "module_nonansi_header",
            ],
        ),
        "interface_declaration" => header_name(
            decl,
            &["interface_ansi_header", "interface_nonansi_header"],
        ),
        "program_declaration" => header_name(
            decl,
            &["program_ansi_header", "program_nonansi_header"],
        ),
        "package_declaration" => first_descendant_of_kind(decl, "simple_identifier"),
        "class_declaration" => {
            // `class_identifier` appears twice in `class c extends b;` —
            // the first one is the class being defined, the second is
            // the parent type. We want the first.
            let id = first_named_child_of_kind(decl, "class_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "function_body_declaration" => {
            let id = first_named_child_of_kind(decl, "function_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "task_body_declaration" => {
            let id = first_named_child_of_kind(decl, "task_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "type_declaration" => first_named_child_of_kind(decl, "simple_identifier"),
        "param_assignment" => {
            let id = first_named_child_of_kind(decl, "parameter_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "variable_decl_assignment" => first_named_child_of_kind(decl, "simple_identifier"),
        "ansi_port_declaration" => {
            let id = first_named_child_of_kind(decl, "port_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "property_declaration" => {
            let id = first_named_child_of_kind(decl, "property_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "sequence_declaration" => first_named_child_of_kind(decl, "simple_identifier"),
        "covergroup_declaration" => {
            let id = first_named_child_of_kind(decl, "covergroup_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        _ => None,
    }
}

/// Pull the name out of the first matching header child.
///
/// Module/interface/program declarations all share the pattern
/// "header child holds the identifier" with kind names that vary by
/// ANSI vs non-ANSI form.
fn header_name<'a>(decl: Node<'a>, header_kinds: &[&str]) -> Option<Node<'a>> {
    let header = first_named_child_of_kinds(decl, header_kinds)?;
    first_descendant_of_kind(header, "simple_identifier")
}

/// First *named* (i.e. non-anonymous) direct child whose kind matches.
fn first_named_child_of_kind<'a>(parent: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = parent.walk();
    let found = parent
        .named_children(&mut cursor)
        .find(|c| c.kind() == kind);
    found
}

/// First named direct child whose kind matches any of `kinds`.
fn first_named_child_of_kinds<'a>(parent: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = parent.walk();
    let found = parent
        .named_children(&mut cursor)
        .find(|c| kinds.contains(&c.kind()));
    found
}

/// Pre-order DFS for the first descendant (or `node` itself) whose
/// kind matches. Used when a declaration nests its identifier inside a
/// header / wrapper subtree we don't want to enumerate every level of.
fn first_descendant_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_descendant_of_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

/// Convert a tree-sitter node's byte span to an LSP range.
fn node_range(node: Node<'_>, rope: &Rope) -> Range {
    Range::new(
        Position::from_byte_offset(rope, node.start_byte()),
        Position::from_byte_offset(rope, node.end_byte()),
    )
}

// --------------------------------------------------------------------------
// identifier_at
// --------------------------------------------------------------------------

/// Return the identifier text under `pos`, if any.
///
/// Returns `Some(name)` when `pos` falls inside a `simple_identifier`
/// or `system_tf_identifier` token, `None` otherwise (whitespace,
/// punctuation, comments, keywords, end-of-document).
///
/// `pos` uses LSP coordinates (UTF-16 columns); we convert via
/// `Position::to_byte_offset` exactly once and work in bytes after
/// that, per the crate-wide pattern in
/// [`crates/mimir-core/src/document.rs`](../../mimir-core/src/document.rs).
#[must_use]
pub fn identifier_at<'a>(tree: &'a SyntaxTree, rope: &Rope, pos: Position) -> Option<&'a str> {
    let byte = pos.to_byte_offset(rope).ok()?;
    let root = tree.tree.root_node();
    // `descendant_for_byte_range(b, b)` returns the deepest node whose
    // span contains byte `b`. tree-sitter treats the range as
    // inclusive-of-start, exclusive-of-end — so a cursor positioned
    // *just past* the last char of an identifier yields the next node
    // (typically punctuation). That's the LSP semantics we want.
    let leaf = root.descendant_for_byte_range(byte, byte)?;
    if matches!(leaf.kind(), "simple_identifier" | "system_tf_identifier") {
        tree.source().get(leaf.byte_range())
    } else {
        None
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
    use pretty_assertions::assert_eq;

    /// Helper: parse `src` and return its symbol index.
    fn idx(src: &str) -> Vec<Symbol> {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        index(&tree, &Rope::from_str(src))
    }

    /// Helper: parse `src` and look up identifier at `(line, col)`.
    fn ident_at(src: &str, line: u32, col: u32) -> Option<String> {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        identifier_at(&tree, &Rope::from_str(src), Position::new(line, col)).map(str::to_owned)
    }

    /// Find the first symbol with the given name. Many tests have only
    /// one symbol of interest; this keeps the assertions readable.
    fn pick<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:#?}"))
    }

    #[test]
    fn module_is_indexed() {
        let s = idx("module my_mod;\nendmodule\n");
        let m = pick(&s, "my_mod");
        assert_eq!(m.kind, SymbolKind::Module);
        // Name range covers exactly "my_mod" on line 0, columns 7..13.
        assert_eq!(m.name_range.start.line, 0);
        assert_eq!(m.name_range.start.character, 7);
        assert_eq!(m.name_range.end.character, 13);
    }

    #[test]
    fn interface_is_indexed() {
        let s = idx("interface my_if; endinterface\n");
        assert_eq!(pick(&s, "my_if").kind, SymbolKind::Interface);
    }

    #[test]
    fn program_is_indexed() {
        let s = idx("program my_pgm; endprogram\n");
        assert_eq!(pick(&s, "my_pgm").kind, SymbolKind::Program);
    }

    #[test]
    fn package_is_indexed() {
        let s = idx("package my_pkg; endpackage\n");
        assert_eq!(pick(&s, "my_pkg").kind, SymbolKind::Package);
    }

    #[test]
    fn class_with_extends_picks_self_not_parent() {
        // Regression: a `class c extends b;` produces two
        // `class_identifier` nodes. We must take the first (the class
        // being defined), not the second (the parent class type).
        let s = idx("class c extends b; endclass\n");
        let classes: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Class).collect();
        assert_eq!(classes.len(), 1, "expected one class symbol, got {classes:#?}");
        assert_eq!(classes[0].name, "c");
    }

    #[test]
    fn function_is_indexed_once() {
        // Both `function_declaration` and `function_body_declaration`
        // appear in the tree; we want exactly one symbol per function.
        let s = idx("class c; function void f(); endfunction\nendclass\n");
        let fns: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Function).collect();
        assert_eq!(fns.len(), 1, "expected one function symbol, got {fns:#?}");
        assert_eq!(fns[0].name, "f");
    }

    #[test]
    fn task_is_indexed_once() {
        let s = idx("class c; task t(); endtask\nendclass\n");
        let tasks: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Task).collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "t");
    }

    #[test]
    fn typedef_struct_uses_alias_name_not_field_name() {
        // The struct member `a` is also a `simple_identifier`; we must
        // pick the typedef's alias, which is the *direct* simple_identifier
        // child of `type_declaration`.
        let s = idx("typedef struct { int a; } my_t;\n");
        assert_eq!(pick(&s, "my_t").kind, SymbolKind::Typedef);
        // `a` should not be picked up as a typedef.
        assert!(!s.iter().any(|sy| sy.name == "a" && sy.kind == SymbolKind::Typedef));
    }

    #[test]
    fn typedef_enum_picks_alias_not_enumerators() {
        let s = idx("typedef enum { A, B } e_t;\n");
        assert_eq!(pick(&s, "e_t").kind, SymbolKind::Typedef);
    }

    #[test]
    fn parameter_declaration_inside_module() {
        let s = idx("module m;\nparameter int W = 8;\nendmodule\n");
        assert_eq!(pick(&s, "W").kind, SymbolKind::Parameter);
    }

    #[test]
    fn multi_variable_declaration_yields_one_symbol_each() {
        let s = idx("module m;\nlogic a, b, c;\nendmodule\n");
        let vars: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Variable).collect();
        let names: Vec<&str> = vars.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn ansi_ports_are_indexed() {
        let s = idx("module m(input logic clk, output logic q);\nendmodule\n");
        let ports: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Port).collect();
        let names: Vec<&str> = ports.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["clk", "q"]);
    }

    #[test]
    fn property_and_sequence_are_indexed() {
        let s = idx(
            "module m;\nproperty p; @(posedge clk) 1; endproperty\nsequence s; @(posedge clk) 1; endsequence\nendmodule\n",
        );
        assert_eq!(pick(&s, "p").kind, SymbolKind::Property);
        assert_eq!(pick(&s, "s").kind, SymbolKind::Sequence);
    }

    #[test]
    fn covergroup_is_indexed() {
        let s = idx("module m;\ncovergroup cg @(posedge clk); coverpoint x; endgroup\nendmodule\n");
        assert_eq!(pick(&s, "cg").kind, SymbolKind::Covergroup);
    }

    #[test]
    fn nested_class_yields_both_classes() {
        let s = idx("package p;\nclass outer;\nclass inner; endclass\nendclass\nendpackage\n");
        let classes: Vec<&str> = s
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(classes, vec!["outer", "inner"]);
    }

    // --- identifier_at -------------------------------------------------

    #[test]
    fn identifier_at_start_of_token() {
        // "module foo;" — column 7 is the 'f' of "foo".
        assert_eq!(ident_at("module foo;\nendmodule\n", 0, 7).as_deref(), Some("foo"));
    }

    #[test]
    fn identifier_at_middle_of_token() {
        // Column 8 is the 'o' in the middle of "foo".
        assert_eq!(ident_at("module foo;\nendmodule\n", 0, 8).as_deref(), Some("foo"));
    }

    #[test]
    fn identifier_at_just_past_token_returns_none() {
        // Column 10 is the ';' immediately after "foo". LSP semantics:
        // the position points *between* characters, so a position past
        // the last character of "foo" is the punctuation, not the
        // identifier.
        assert_eq!(ident_at("module foo;\nendmodule\n", 0, 10).as_deref(), None);
    }

    #[test]
    fn identifier_at_whitespace_returns_none() {
        // Column 6 is the space between "module" and "foo".
        assert_eq!(ident_at("module foo;\nendmodule\n", 0, 6), None);
    }

    #[test]
    fn identifier_at_keyword_returns_none() {
        // Column 0 is the 'm' of "module" — a keyword, not a
        // `simple_identifier`.
        assert_eq!(ident_at("module foo;\nendmodule\n", 0, 0), None);
    }

    #[test]
    fn identifier_at_out_of_bounds_returns_none() {
        // Position past end of file should not panic.
        assert_eq!(ident_at("module foo;\nendmodule\n", 99, 0), None);
    }

    #[test]
    fn identifier_at_finds_reference_inside_expression() {
        // `x` on the right-hand side is a `simple_identifier` referring
        // to the parameter on the left. identifier_at should find it.
        let src = "module m;\nparameter int W = 8;\ninitial W = W;\nendmodule\n";
        // Line 2 ("initial W = W;"), column 8 is the 'W' after `=`.
        assert_eq!(ident_at(src, 2, 12).as_deref(), Some("W"));
    }
}
