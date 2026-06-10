//! AST-driven method-call resolution and member completion.
//!
//! The tree-sitter fallback for `this.X` / `super.X` / `obj.X` / multi-hop
//! receiver chains: [`resolve_method_symbol`] turns a call site into a
//! [`Symbol`] for inlay hints and signature help, and
//! [`syntax_member_completion`] enumerates a receiver's members for the
//! completion popup. Used when slang is unavailable or busy; the slang
//! reference map takes priority in the handlers.

use std::collections::HashSet;

use mimir_core::Position as MPosition;
use mimir_syntax::{Symbol, SymbolKind as MSymbolKind, SyntaxTree};
use ropey::Rope;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Documentation, Url,
};
use tracing::debug;

use crate::chain_resolve;
use crate::lsp_convert::{make_resolve_data, symbol_kind_to_completion_kind};
use crate::workspace_index::{self, WorkspaceIndex};

/// Outcome of the AST-driven method-call resolver. `Resolved(sym, via)`
/// carries the resolved symbol plus a short human-readable tag for the
/// route taken (used by trace logs). `NotResolved(reason)` is also
/// human-readable.
pub(crate) enum MethodResolution {
    Resolved(Symbol, &'static str),
    NotResolved(&'static str),
}

/// Resolve a method-call site to a `Symbol` via the AST, without slang.
///
/// `recv` is the receiver text as the call-site builder stored it —
/// "super", "this", "" (for `class_new`), or a `hierarchical_identifier`
/// for `obj.method` chains.
///
/// Routes:
///   * `recv == "this"`  → look up `call.name` as a Method in the enclosing
///     class's same-file index entries.
///   * `recv == "super"` → walk the enclosing class's `extends` chain via
///     [`chain_resolve::find_method_in_class`].
///   * `recv == ""`      → constructor-expression form (`class_new`); use
///     [`mimir_syntax::symbols::class_new_lhs_at`] to find the LHS context,
///     then look up `"new"` in the resolved class.
///   * otherwise        → `obj.method`-style; the receiver is a
///     `hierarchical_identifier` that includes the method name, so we
///     strip the trailing segment, refuse chained receivers (`obj.field`
///     would need slang), feed the bare identifier through
///     [`mimir_syntax::symbols::find_variable_type_at`] to get its
///     declared type, normalize, and resolve.
pub(crate) fn resolve_method_symbol(
    call: &mimir_syntax::calls::CallSite,
    recv: &str,
    tree: &mimir_syntax::SyntaxTree,
    rope: &Rope,
    same_file_index: &[Symbol],
    wi: &workspace_index::WorkspaceIndex,
) -> MethodResolution {
    use mimir_syntax::symbols::{
        class_new_lhs_at, enclosing_class_info_at, find_variable_type_at, normalize_type_name,
        ClassNewLhs,
    };

    match recv {
        "this" => {
            // Same-file methods of the enclosing class. The same-file index
            // already only contains symbols from this document; for typical
            // single-class files (apb_monitor, packet, …) name lookup is
            // sufficient.
            let _ = enclosing_class_info_at(tree, rope, call.name_range.start);
            same_file_index
                .iter()
                .find(|s| s.name == call.name && s.kind == MSymbolKind::Method)
                .cloned()
                .map(|s| MethodResolution::Resolved(s, "this/same-file"))
                .unwrap_or(MethodResolution::NotResolved(
                    "this.X not in same-file index",
                ))
        }
        "super" => {
            let info = enclosing_class_info_at(tree, rope, call.name_range.start);
            let Some(parent) = info.and_then(|i| i.parent_class_name) else {
                return MethodResolution::NotResolved("super used but no extends clause");
            };
            chain_resolve::find_method_in_class(wi, &parent, &call.name)
                .map(|(_, s)| MethodResolution::Resolved(s, "super/inheritance walk"))
                .unwrap_or(MethodResolution::NotResolved(
                    "super.X not found in any ancestor",
                ))
        }
        "" => {
            // `class_new` expression — find the LHS context.
            let ctx = match class_new_lhs_at(tree, rope, call.name_range.start) {
                Some(c) => c,
                None => {
                    return MethodResolution::NotResolved(
                        "class_new not in a recognised assignment shape",
                    )
                }
            };
            let target_class = match ctx {
                ClassNewLhs::DeclaredType(ty) => normalize_type_name(&ty),
                ClassNewLhs::LhsName(name) => {
                    find_variable_type_at(tree, rope, call.name_range.start, &name)
                        .as_deref()
                        .and_then(normalize_type_name)
                }
            };
            let Some(cls) = target_class else {
                return MethodResolution::NotResolved("class_new LHS type unresolvable from AST");
            };
            chain_resolve::find_method_in_class(wi, &cls, "new")
                .map(|(_, s)| MethodResolution::Resolved(s, "class_new/LHS-type"))
                .unwrap_or(MethodResolution::NotResolved(
                    "constructor not found for resolved class",
                ))
        }
        _ => {
            // `obj.method` or `a.b.method` style. `recv` is the whole
            // hierarchical_identifier including the method name. Strip the
            // trailing segment to get the receiver chain, then build a
            // MemberChain and resolve with the chain resolver (supports up to
            // 2 intermediate hops on the tree-sitter path).
            let receiver_chain = match recv.rsplit_once('.') {
                Some((before, _method)) => before,
                None => recv,
            };
            let chain = chain_resolve::build_chain_for_receiver(receiver_chain, &call.name);
            if let Some((_, sym)) = chain_resolve::resolve_member_chain(
                &chain, call.name_range.start, tree, rope, wi,
            ) {
                return MethodResolution::Resolved(sym, "obj.method/chain");
            }
            // Single-segment receiver fast path for built-in methods.
            let cls_opt = if !receiver_chain.contains('.') {
                find_variable_type_at(tree, rope, call.name_range.start, receiver_chain)
                    .as_deref()
                    .and_then(normalize_type_name)
                    .map(|s| s.to_string())
            } else {
                None
            };
            if let Some(cls) = cls_opt {
                if let Some(m) = mimir_syntax::builtin_methods::find_method(&cls, &call.name)
                    .or_else(|| mimir_syntax::builtin_methods::find_universal(&call.name))
                {
                    return MethodResolution::Resolved(
                        builtin_to_symbol(m, call.name_range),
                        "obj.method/builtin",
                    );
                }
            }
            MethodResolution::NotResolved(
                "method not found in resolved receiver class",
            )
        }
    }
}

/// Synthesise a [`Symbol`] from a [`mimir_syntax::builtin_methods::BuiltinMethod`]
/// so it can be passed to [`mimir_syntax::inlay::hints_for`] and
/// [`mimir_syntax::signature::signature_for`].
///
/// The `name_range` is taken from the call site so the symbol has a
/// plausible source location; `full_range` matches it (we have no
/// declaration site for built-ins).
pub(crate) fn builtin_to_symbol(
    m: &mimir_syntax::builtin_methods::BuiltinMethod,
    range: mimir_core::Range,
) -> Symbol {
    Symbol {
        name: m.name.to_owned(),
        kind: mimir_syntax::SymbolKind::Method,
        name_range: range,
        full_range: range,
        params: Some(
            m.params
                .iter()
                .map(|p| mimir_syntax::symbols::Param {
                    name: p.name.to_owned(),
                    ty: p.ty.map(str::to_owned),
                })
                .collect(),
        ),
        parent_class_name: None,
        return_type: None,
        decl_type: None,
    }
}

/// Synthesise a callable [`Symbol`] from `(name, type)` params resolved via
/// the slang reference map, so it can feed
/// [`mimir_syntax::inlay::hints_for`], which reads
/// only `params`; the ranges are taken from the call site for plausibility.
pub(crate) fn synth_method_symbol(
    call: &mimir_syntax::calls::CallSite,
    params: Vec<(String, Option<String>)>,
) -> Symbol {
    Symbol {
        name: call.name.clone(),
        kind: mimir_syntax::SymbolKind::Method,
        name_range: call.name_range,
        full_range: call.name_range,
        params: Some(
            params
                .into_iter()
                .map(|(name, ty)| mimir_syntax::symbols::Param { name, ty })
                .collect(),
        ),
        parent_class_name: None,
        return_type: None,
        decl_type: None,
    }
}

// --------------------------------------------------------------------------
// Syntax-only member completion (AST fallback for `super.` / `this.` / `obj.` / chains)
// --------------------------------------------------------------------------

/// Parse the receiver chain immediately before the `.` trigger.
///
/// For `a.b.` at cursor returns `["a", "b"]`; for `obj.` returns `["obj"]`;
/// for `super.fo` (partial prefix typed) returns `["super"]`. Returns
/// `None` when the trigger is `::` (package scope) or when a
/// non-identifier character (e.g. `)` from a chained call like
/// `get_obj().`) sits left of a dot.
///
/// Built on the shared cursor-context scanners in
/// [`mimir_syntax::symbols`] (`line_prefix_at` / `trailing_ident_start`).
pub(crate) fn receiver_chain_before_dot(rope: &Rope, pos: MPosition) -> Option<Vec<String>> {
    let buf = mimir_syntax::symbols::line_prefix_at(rope, pos)?;

    // Strip the completion prefix (e.g. "fo" in "obj.fo"); only handle a
    // `.` trigger — `::` is package scope, handled elsewhere.
    let mut rest = &buf[..mimir_syntax::symbols::trailing_ident_start(&buf)];
    if !rest.ends_with('.') {
        return None;
    }

    // Read segments backwards, stopping at a non-identifier non-dot char.
    let mut segments: Vec<String> = Vec::new();
    loop {
        rest = &rest[..rest.len() - 1]; // consume the `.`
        let start = mimir_syntax::symbols::trailing_ident_start(rest);
        if start == rest.len() {
            return None; // non-identifier char (e.g. `)`) — bail
        }
        segments.push(rest[start..].to_owned());
        rest = &rest[..start];
        if !rest.ends_with('.') {
            break;
        }
    }
    segments.reverse();
    Some(segments)
}

/// Enumerate all member symbols declared in `class_name` and its ancestors
/// (via `Symbol::parent_class_name`). Capped at 16 hops to guard against
/// cycles in malformed code.
///
/// Closest-ancestor wins: if a subclass overrides a parent's method, only
/// the subclass version is included (matching SV override semantics).
///
/// Returns `(Symbol, Url)` pairs so callers can show the declaring file name
/// in the completion item's `detail` field.
pub(crate) fn collect_class_members(
    wi: &WorkspaceIndex,
    class_name: &str,
) -> Vec<(Symbol, Url)> {
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut result: Vec<(Symbol, Url)> = Vec::new();
    let mut current = class_name.to_string();
    let mut visited: HashSet<String> = HashSet::new();

    for _ in 0..16 {
        if !visited.insert(current.clone()) {
            break;
        }
        let Some(class_entry) = wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == MSymbolKind::Class)
            .cloned()
        else {
            break;
        };
        let class_url = class_entry.url.clone();
        let class_range = class_entry.symbol.full_range;

        for e in wi.entries() {
            if e.url != class_url {
                continue;
            }
            if e.symbol.kind == MSymbolKind::Class {
                continue;
            }
            if !class_range.contains_range(e.symbol.full_range) {
                continue;
            }
            if seen_names.insert(e.symbol.name.clone()) {
                result.push((e.symbol.clone(), class_url.clone()));
            }
        }

        match class_entry.symbol.parent_class_name {
            Some(parent) => current = parent,
            None => break,
        }
    }
    result
}

/// Best-effort member completion backed by the cached AST and workspace
/// index. Used when slang is unavailable or busy with a background elaborate.
///
/// Handles single-hop and multi-hop receiver chains:
/// - `super` → members of the parent class (from `extends` on the enclosing class)
/// - `this`  → members of the enclosing class and its ancestors
/// - `<ident>` → resolves the identifier's declared type, then enumerates members
/// - `a.b.` → resolves `a` to a type, then `b` to a member type, then enumerates
///   members of that type (up to 2 intermediate hops on the tree-sitter path)
///
/// Returns `None` when the receiver's type cannot be determined from syntax
/// alone (e.g. undeclared variable, deeper chain). This avoids the workspace-
/// dump anti-pattern — no irrelevant candidates are ever returned.
pub(crate) fn syntax_member_completion(
    wi: &WorkspaceIndex,
    tree: &SyntaxTree,
    rope: &Rope,
    pos: MPosition,
    prefix: &str,
) -> Option<CompletionResponse> {
    let segments = receiver_chain_before_dot(rope, pos)?;

    // `dim_suffix` carries `"[$]"`, `"[]"`, or `"[K]"` when the receiver is a
    // queue / dynamic array / associative array so we can append the right
    // built-in table after workspace members.
    let mut dim_suffix: Option<String> = None;

    let class_name: String = if segments.len() == 1 {
        match segments[0].as_str() {
            "super" => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?;
                info.parent_class_name?
            }
            "this" => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?;
                info.class_name
            }
            ident => {
                let type_info =
                    mimir_syntax::symbols::find_variable_type_info_at(tree, rope, pos, ident)?;
                dim_suffix = type_info.suffix.clone();
                mimir_syntax::symbols::normalize_type_name(&type_info.base)?
            }
        }
    } else {
        // Multi-hop: walk the receiver segments manually to find the type at
        // the end of the chain, then enumerate that type's members.
        let root_name = &segments[0];
        let root_type = match root_name.as_str() {
            "this" => mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?.class_name,
            "super" => {
                mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?.parent_class_name?
            }
            _ => {
                let raw =
                    mimir_syntax::symbols::find_variable_type_at(tree, rope, pos, root_name)?;
                mimir_syntax::symbols::normalize_type_name(&raw)?
            }
        };
        let mut current_type = root_type;
        for seg in &segments[1..] {
            let (_, sym) = chain_resolve::find_member(wi, &current_type, seg)?;
            let raw = sym.decl_type.as_deref().or(sym.return_type.as_deref())?;
            current_type = mimir_syntax::symbols::normalize_type_name(raw)?;
        }
        current_type
    };

    let workspace_members = collect_class_members(wi, &class_name);
    let builtins = mimir_syntax::builtin_methods::methods_for_type(&class_name);
    let universals = mimir_syntax::builtin_methods::universal_methods();
    // Only return None when there is truly nothing to offer — workspace
    // members, type-specific builtins, AND universal methods are all empty.
    // (universals is never empty in practice, but guard explicitly.)
    if workspace_members.is_empty() && builtins.is_empty() && universals.is_empty() {
        return None;
    }

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut items: Vec<CompletionItem> = workspace_members
        .into_iter()
        .filter(|(s, _)| {
            prefix_lower.is_empty() || s.name.to_ascii_lowercase().starts_with(&prefix_lower)
        })
        .map(|(sym, url)| {
            let detail = url
                .path_segments()
                .and_then(|mut segs| segs.next_back())
                .map(str::to_owned);
            CompletionItem {
                label: sym.name.clone(),
                kind: Some(symbol_kind_to_completion_kind(sym.kind)),
                detail,
                data: make_resolve_data(&url, sym.name_range.start.line),
                ..Default::default()
            }
        })
        .collect();

    // Helper: append a builtin slice, deduplicating against existing items.
    let append_builtins = |items: &mut Vec<CompletionItem>,
                           table: &'static [mimir_syntax::builtin_methods::BuiltinMethod],
                           prefix_lower: &str| {
        for m in table {
            if !prefix_lower.is_empty()
                && !m.name.to_ascii_lowercase().starts_with(prefix_lower)
            {
                continue;
            }
            if items.iter().any(|i| i.label == m.name) {
                continue;
            }
            items.push(CompletionItem {
                label: m.name.to_owned(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some("built-in".to_owned()),
                documentation: Some(Documentation::String(m.doc.to_owned())),
                ..Default::default()
            });
        }
    };

    // Type-specific built-ins (e.g. string methods). Workspace wins on collision.
    append_builtins(&mut items, builtins, &prefix_lower);
    // Dimension-based built-ins: queue / dynamic-array / associative-array methods.
    if let Some(sfx) = dim_suffix.as_deref() {
        append_builtins(
            &mut items,
            mimir_syntax::builtin_methods::methods_for_suffix(sfx),
            &prefix_lower,
        );
    }
    // Universal methods (rand_mode, constraint_mode, randomize) on any class.
    append_builtins(&mut items, universals, &prefix_lower);

    debug!(
        class = %class_name,
        receiver = ?segments,
        count = items.len(),
        "member completion: syntax fallback",
    );
    Some(CompletionResponse::Array(items))
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::Range as MRange;
    use mimir_syntax::SyntaxParser;
    use ropey::Rope;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Parse `text` and return the tree (panics on parser init failure).
    fn parse_tree(text: &str) -> SyntaxTree {
        let mut p = SyntaxParser::new().expect("parser init");
        p.parse(text, None).expect("parse")
    }

    /// Build a workspace index over `(url, source)` pairs.
    fn workspace_index_from(files: &[(&Url, &str)]) -> WorkspaceIndex {
        let mut wi = WorkspaceIndex::default();
        for (u, text) in files {
            let tree = parse_tree(text);
            let rope = Rope::from_str(tree.source());
            let syms = mimir_syntax::symbols::index(&tree, &rope);
            wi.update((*u).clone(), &syms);
        }
        wi
    }

    // ----------------------------------------------------------------------
    // syntax_member_completion helpers
    // ----------------------------------------------------------------------

    /// `receiver_chain_before_dot` extracts the identifier(s) left of the `.`.
    #[test]
    fn receiver_chain_before_dot_super() {
        let rope = Rope::from_str("    super.");
        // cursor after the dot (col 10)
        let pos = MPosition::new(0, 10);
        assert_eq!(
            receiver_chain_before_dot(&rope, pos),
            Some(vec!["super".to_string()]),
        );
    }


    #[test]
    fn receiver_chain_before_dot_with_prefix() {
        // partial prefix: "obj.fo" — cursor at end
        let rope = Rope::from_str("obj.fo");
        let pos = MPosition::new(0, 6);
        assert_eq!(
            receiver_chain_before_dot(&rope, pos),
            Some(vec!["obj".to_string()]),
        );
    }


    #[test]
    fn receiver_chain_before_dot_chained_call_returns_none() {
        // "get_obj()." — nothing plain before the dot
        let rope = Rope::from_str("get_obj().");
        let pos = MPosition::new(0, 10);
        assert!(receiver_chain_before_dot(&rope, pos).is_none());
    }


    #[test]
    fn receiver_chain_before_dot_scope_trigger_returns_none() {
        // "::" is not a `.` trigger
        let rope = Rope::from_str("pkg::");
        let pos = MPosition::new(0, 5);
        assert!(receiver_chain_before_dot(&rope, pos).is_none());
    }


    /// `collect_class_members` returns all symbols inside a class body,
    /// walks the `extends` chain, and closest-ancestor wins on name collision.
    #[test]
    fn collect_class_members_single_class() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void foo(); endfunction
  int bar;
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let members = collect_class_members(&wi, "Base");
        let names: Vec<&str> = members.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"foo"), "should include method foo");
        assert!(names.contains(&"bar"), "should include field bar");
        assert!(!names.contains(&"Base"), "should not include the class itself");
    }


    #[test]
    fn collect_class_members_walks_extends_chain() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void base_fn(); endfunction
endclass
class Child extends Base;
  function void child_fn(); endfunction
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let members = collect_class_members(&wi, "Child");
        let names: Vec<&str> = members.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"child_fn"), "own method");
        assert!(names.contains(&"base_fn"), "inherited method");
    }


    #[test]
    fn collect_class_members_override_deduplication() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void run(); endfunction
endclass
class Child extends Base;
  function void run(); endfunction
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let members = collect_class_members(&wi, "Child");
        let run_count = members.iter().filter(|(s, _)| s.name == "run").count();
        assert_eq!(run_count, 1, "overridden method should appear only once");
    }


    /// `syntax_member_completion` returns candidates for `super.` when the
    /// enclosing class has a known parent.
    #[test]
    fn syntax_member_completion_super() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void base_method(); endfunction
endclass
class Child extends Base;
  function void my_fn();
    super.
  endfunction
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let tree = parse_tree(src);
        let rope = Rope::from_str(src);
        // Line 5 (0-indexed): "    super." — cursor after the dot at col 10.
        let pos = MPosition::new(5, 10);
        let resp = syntax_member_completion(&wi, &tree, &rope, pos, "");
        assert!(resp.is_some(), "should return Some for super.");
        if let Some(CompletionResponse::Array(items)) = resp {
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"base_method"), "parent method should appear");
        }
    }


    /// `syntax_member_completion` always offers universal methods (rand_mode,
    /// constraint_mode, randomize) on any class receiver, even when the
    /// class has no workspace-indexed members.
    #[test]
    fn syntax_member_completion_universal_methods_on_any_class() {
        // Index has MyClass but no members — universal methods must still appear.
        let url = Url::parse("file:///test/my.sv").unwrap();
        let mut wi = WorkspaceIndex::default();
        wi.update(url, &[Symbol {
            name: "MyClass".to_string(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 7)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(5, 0)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        }]);

        let src = "class wrapper;\n  MyClass obj;\n  function void run();\n    obj.\n  endfunction\nendclass\n";
        let tree = parse_tree(src);
        let rope = Rope::from_str(src);
        // Line 3 (0-indexed): "    obj." — cursor after the dot.
        let pos = MPosition::new(3, 8);
        let resp = syntax_member_completion(&wi, &tree, &rope, pos, "");
        assert!(resp.is_some(), "universal methods should make Some");
        if let Some(CompletionResponse::Array(items)) = resp {
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"rand_mode"), "rand_mode should appear");
            assert!(labels.contains(&"constraint_mode"), "constraint_mode should appear");
            assert!(labels.contains(&"randomize"), "randomize should appear");
        }
    }


    /// `syntax_member_completion` returns `None` for an unknown receiver
    /// (undeclared variable) — no workspace dump.
    #[test]
    fn syntax_member_completion_unknown_receiver_returns_none() {
        let wi = WorkspaceIndex::default();
        let src = "module m; initial unknown_var. endmodule\n";
        let tree = parse_tree(src);
        let rope = Rope::from_str(src);
        let pos = MPosition::new(0, 30);
        assert!(
            syntax_member_completion(&wi, &tree, &rope, pos, "").is_none(),
            "undeclared variable should return None"
        );
    }
}
