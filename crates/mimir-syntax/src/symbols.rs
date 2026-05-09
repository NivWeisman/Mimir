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

/// A formal parameter of a callable symbol (function, task, method, macro).
///
/// For macro parameters `ty` is always `None` — macros are textual
/// substitution and carry no SV type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// Parameter name, e.g. `"phase"`.
    pub name: String,
    /// Declared type text, e.g. `"int"`, `"string"`, `"uvm_phase"`.
    /// `None` for macro parameters or when the type is implicit.
    pub ty: Option<String>,
}

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
    /// `task foo(); … endtask` declared at file/module/package scope.
    Task,
    /// `function … foo(); … endfunction` declared at file/module/package scope.
    Function,
    /// `function`/`task` declared inside a `class` body. Distinguished
    /// from [`Function`]/[`Task`] so the editor's outline view (and
    /// future call-hierarchy work) can present class members as methods.
    Method,
    /// `typedef … foo;` (struct, enum, alias).
    Typedef,
    /// One name in `enum { A, B }` — the enumerator constants, not the
    /// surrounding `typedef enum`.
    EnumMember,
    /// `constraint c { … }` inside a class.
    Constraint,
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
    /// `` `define MY_MACRO … `` — a preprocessor text-macro definition.
    Macro,
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
    /// Formal parameters for callable declarations (functions, tasks, methods,
    /// macros). `None` for non-callable symbols (modules, classes, variables…).
    pub params: Option<Vec<Param>>,
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
    walk_for_symbols(
        tree.tree.root_node(),
        tree.source(),
        rope,
        /*inside_class=*/ false,
        &mut out,
    );
    trace!(count = out.len(), "indexed symbols");
    out
}

/// Recursive walker. We always descend, even after emitting a symbol —
/// a `class` contains methods, a `module` contains parameters and
/// instances, etc.
///
/// `inside_class` is sticky-on-descent: once we enter a `class_declaration`
/// every nested `function_body_declaration` / `task_body_declaration`
/// gets tagged as [`SymbolKind::Method`] instead of `Function` / `Task`.
/// Tree-sitter doesn't otherwise distinguish them — class scope is the
/// only thing that makes a `function` a method in SystemVerilog.
fn walk_for_symbols(
    node: Node<'_>,
    source: &str,
    rope: &Rope,
    inside_class: bool,
    out: &mut Vec<Symbol>,
) {
    if let Some(symbol) = symbol_for(node, source, rope, inside_class) {
        out.push(symbol);
    }
    let descend_inside_class = inside_class || node.kind() == "class_declaration";
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_symbols(child, source, rope, descend_inside_class, out);
    }
}

/// If `node` is a declaration we recognise, build a `Symbol` for it.
fn symbol_for(
    node: Node<'_>,
    source: &str,
    rope: &Rope,
    inside_class: bool,
) -> Option<Symbol> {
    let mut kind = SymbolKind::from_node_kind(node.kind())?;
    if inside_class && matches!(kind, SymbolKind::Function | SymbolKind::Task) {
        kind = SymbolKind::Method;
    }
    let name_node = name_node_of(node)?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_string();
    let params = extract_callable_params(node, source);
    Some(Symbol {
        name,
        kind,
        name_range: node_range(name_node, rope),
        full_range: node_range(node, rope),
        params,
    })
}

/// Extract formal parameters for callable declarations, or `None` for
/// non-callable symbols. Called from [`symbol_for`].
fn extract_callable_params(node: Node<'_>, source: &str) -> Option<Vec<Param>> {
    match node.kind() {
        "function_body_declaration" | "task_body_declaration" => {
            Some(collect_tf_port_params(node, source))
        }
        "text_macro_definition" => Some(collect_macro_params(node, source)),
        _ => None,
    }
}

/// Collect `Param` entries from `tf_port_list` inside a function/task body.
fn collect_tf_port_params(node: Node<'_>, source: &str) -> Vec<Param> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "tf_port_list" {
            let mut c2 = child.walk();
            for item in child.named_children(&mut c2) {
                if matches!(item.kind(), "tf_port_item" | "tf_port_item1") {
                    if let Some(p) = param_from_port_item(item, source) {
                        out.push(p);
                    }
                }
            }
            break;
        }
    }
    out
}

/// Build a single `Param` from a `tf_port_item` or `tf_port_item1` node.
fn param_from_port_item(item: Node<'_>, source: &str) -> Option<Param> {
    let port_id = first_named_child_of_kind(item, "port_identifier")?;
    let name_node = first_descendant_of_kind(port_id, "simple_identifier")?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_string();

    let ty = {
        let mut cursor = item.walk();
        let mut found = None;
        for child in item.named_children(&mut cursor) {
            match child.kind() {
                "data_type_or_implicit1" | "data_type" | "implicit_data_type1"
                | "data_type_or_implicit" => {
                    found = child
                        .utf8_text(source.as_bytes())
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    break;
                }
                _ => {}
            }
        }
        found
    };

    Some(Param { name, ty })
}

/// Collect `Param` entries (name-only, no types) from a macro definition.
fn collect_macro_params(node: Node<'_>, source: &str) -> Vec<Param> {
    let macro_name = match first_named_child_of_kind(node, "text_macro_name") {
        Some(n) => n,
        None => return Vec::new(),
    };
    let formal_list = match first_named_child_of_kind(macro_name, "list_of_formal_arguments") {
        Some(n) => n,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = formal_list.walk();
    for arg in formal_list.named_children(&mut cursor) {
        if arg.kind() == "formal_argument" {
            if let Some(ident) = first_named_child_of_kind(arg, "simple_identifier") {
                if let Ok(name) = ident.utf8_text(source.as_bytes()) {
                    out.push(Param {
                        name: name.to_string(),
                        ty: None,
                    });
                }
            }
        }
    }
    out
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
            "enum_name_declaration" => SymbolKind::EnumMember,
            // `constraint_declaration` covers in-class constraints;
            // `extern_constraint_declaration` is the out-of-class body.
            // The latter restates the name (`class_scope` + identifier),
            // so we'd double-count if we picked it up — leave it for now.
            "constraint_declaration" => SymbolKind::Constraint,
            "text_macro_definition" => SymbolKind::Macro,
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
        "enum_name_declaration" => {
            // `enum_name_declaration` may carry a value expression
            // (`A = 1`), so its tree contains other identifiers. The
            // name is always the first `enum_identifier` child.
            let id = first_named_child_of_kind(decl, "enum_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        "constraint_declaration" => {
            let id = first_named_child_of_kind(decl, "constraint_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")
        }
        // `` `define MY_MACRO … `` — the macro name is in the
        // `text_macro_name` → `text_macro_identifier` → `simple_identifier`
        // chain.
        "text_macro_definition" => {
            let macro_name = first_named_child_of_kind(decl, "text_macro_name")?;
            let ident      = first_named_child_of_kind(macro_name, "text_macro_identifier")?;
            first_descendant_of_kind(ident, "simple_identifier")
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
pub(crate) fn node_range(node: Node<'_>, rope: &Rope) -> Range {
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
// occurrences_of
// --------------------------------------------------------------------------

/// Return every occurrence of an identifier `name` in `tree`, as LSP ranges.
///
/// Powers `textDocument/documentHighlight`: when the cursor sits on an
/// identifier, the server first calls [`identifier_at`] to grab the name,
/// then this function to find every other place that name appears.
///
/// ## Matching policy
///
/// * **Token-level, not text-level.** We only consider tree-sitter nodes
///   whose `kind()` is `"simple_identifier"` or `"system_tf_identifier"`.
///   String hits inside comments, string literals, or keywords are *not*
///   returned. (Comments aren't in the tree, so they couldn't be even if
///   we wanted; string literals are their own node kind.)
/// * **Full-string equality.** A query for `"foo"` does not match `"foo_bar"`
///   or `"my_foo"` — only the identifier `foo` itself.
/// * **No scoping.** Variables named `x` declared in two different scopes
///   both return. v1 is text-based; future work can add scope-aware
///   matching by routing through a semantic backend.
///
/// Returns an empty `Vec` for an empty `name` (defensive — saves the walk).
#[must_use]
pub fn occurrences_of(tree: &SyntaxTree, rope: &Rope, name: &str) -> Vec<Range> {
    if name.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk_for_occurrences(tree.tree.root_node(), tree.source(), rope, name, &mut out);
    trace!(
        name,
        count = out.len(),
        "collected identifier occurrences"
    );
    out
}

/// Pre-order DFS collector for [`occurrences_of`]. Pushes a range when
/// we hit an identifier-kind node whose source slice equals `name`.
fn walk_for_occurrences(
    node: Node<'_>,
    source: &str,
    rope: &Rope,
    name: &str,
    out: &mut Vec<Range>,
) {
    if matches!(node.kind(), "simple_identifier" | "system_tf_identifier") {
        if source.get(node.byte_range()) == Some(name) {
            out.push(node_range(node, rope));
        }
        // Identifier nodes are leaves — no point descending. Returning
        // here is also a small perf win on long files.
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_occurrences(child, source, rope, name, out);
    }
}

// --------------------------------------------------------------------------
// occurrences_of_at  (scope-aware)
// --------------------------------------------------------------------------

/// Return every occurrence of the identifier under the cursor that
/// resolves to the *same lexical scope* as the cursor's identifier.
///
/// Powers `textDocument/documentHighlight`. Compared to [`occurrences_of`]
/// — which is text-only and matches every token of the same name across
/// the file — this variant climbs the parse tree from `pos` to find the
/// narrowest enclosing scope that locally declares the identifier (e.g.
/// the function body declaring it as a formal argument), then collects
/// only the occurrences inside that scope. Nested scopes that re-declare
/// the same name are skipped, so a `phase` parameter in `build_phase`
/// does not light up alongside an unrelated `phase` parameter in
/// `connect_phase`.
///
/// Falls back to whole-file matching when no enclosing scope declares
/// `name` — that's the right answer for free-standing references whose
/// declaration isn't in the visible structure (e.g. a class's `super.x`
/// where `x` lives in the parent class), and matches the previous
/// text-based behaviour.
///
/// Returns an empty `Vec` when the cursor is not on an identifier.
#[must_use]
pub fn occurrences_of_at(tree: &SyntaxTree, rope: &Rope, pos: Position) -> Vec<Range> {
    let Some(name) = identifier_at(tree, rope, pos) else {
        return Vec::new();
    };
    let name = name.to_owned();
    let Ok(byte) = pos.to_byte_offset(rope) else {
        return Vec::new();
    };
    let root = tree.tree.root_node();
    let leaf = root.descendant_for_byte_range(byte, byte).unwrap_or(root);

    // Walk up: pick the narrowest scope ancestor that declares `name`
    // locally. If none does, search the whole file (matches the legacy
    // text-based behaviour for cross-scope references).
    let mut scope = root;
    let mut cur = Some(leaf);
    while let Some(n) = cur {
        if is_scope_kind(n.kind()) && declares_locally(n, tree.source(), &name) {
            scope = n;
            break;
        }
        cur = n.parent();
    }

    let mut out = Vec::new();
    walk_for_occurrences_scoped(scope, tree.source(), rope, &name, &mut out, true);
    trace!(
        name = %name,
        scope = scope.kind(),
        count = out.len(),
        "collected scope-aware identifier occurrences",
    );
    out
}

/// Tree-sitter node kinds that introduce a new lexical scope in
/// SystemVerilog. Used by [`occurrences_of_at`] both to find the search
/// root and to prune nested scopes that re-declare the same name.
///
/// Both `function_declaration` and `function_body_declaration` are listed:
/// the former wraps the latter in tree-sitter-verilog, and walking up
/// from a leaf inside the body hits the (narrower) body first. Listing
/// both keeps the shadowing-prune step consistent regardless of which
/// the search root happens to be.
fn is_scope_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_body_declaration"
            | "task_body_declaration"
            | "function_declaration"
            | "task_declaration"
            | "class_declaration"
            | "module_declaration"
            | "interface_declaration"
            | "program_declaration"
            | "package_declaration"
            | "seq_block"
            | "initial_construct"
            | "always_construct"
            | "generate_block"
    )
}

/// Does `scope` directly declare an identifier named `name`?
///
/// "Directly" means not via a *nested* scope — a `phase` parameter in an
/// inner function does not count as a declaration inside the outer
/// class. We DFS through `scope`, but stop descending whenever we cross
/// another scope boundary.
fn declares_locally(scope: Node<'_>, source: &str, name: &str) -> bool {
    declares_locally_inner(scope, source, name, true)
}

fn declares_locally_inner(
    node: Node<'_>,
    source: &str,
    name: &str,
    is_root: bool,
) -> bool {
    if !is_root && is_scope_kind(node.kind()) {
        return false;
    }
    if declaration_name_text(node, source) == Some(name) {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if declares_locally_inner(child, source, name, false) {
            return true;
        }
    }
    false
}

/// If `node` is a declaration that introduces an identifier into the
/// surrounding scope, return that identifier's text. Covers the kinds
/// [`name_node_of`] already handles plus `tf_port_item` /
/// `tf_port_item1` (function/task formal arguments — these aren't
/// emitted as `documentSymbol`s but they *are* binders that shadow
/// outer-scope names).
fn declaration_name_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let name_node = match node.kind() {
        "tf_port_item" | "tf_port_item1" => {
            let id = first_named_child_of_kind(node, "port_identifier")?;
            first_descendant_of_kind(id, "simple_identifier")?
        }
        _ => name_node_of(node)?,
    };
    name_node.utf8_text(source.as_bytes()).ok()
}

/// Like [`walk_for_occurrences`] but prunes nested scopes that
/// re-declare `name` (proper shadowing). The root invocation must pass
/// `is_root = true` so the search root itself isn't pruned even when
/// it's the very scope that declares `name`.
fn walk_for_occurrences_scoped(
    node: Node<'_>,
    source: &str,
    rope: &Rope,
    name: &str,
    out: &mut Vec<Range>,
    is_root: bool,
) {
    if !is_root && is_scope_kind(node.kind()) && declares_locally(node, source, name) {
        return;
    }
    if matches!(node.kind(), "simple_identifier" | "system_tf_identifier") {
        if source.get(node.byte_range()) == Some(name) {
            out.push(node_range(node, rope));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_occurrences_scoped(child, source, rope, name, out, false);
    }
}

// --------------------------------------------------------------------------
// prefix_at
// --------------------------------------------------------------------------

/// Return the identifier prefix the user is typing at `pos`.
///
/// Reads the rope line up to `pos.character` (UTF-16 code units) and
/// extracts the trailing `[A-Za-z0-9_$]+` suffix. Rope-based and
/// parse-tree-independent — works even when the document has syntax errors
/// or the tree is stale.
///
/// Returns `Some("")` when the cursor is positioned immediately after a
/// delimiter (e.g. `(`, space, `.`). Returns `None` only when `pos.line`
/// is out of bounds.
#[must_use]
pub fn prefix_at(rope: &Rope, pos: Position) -> Option<String> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line_slice = rope.line(pos.line as usize);

    // Collect chars up to the UTF-16 column, respecting surrogate-pair widths.
    let mut buf = String::new();
    let mut utf16: u32 = 0;
    for ch in line_slice.chars() {
        if ch == '\n' || ch == '\r' || utf16 >= pos.character {
            break;
        }
        buf.push(ch);
        utf16 += ch.len_utf16() as u32;
    }

    // Extract the trailing [A-Za-z0-9_$]* suffix from `buf`.
    // Walk char_indices in reverse; stop at the first non-identifier char.
    let start = buf
        .char_indices()
        .rev()
        .take_while(|(_, ch)| matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$'))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(buf.len());

    Some(buf[start..].to_owned())
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
    fn class_function_is_indexed_once_as_method() {
        // Both `function_declaration` and `function_body_declaration`
        // appear in the tree; we want exactly one symbol per function.
        // A function declared inside a class is a method.
        let s = idx("class c; function void f(); endfunction\nendclass\n");
        let methods: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Method).collect();
        assert_eq!(methods.len(), 1, "expected one method symbol, got {methods:#?}");
        assert_eq!(methods[0].name, "f");
        // No bare `Function` should slip through.
        assert!(!s.iter().any(|sy| sy.kind == SymbolKind::Function));
    }

    #[test]
    fn class_task_is_indexed_once_as_method() {
        let s = idx("class c; task t(); endtask\nendclass\n");
        let methods: Vec<&Symbol> =
            s.iter().filter(|s| s.kind == SymbolKind::Method).collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "t");
        assert!(!s.iter().any(|sy| sy.kind == SymbolKind::Task));
    }

    #[test]
    fn package_function_stays_function_not_method() {
        // Outside any class, `function`/`task` keeps its `Function`/`Task`
        // tag — only class-scoped ones get retagged as `Method`.
        let s = idx("package p;\nfunction int f(); return 0; endfunction\nendpackage\n");
        assert_eq!(pick(&s, "f").kind, SymbolKind::Function);
        assert!(!s.iter().any(|sy| sy.kind == SymbolKind::Method));
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
    fn enum_members_are_indexed() {
        // The typedef alias still wins as the `Typedef`; the enumerator
        // names show up as `EnumMember`s alongside it. Both are needed
        // for F12 — one to jump to `e_t`, others to jump to `READ` /
        // `WRITE`.
        let s = idx("typedef enum { READ, WRITE } e_t;\n");
        assert_eq!(pick(&s, "e_t").kind, SymbolKind::Typedef);
        assert_eq!(pick(&s, "READ").kind, SymbolKind::EnumMember);
        assert_eq!(pick(&s, "WRITE").kind, SymbolKind::EnumMember);
    }

    #[test]
    fn enum_member_with_value_uses_member_name_not_value_ident() {
        // `A = SOME_CONST` parses with `SOME_CONST` as a `simple_identifier`
        // descendant of the enum_name_declaration. We must pick `A`, not
        // the value-expression's identifier.
        let s = idx("typedef enum { A = 1, B = 2 } e_t;\n");
        let members: Vec<&str> = s
            .iter()
            .filter(|s| s.kind == SymbolKind::EnumMember)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(members, vec!["A", "B"]);
    }

    #[test]
    fn constraint_block_is_indexed() {
        let s = idx("class c; rand int x; constraint c1 { x > 0; }\nendclass\n");
        assert_eq!(pick(&s, "c1").kind, SymbolKind::Constraint);
    }

    #[test]
    fn text_macro_definition_is_indexed() {
        let s = idx("`define MY_MACRO 1\n`define ANOTHER_MACRO(x) (x+1)\n");
        assert_eq!(pick(&s, "MY_MACRO").kind, SymbolKind::Macro);
        assert_eq!(pick(&s, "ANOTHER_MACRO").kind, SymbolKind::Macro);
    }

    #[test]
    fn text_macro_inside_module_is_indexed() {
        let s = idx("module m;\n`define LOCAL 42\nendmodule\n");
        assert_eq!(pick(&s, "LOCAL").kind, SymbolKind::Macro);
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

    // ------------------------------------------------------------------
    // prefix_at
    // ------------------------------------------------------------------

    /// Helper: compute prefix at (line, col) in `src`.
    fn pfx(src: &str, line: u32, col: u32) -> Option<String> {
        prefix_at(&Rope::from_str(src), Position::new(line, col))
    }

    #[test]
    fn prefix_at_mid_identifier() {
        // Cursor after "my_cl" in "my_class" → prefix is "my_cl".
        assert_eq!(pfx("my_class foo;", 0, 5), Some("my_cl".into()));
    }

    #[test]
    fn prefix_at_after_space_returns_empty() {
        // Cursor on whitespace (after "class ") → no identifier chars before cursor.
        assert_eq!(pfx("class foo;", 0, 6), Some("".into()));
    }

    #[test]
    fn prefix_at_after_dot_returns_empty() {
        // Cursor right after the `.` in `obj.` → empty prefix.
        assert_eq!(pfx("obj.field", 0, 4), Some("".into()));
    }

    #[test]
    fn prefix_at_full_identifier() {
        // Cursor at end of "my_class" → full name returned.
        assert_eq!(pfx("my_class", 0, 8), Some("my_class".into()));
    }

    #[test]
    fn prefix_at_dollar_prefix() {
        // SystemVerilog system tasks start with `$`.
        assert_eq!(pfx("$disp", 0, 5), Some("$disp".into()));
    }

    #[test]
    fn prefix_at_out_of_bounds_returns_none() {
        assert_eq!(pfx("module foo;", 99, 0), None);
    }

    #[test]
    fn prefix_at_start_of_line_returns_empty() {
        // Cursor at column 0 → nothing before it.
        assert_eq!(pfx("class foo;", 0, 0), Some("".into()));
    }

    // ----------------------------------------------------------------------
    // occurrences_of
    // ----------------------------------------------------------------------

    /// Helper: parse `src` and return all occurrences of `name`.
    fn occ(src: &str, name: &str) -> Vec<Range> {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        occurrences_of(&tree, &Rope::from_str(src), name)
    }

    /// Helper: parse `src` and return scope-aware occurrences for the
    /// identifier under `(line, col)`. Mirrors what
    /// `Backend::document_highlight` does on the wire.
    fn occ_at(src: &str, line: u32, col: u32) -> Vec<Range> {
        init_for_tests();
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let rope = Rope::from_str(src);
        occurrences_of_at(&tree, &rope, Position::new(line, col))
    }

    #[test]
    fn occurrences_finds_all_uses() {
        // `W` appears 3 times: declaration, in `[W-1:0]`, and on the RHS
        // of `assign`.
        let src = "\
module m;
  parameter int W = 8;
  logic [W-1:0] x;
  initial x = W;
endmodule
";
        let hits = occ(src, "W");
        assert_eq!(hits.len(), 3, "expected 3 W occurrences, got {hits:#?}");
    }

    #[test]
    fn occurrences_full_token_only() {
        // A query for "foo" must not match the identifier "foo_bar".
        let src = "\
module m;
  logic foo;
  logic foo_bar;
  initial foo = 1;
endmodule
";
        let foo_hits = occ(src, "foo");
        let foo_bar_hits = occ(src, "foo_bar");
        assert_eq!(
            foo_hits.len(),
            2,
            "expected only the two `foo` (decl + use), got {foo_hits:#?}",
        );
        assert_eq!(foo_bar_hits.len(), 1, "exactly one `foo_bar` decl");
    }

    #[test]
    fn occurrences_unknown_returns_empty() {
        let src = "module m; logic x; endmodule\n";
        assert!(occ(src, "no_such_name").is_empty());
    }

    #[test]
    fn occurrences_empty_name_returns_empty() {
        let src = "module m; logic x; endmodule\n";
        assert!(occ(src, "").is_empty());
    }

    #[test]
    fn occurrences_in_different_scopes_all_match() {
        // v1 is text-based — `x` in two functions both come back.
        // When semantic scoping lands, this test will need to be revised.
        let src = "\
package p;
  function void f();
    int x;
    x = 1;
  endfunction
  function void g();
    int x;
    x = 2;
  endfunction
endpackage
";
        let hits = occ(src, "x");
        assert_eq!(
            hits.len(),
            4,
            "expected 4 x occurrences (2 decls + 2 assigns), got {hits:#?}",
        );
    }

    #[test]
    fn occurrences_at_class_field_spans_whole_class() {
        // `cfg` is a class field. Cursor on the `cfg` reference inside
        // `f` must light up every `cfg` in the class — the field decl
        // and both method uses — because no enclosing function declares
        // `cfg` locally.
        let src = "\
class c;
  int cfg;
  function void f();
    cfg = 1;
  endfunction
  function void g();
    cfg = 2;
  endfunction
endclass
";
        // Line 3, column 5 is inside `cfg` on the `cfg = 1;` line.
        let hits = occ_at(src, 3, 5);
        assert_eq!(
            hits.len(),
            3,
            "expected 3 cfg occurrences (decl + 2 uses), got {hits:#?}",
        );
    }

    #[test]
    fn occurrences_at_skips_shadowing_inner_scope() {
        // `x` is declared at function scope and re-declared inside a
        // begin/end block. With the cursor on the outer `x`, the inner
        // block's `x` (decl + assignment) must be excluded — they bind
        // a different variable.
        let src = "\
module m;
  initial begin
    int x;
    x = 1;
    begin
      int x;
      x = 2;
    end
    x = 3;
  end
endmodule
";
        // Line 3 is `    x = 1;` — column 4 is the `x`.
        let hits = occ_at(src, 3, 4);
        assert_eq!(
            hits.len(),
            3,
            "expected outer `x` decl + 2 outer assigns; inner `x` shadow \
             must be pruned. got {hits:#?}",
        );
    }

    #[test]
    fn occurrences_scope_aware_to_enclosing_function() {
        // Regression for the apb_monitor.sv UVM example: `phase` appears as
        // a parameter in both `build_phase` and `connect_phase`. With the
        // cursor on the `phase` parameter inside `build_phase`, document-
        // highlight must only mark the two references inside that function
        // (the parameter declaration and its assignment), not the
        // unrelated `phase` parameter living in `connect_phase`.
        let src = "\
class c;
  function void build_phase(int phase);
    phase = 1;
  endfunction
  function void connect_phase(int phase);
    phase = 2;
  endfunction
endclass
";
        // Line 1, column 33 is the `h` inside `phase` in `build_phase`'s
        // signature: `  function void build_phase(int phase);`.
        let hits = occ_at(src, 1, 33);
        assert_eq!(
            hits.len(),
            2,
            "expected only the 2 `phase` refs inside build_phase \
             (decl + assignment), got {hits:#?}",
        );
    }

    #[test]
    fn occurrences_system_tf_identifier() {
        // `$display` is a `system_tf_identifier`, not a `simple_identifier`.
        // Confirm our walk includes that kind.
        let src = "\
module m;
  initial begin
    $display(\"a\");
    $display(\"b\");
  end
endmodule
";
        let hits = occ(src, "$display");
        assert_eq!(hits.len(), 2, "expected 2 $display calls, got {hits:#?}");
    }
}
