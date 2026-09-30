//! Syntactic scanner for `uvm_config_db` / `uvm_resource_db` call sites.
//!
//! Powers the custom `mimir/uvmDb` request: the server calls [`db_calls`]
//! on every workspace tree and groups the results into the "who sets what,
//! who gets what" viewer. Everything here is **syntactic** (tree-sitter
//! only, no slang): the scan must work whether or not the sidecar is
//! configured, and a call site is identifiable from the parse tree alone.
//!
//! This is deliberately *not* part of [`crate::uvm`] (that module is UVM
//! lint checks, this one is a call-site scanner) and *not* part of
//! [`crate::calls`] (whose `call_sites_in` only recognises
//! `implicit_class_handle` receivers — `super.` / `this.` — and returns
//! nothing for the `class_type`-receiver static calls handled here).
//!
//! ## Tree-sitter shape recognised
//!
//! Under tree-sitter-systemverilog 0.3.1, both statement-position
//! `uvm_config_db#(int)::set(this, "env.agent", "cfg", 42);` and
//! expression-position `if (!uvm_config_db#(T)::get(...))` parse uniformly
//! as:
//!
//! ```text
//! (method_call
//!   (class_type (simple_identifier)              ; uvm_config_db | uvm_resource_db
//!     (parameter_value_assignment
//!       (list_of_parameter_value_assignments
//!         (ordered_parameter_assignment (param_expression ...)))))  ; #(T)
//!   (method_call_body name: (simple_identifier)  ; set / get / read_by_name / ...
//!     arguments: (list_of_arguments (expression ...) ...)))
//! ```
//!
//! Notes pinned by unit tests below: `this` as an argument is a bare
//! `(primary)` with no children; a string argument descends through
//! `primary_literal → string_literal → quoted_string`, with one
//! `quoted_string_item` per contiguous text segment (an empty `""` has
//! none). The grammar's `arguments:` field on `method_call_body` is
//! unreliable (see `calls.rs`) — we scan named children for
//! `list_of_arguments` instead.

use ropey::Rope;
use tracing::debug;
use tree_sitter::Node;

use mimir_core::Range;

use crate::symbols::node_range;
use crate::SyntaxTree;

// --------------------------------------------------------------------------
// Public types
// --------------------------------------------------------------------------

/// Which UVM database class a call targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DbKind {
    /// `uvm_config_db#(T)::…` — the hierarchical configuration database.
    ConfigDb,
    /// `uvm_resource_db#(T)::…` — the flat resource database underneath it.
    ResourceDb,
}

/// Whether a recognised method writes to or reads from the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbAccess {
    /// `set` / `write_by_name` — publishes a value.
    Write,
    /// `get` / `exists` / `wait_modified` / `read_by_name` / `read_by_type`
    /// — consumes (or observes) a value.
    Read,
}

/// One argument expression inside a db call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbArg {
    /// Raw source text of the argument expression (e.g. `this`, `cfg`,
    /// `"env.agent"` including quotes).
    pub text: String,
    /// Unquoted content when the argument is a string literal
    /// (`Some("")` for an empty `""`); `None` for any other expression
    /// shape (variables, concatenations, `$sformatf(...)`, …).
    pub string_value: Option<String>,
    /// LSP range of the argument expression.
    pub range: Range,
}

/// One `uvm_config_db#(T)::m(...)` / `uvm_resource_db#(T)::m(...)` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbCall {
    /// Which database class the call targets.
    pub kind: DbKind,
    /// The method name as written: `set`, `get`, `exists`, `wait_modified`,
    /// `read_by_name`, `read_by_type`, or `write_by_name`.
    pub method: String,
    /// Read/write classification of [`Self::method`].
    pub access: DbAccess,
    /// Source text of the `#(T)` type parameter (e.g. `virtual apb_if`);
    /// empty when the parameter list is missing or unparseable.
    pub type_param: String,
    /// All argument expressions, in call order.
    pub args: Vec<DbArg>,
    /// config_db only: raw text of the context argument (`this`, `null`, …).
    pub cntxt: Option<String>,
    /// config_db only: instance-path argument when it is a string literal.
    pub inst_name: Option<String>,
    /// The pairing key — config_db `field_name` (arg 2) / resource_db
    /// `name` (arg 1) — when that argument is a string literal. `None` for
    /// non-literal keys and for `read_by_type` (which has no name arg).
    pub key: Option<String>,
    /// resource_db only: scope argument (arg 0) when it is a string literal.
    pub scope: Option<String>,
    /// LSP range of the method-name token — the natural jump target.
    pub name_range: Range,
    /// LSP range of the whole `method_call` node.
    pub call_range: Range,
}

// --------------------------------------------------------------------------
// Public API
// --------------------------------------------------------------------------

/// Collect every `uvm_config_db` / `uvm_resource_db` call in the tree.
///
/// Walks the whole tree; matched calls do **not** stop the descent, so a db
/// call nested inside another call's argument list is still found. Calls
/// with an unrecognised method name (e.g. `uvm_config_db#(int)::dump()`)
/// are skipped, as is any node the grammar shaped differently than the
/// pattern above — the scanner never panics on malformed input, it just
/// omits the call.
#[must_use]
pub fn db_calls(tree: &SyntaxTree, rope: &Rope) -> Vec<DbCall> {
    let source = tree.source();
    let mut out = Vec::new();
    // Iterative walk (see `crate::walk`): try to read a [`DbCall`] out of
    // every `method_call`, and always keep descending — nested db calls
    // live in argument subtrees.
    crate::walk::preorder(tree.tree.root_node(), |node| {
        if !node.is_named() {
            return crate::walk::Walk::Skip;
        }
        if node.kind() == "method_call" {
            if let Some(call) = db_call_from_method_call(node, source, rope) {
                out.push(call);
            }
        }
        crate::walk::Walk::Descend
    });
    debug!(count = out.len(), "db_calls collected");
    out
}

// --------------------------------------------------------------------------
// Internal helpers
// --------------------------------------------------------------------------

/// Classify `method` for `kind`, or `None` when the method isn't part of
/// the recognised db API (which also filters out unrelated classes that
/// happen to share a method name with the db API — the class name check
/// in the caller already handled those).
fn classify(kind: DbKind, method: &str) -> Option<DbAccess> {
    match (kind, method) {
        (DbKind::ConfigDb, "set") => Some(DbAccess::Write),
        (DbKind::ConfigDb, "get" | "exists" | "wait_modified") => Some(DbAccess::Read),
        (DbKind::ResourceDb, "set" | "write_by_name") => Some(DbAccess::Write),
        (DbKind::ResourceDb, "read_by_name" | "read_by_type") => Some(DbAccess::Read),
        _ => None,
    }
}

/// Attempt to read a [`DbCall`] out of one `method_call` node. Every step
/// is `Option`-chained: any deviation from the expected grammar shape
/// (missing class_type, unrecognised class name, absent method body, …)
/// yields `None` rather than a panic.
fn db_call_from_method_call(node: Node<'_>, source: &str, rope: &Rope) -> Option<DbCall> {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();

    // Receiver must be a `class_type` whose head identifier is exactly one
    // of the two db classes. (`super.m(...)` / `this.m(...)` have an
    // `implicit_class_handle` receiver instead and fall out here.)
    let class_type = children.iter().find(|c| c.kind() == "class_type")?;
    let head = first_named_child_of_kind(*class_type, "simple_identifier")?;
    let kind = match head.utf8_text(source.as_bytes()).ok()? {
        "uvm_config_db" => DbKind::ConfigDb,
        "uvm_resource_db" => DbKind::ResourceDb,
        _ => return None,
    };

    // `#(T)`: descend parameter_value_assignment → … → param_expression and
    // take its text. Missing anywhere along the chain → empty string (the
    // call is still reported; a paramless `uvm_config_db::set` that the
    // grammar shapes this way shouldn't vanish from the viewer).
    let type_param = first_named_child_of_kind(*class_type, "parameter_value_assignment")
        .and_then(|pva| first_descendant_of_kind(pva, "param_expression"))
        .and_then(|pe| pe.utf8_text(source.as_bytes()).ok())
        .map(|t| t.trim().to_string())
        .unwrap_or_default();

    // Method name + arguments live on `method_call_body`. The grammar's
    // `arguments:` field is unreliable (returns the `(` token — see
    // calls.rs), so scan named children for `list_of_arguments`. One
    // grammar quirk: `exists` is also SystemVerilog's built-in
    // associative-array method, so `uvm_config_db#(T)::exists(...)` gets a
    // `method_call_body (associative_array_method_call
    // (associative_array_method_name) (list_of_arguments …))` shape — no
    // `name:` field, and the arguments hang off the inner node.
    let body = children.iter().find(|c| c.kind() == "method_call_body")?;
    let (name_node, args_parent) = match body.child_by_field_name("name") {
        Some(n) => (n, *body),
        None => {
            let aamc = first_named_child_of_kind(*body, "associative_array_method_call")?;
            let name = first_named_child_of_kind(aamc, "associative_array_method_name")?;
            (name, aamc)
        }
    };
    let method = name_node.utf8_text(source.as_bytes()).ok()?.to_string();
    let access = classify(kind, &method)?;

    let args: Vec<DbArg> = match first_named_child_of_kind(args_parent, "list_of_arguments") {
        Some(list) => {
            let mut lc = list.walk();
            let mut collected = Vec::new();
            for arg in list.named_children(&mut lc) {
                collected.push(db_arg_from_node(arg, source, rope));
            }
            collected
        }
        None => Vec::new(),
    };

    // Semantic fields by method-specific argument position. `args.get(i)`
    // (never indexing) keeps a malformed `set()` or a short `get(this)`
    // from panicking — the fields just come back `None`.
    let (cntxt, inst_name, key, scope) = match kind {
        DbKind::ConfigDb => (
            args.first().map(|a| a.text.clone()),
            args.get(1).and_then(|a| a.string_value.clone()),
            args.get(2).and_then(|a| a.string_value.clone()),
            None,
        ),
        DbKind::ResourceDb => {
            // set(scope, name, val, accessor) / write_by_name(scope, name,
            // val, accessor) / read_by_name(scope, name, val, accessor);
            // read_by_type(scope, val, accessor) has no name argument.
            let key = if method == "read_by_type" {
                None
            } else {
                args.get(1).and_then(|a| a.string_value.clone())
            };
            (
                None,
                None,
                key,
                args.first().and_then(|a| a.string_value.clone()),
            )
        }
    };

    Some(DbCall {
        kind,
        method,
        access,
        type_param,
        args,
        cntxt,
        inst_name,
        key,
        scope,
        name_range: node_range(name_node, rope),
        call_range: node_range(node, rope),
    })
}

/// Build a [`DbArg`] from one named child of `list_of_arguments`.
fn db_arg_from_node(node: Node<'_>, source: &str, rope: &Rope) -> DbArg {
    let text = node
        .utf8_text(source.as_bytes())
        .unwrap_or_default()
        .to_string();
    DbArg {
        text,
        string_value: string_literal_value(node, source),
        range: node_range(node, rope),
    }
}

/// Unquote a string-literal argument: descend to `quoted_string` and
/// concatenate its `quoted_string_item` children (zero items ⇒ empty
/// string, i.e. `""` yields `Some("")`). Returns `None` when the argument
/// isn't a plain string literal. Escapes are kept raw — the viewer shows
/// keys verbatim, it doesn't interpret them.
fn string_literal_value(node: Node<'_>, source: &str) -> Option<String> {
    let quoted = first_descendant_of_kind(node, "quoted_string")?;
    // A quoted_string nested under anything but a lone literal expression
    // (e.g. inside a concat `{"a", b}` or a `$sformatf(...)` call) must not
    // count as "this argument is a string literal". A plain literal's text
    // is exactly the quoted string itself.
    let node_text = node.utf8_text(source.as_bytes()).ok()?;
    let quoted_text = quoted.utf8_text(source.as_bytes()).ok()?;
    if node_text.trim() != quoted_text {
        return None;
    }
    let mut cursor = quoted.walk();
    let value = quoted
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "quoted_string_item")
        .filter_map(|c| c.utf8_text(source.as_bytes()).ok())
        .collect::<String>();
    Some(value)
}

/// First *named* direct child of `node` with the given kind.
///
/// Indexed access instead of a `named_children(&mut cursor)` iterator: the
/// iterator borrows the cursor, and returning a `Node` found through it
/// trips E0597 (the temporary iterator outlives the local cursor).
fn first_named_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .find(|c| c.kind() == kind)
}

/// Pre-order search for the first descendant (including `node` itself) of
/// the given kind.
fn first_descendant_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    crate::walk::first_descendant_of_kind(node, kind)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxParser;
    use mimir_core::logging::init_for_tests;

    /// Parse `body` inside a class-method shell and scan it.
    fn scan(body: &str) -> Vec<DbCall> {
        init_for_tests();
        let src = format!(
            "class c;\n  function void f();\n{body}\n  endfunction\nendclass\n"
        );
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(&src, None).expect("parse");
        let rope = Rope::from_str(&src);
        db_calls(&tree, &rope)
    }

    #[test]
    fn statement_position_set_is_found() {
        let calls = scan(r#"    uvm_config_db#(int)::set(this, "env.agent", "cfg", 42);"#);
        assert_eq!(calls.len(), 1, "got {calls:?}");
        let c = &calls[0];
        assert_eq!(c.kind, DbKind::ConfigDb);
        assert_eq!(c.method, "set");
        assert_eq!(c.access, DbAccess::Write);
        assert_eq!(c.type_param, "int");
        assert_eq!(c.args.len(), 4);
        assert_eq!(c.cntxt.as_deref(), Some("this"));
        assert_eq!(c.inst_name.as_deref(), Some("env.agent"));
        assert_eq!(c.key.as_deref(), Some("cfg"));
        assert_eq!(c.scope, None);
        // name_range covers exactly the `set` token on the fixture line.
        assert_eq!(c.name_range.start.line, 2);
        assert_eq!(
            c.name_range.end.character - c.name_range.start.character,
            "set".len() as u32
        );
    }

    #[test]
    fn expression_position_get_with_empty_inst_name() {
        let calls = scan(
            r#"    if (!uvm_config_db#(virtual apb_if)::get(this, "", "vif", vif)) begin
    end"#,
        );
        assert_eq!(calls.len(), 1, "got {calls:?}");
        let c = &calls[0];
        assert_eq!(c.method, "get");
        assert_eq!(c.access, DbAccess::Read);
        assert_eq!(c.type_param, "virtual apb_if");
        // Empty `""` is a quoted_string with no quoted_string_item children;
        // it must still unquote to Some("").
        assert_eq!(c.inst_name.as_deref(), Some(""));
        assert_eq!(c.key.as_deref(), Some("vif"));
    }

    #[test]
    fn non_literal_key_yields_none_key_but_keeps_text() {
        let calls = scan(r#"    void'(uvm_config_db#(int)::get(this, "", field, x));"#);
        assert_eq!(calls.len(), 1, "got {calls:?}");
        let c = &calls[0];
        assert_eq!(c.key, None);
        assert_eq!(c.args[2].text, "field");
        assert_eq!(c.args[2].string_value, None);
    }

    #[test]
    fn short_arg_lists_do_not_panic() {
        // A truncated `get` still reports what it has; a bare `set()` has
        // no semantic fields at all. Neither may panic.
        let calls = scan(
            r#"    void'(uvm_config_db#(int)::get(this, "", "cfg"));
    uvm_config_db#(int)::set();"#,
        );
        assert_eq!(calls.len(), 2, "got {calls:?}");
        assert_eq!(calls[0].key.as_deref(), Some("cfg"));
        assert_eq!(calls[1].args.len(), 0);
        assert_eq!(calls[1].cntxt, None);
        assert_eq!(calls[1].key, None);
    }

    #[test]
    fn resource_db_methods_and_fields() {
        let calls = scan(
            r#"    uvm_resource_db#(int)::set("scope", "name", 7, this);
    void'(uvm_resource_db#(int)::read_by_name("scope", "name", tmp));
    void'(uvm_resource_db#(int)::read_by_type("scope", tmp, this));"#,
        );
        assert_eq!(calls.len(), 3, "got {calls:?}");

        let set = &calls[0];
        assert_eq!(set.kind, DbKind::ResourceDb);
        assert_eq!(set.access, DbAccess::Write);
        assert_eq!(set.scope.as_deref(), Some("scope"));
        assert_eq!(set.key.as_deref(), Some("name"));
        assert_eq!(set.cntxt, None);
        assert_eq!(set.inst_name, None);

        let read = &calls[1];
        assert_eq!(read.access, DbAccess::Read);
        assert_eq!(read.key.as_deref(), Some("name"));

        // read_by_type has no name argument: key must stay None even though
        // arg 1 exists (it's the value output, not a name).
        let by_type = &calls[2];
        assert_eq!(by_type.key, None);
        assert_eq!(by_type.scope.as_deref(), Some("scope"));
    }

    #[test]
    fn unrelated_calls_are_ignored() {
        let calls = scan(
            r#"    my_config_db#(int)::set(this, "a", "b", 1);
    other_cls::set(a);
    set(x);
    this.set(x);"#,
        );
        assert!(calls.is_empty(), "expected no db calls, got {calls:?}");
    }

    #[test]
    fn nested_db_calls_are_found() {
        let calls = scan(
            r#"    foo(uvm_config_db#(int)::exists(this, "", "k"));
    uvm_config_db#(int)::set(this, "", "k", compute(a, b));"#,
        );
        assert_eq!(calls.len(), 2, "got {calls:?}");
        assert_eq!(calls[0].method, "exists");
        assert_eq!(calls[0].access, DbAccess::Read);
        assert_eq!(calls[1].method, "set");
    }

    #[test]
    fn unrecognized_method_is_skipped() {
        let calls = scan(r#"    uvm_config_db#(int)::dump();"#);
        assert!(calls.is_empty(), "expected dump() skipped, got {calls:?}");
    }

    #[test]
    fn access_classification_covers_all_methods() {
        let calls = scan(
            r#"    void'(uvm_config_db#(int)::exists(this, "", "k"));
    uvm_config_db#(int)::wait_modified(this, "", "k");
    uvm_resource_db#(int)::write_by_name("s", "n", 1, this);"#,
        );
        assert_eq!(calls.len(), 3, "got {calls:?}");
        assert_eq!(calls[0].access, DbAccess::Read);
        assert_eq!(calls[1].access, DbAccess::Read);
        assert_eq!(calls[2].access, DbAccess::Write);
    }

    #[test]
    fn concat_key_is_not_a_string_literal() {
        // `{"a", b}` contains a quoted_string but is not itself a string
        // literal — the key must come back None (lands in "ungrouped").
        let calls = scan(r#"    uvm_config_db#(int)::set(this, "", {"a", b}, 1);"#);
        assert_eq!(calls.len(), 1, "got {calls:?}");
        assert_eq!(calls[0].key, None);
    }
}
