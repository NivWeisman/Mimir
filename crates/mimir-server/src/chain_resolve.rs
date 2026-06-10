//! Class member lookup and multi-hop chain resolution.
//!
//! This module owns the canonical implementations of class-member lookup
//! (previously scattered in `backend.rs`) and extends them with a general
//! chain resolver that supports up to 2 intermediate hops on the
//! tree-sitter path. Slang resolves chains itself and never calls into
//! this module.
//!
//! ## Backend-agnosticism
//!
//! All imports are from `mimir_syntax`, `mimir_core`, the workspace index,
//! and `url`. There are **no** imports from `mimir_slang` or `mimir_ast`.
//! When slang is replaced by a future backend, only `slang_adapter.rs` and
//! `ast_features.rs` change — this module is unaffected.

use mimir_core::Position;
use mimir_syntax::{
    symbols::{
        enclosing_class_info_at, find_variable_type_at, normalize_type_name, ChainSegment,
        MemberChain,
    },
    SymbolKind as MSymbolKind, SyntaxTree,
};
use mimir_syntax::Symbol;
use ropey::Rope;
use tower_lsp::lsp_types::Url;
use tracing::{debug, trace};

use crate::workspace_index::WorkspaceIndex;

// --------------------------------------------------------------------------
// find_method_in_class / find_field_in_class
// (moved from backend.rs; signatures upgraded to return the file URL)
// --------------------------------------------------------------------------

/// Maximum number of `extends` hops any inheritance walk will follow.
/// Real UVM hierarchies are 5–8 deep; 16 catches runaway chains long
/// before the walk gets expensive.
const MAX_INHERITANCE_HOPS: usize = 16;

/// Shared inheritance-walk core behind [`find_method_in_class`],
/// [`find_field_in_class`], and the code-lens override lookup: starting at
/// `class_name`, look for `member_name` declared inside each class body
/// (filtered by `kind_matches`), ascending via
/// [`Symbol::parent_class_name`] on a miss.
///
/// Returns `(declaring_class, file_url, symbol)` of the nearest match.
/// Caps at [`MAX_INHERITANCE_HOPS`] and cycle-detects via a visited set.
/// `what` is a label for the trace logs ("method" / "field" / "override").
pub(crate) fn find_member_in_class_chain(
    wi: &WorkspaceIndex,
    class_name: &str,
    member_name: &str,
    what: &'static str,
    kind_matches: impl Fn(MSymbolKind) -> bool,
) -> Option<(String, Url, Symbol)> {
    let mut current = class_name.to_string();
    let mut visited = std::collections::HashSet::new();
    for hop in 0..MAX_INHERITANCE_HOPS {
        if !visited.insert(current.clone()) {
            debug!(class = %current, hop, what, "member lookup: inheritance cycle detected");
            return None;
        }
        let class_entry = match wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == MSymbolKind::Class)
            .cloned()
        {
            Some(e) => e,
            None => {
                debug!(
                    class = %current,
                    member = %member_name,
                    hop,
                    what,
                    "member lookup: class not in workspace index",
                );
                return None;
            }
        };
        if let Some(entry) = wi.lookup(member_name).iter().find(|e| {
            e.url == class_entry.url
                && class_entry.symbol.full_range.contains_range(e.symbol.full_range)
                && kind_matches(e.symbol.kind)
        }) {
            debug!(
                class = %current,
                member = %member_name,
                hop,
                what,
                url = %entry.url,
                kind = ?entry.symbol.kind,
                "member lookup: hit",
            );
            return Some((current, entry.url.clone(), entry.symbol.clone()));
        }
        match class_entry.symbol.parent_class_name {
            Some(parent) => {
                trace!(
                    from = %current,
                    parent = %parent,
                    member = %member_name,
                    hop,
                    what,
                    "member lookup: miss in class — ascending to parent",
                );
                current = parent;
            }
            None => {
                debug!(
                    class = %current,
                    member = %member_name,
                    hop,
                    what,
                    "member lookup: no match and no parent — giving up",
                );
                return None;
            }
        }
    }
    debug!(
        class = %class_name,
        member = %member_name,
        what,
        "member lookup: exceeded inheritance-hop limit",
    );
    None
}

/// Look up `method_name` declared inside the body of `class_name`, walking
/// up the inheritance chain via [`Symbol::parent_class_name`].
///
/// Returns `(file_url, symbol)` so callers can navigate to the declaration
/// without a separate URL lookup.
pub(crate) fn find_method_in_class(
    wi: &WorkspaceIndex,
    class_name: &str,
    method_name: &str,
) -> Option<(Url, Symbol)> {
    find_member_in_class_chain(wi, class_name, method_name, "method", |k| {
        k == MSymbolKind::Method
    })
    .map(|(_, url, sym)| (url, sym))
}

/// Variant of [`find_method_in_class`] that matches `Variable`, `Port`, and
/// `Parameter` kinds — i.e. class fields.
pub(crate) fn find_field_in_class(
    wi: &WorkspaceIndex,
    class_name: &str,
    field_name: &str,
) -> Option<(Url, Symbol)> {
    find_member_in_class_chain(wi, class_name, field_name, "field", |k| {
        matches!(
            k,
            MSymbolKind::Variable | MSymbolKind::Port | MSymbolKind::Parameter
        )
    })
    .map(|(_, url, sym)| (url, sym))
}

// --------------------------------------------------------------------------
// find_member — unified method-or-field lookup
// --------------------------------------------------------------------------

/// Look up `member_name` in `class_name`, trying methods first then fields.
/// Returns `(file_url, symbol)` on success.
pub(crate) fn find_member(
    wi: &WorkspaceIndex,
    class_name: &str,
    member_name: &str,
) -> Option<(Url, Symbol)> {
    find_method_in_class(wi, class_name, member_name)
        .or_else(|| find_field_in_class(wi, class_name, member_name))
}

// --------------------------------------------------------------------------
// resolve_member_chain
// --------------------------------------------------------------------------

/// Resolve the member-access chain to the symbol at `chain.target_idx`.
///
/// Returns `(declaring_url, resolved_symbol)` on success.
///
/// The tree-sitter path supports at most **2 intermediate hops** between
/// the root and the target — chains exceeding this limit return `None`,
/// allowing the caller to fall back to slang.
///
/// **Backend-agnostic**: zero imports from `mimir_slang` / `mimir_ast`.
/// When slang is replaced, this function is unchanged.
pub(crate) fn resolve_member_chain(
    chain: &MemberChain,
    pos: Position,
    tree: &SyntaxTree,
    rope: &Rope,
    wi: &WorkspaceIndex,
) -> Option<(Url, Symbol)> {
    const MAX_INTERMEDIATE_HOPS: usize = 2;

    // ── Phase 1: resolve root type ────────────────────────────────────────
    let root_type: String = match &chain.segments[0] {
        ChainSegment::Root(name) => {
            let raw = match find_variable_type_at(tree, rope, pos, name) {
                Some(t) => t,
                None => {
                    debug!(receiver = %name, "chain phase 1: find_variable_type_at returned None");
                    return None;
                }
            };
            match normalize_type_name(&raw) {
                Some(t) => {
                    debug!(receiver = %name, raw_type = %raw, normalized = %t, "chain phase 1: root resolved (Root)");
                    t
                }
                None => {
                    debug!(receiver = %name, raw_type = %raw, "chain phase 1: normalize_type_name failed");
                    return None;
                }
            }
        }
        ChainSegment::This => {
            match enclosing_class_info_at(tree, rope, pos) {
                Some(info) => {
                    debug!(class = %info.class_name, "chain phase 1: root resolved (This)");
                    info.class_name
                }
                None => {
                    debug!("chain phase 1: enclosing_class_info_at returned None for `this`");
                    return None;
                }
            }
        }
        ChainSegment::Super => {
            let info = match enclosing_class_info_at(tree, rope, pos) {
                Some(i) => i,
                None => {
                    debug!("chain phase 1: enclosing_class_info_at returned None for `super`");
                    return None;
                }
            };
            match info.parent_class_name {
                Some(p) => {
                    debug!(parent = %p, "chain phase 1: root resolved (Super)");
                    p
                }
                None => {
                    debug!(class = %info.class_name, "chain phase 1: `super` used in class with no parent");
                    return None;
                }
            }
        }
        other => {
            debug!(segment = ?other, "chain phase 1: unexpected root segment kind");
            return None;
        }
    };

    if chain.target_idx == 0 {
        return None; // cursor on root keyword — not a member
    }

    // ── Phase 2: walk intermediate hops (1 .. target_idx - 1) ───────────
    let mut current_type = root_type;
    let intermediate = &chain.segments[1..chain.target_idx];
    for (hop, seg) in intermediate.iter().enumerate() {
        if hop >= MAX_INTERMEDIATE_HOPS {
            debug!(
                hop,
                segment = ?seg,
                "chain resolver: exceeded 2-hop limit, falling back to slang"
            );
            return None;
        }
        let name = match seg.name() {
            Some(n) => n,
            None => {
                debug!(hop, segment = ?seg, "chain phase 2: hop segment has no name");
                return None;
            }
        };
        let (_, sym) = match find_member(wi, &current_type, name) {
            Some(m) => m,
            None => {
                debug!(
                    hop,
                    class = %current_type,
                    member = %name,
                    "chain phase 2: find_member returned None at hop",
                );
                return None;
            }
        };
        // Advance the current type: MethodCall uses return_type, Member uses decl_type.
        // Try both to handle edge cases where the classifier is uncertain.
        let raw_type = match seg {
            ChainSegment::MethodCall(_) => {
                match sym.return_type.as_deref().or(sym.decl_type.as_deref()) {
                    Some(t) => t,
                    None => {
                        debug!(
                            hop, class = %current_type, member = %name,
                            "chain phase 2: method hop has neither return_type nor decl_type",
                        );
                        return None;
                    }
                }
            }
            _ => {
                match sym.decl_type.as_deref().or(sym.return_type.as_deref()) {
                    Some(t) => t,
                    None => {
                        debug!(
                            hop, class = %current_type, member = %name,
                            "chain phase 2: field hop has neither decl_type nor return_type",
                        );
                        return None;
                    }
                }
            }
        };
        let normalized = match normalize_type_name(raw_type) {
            Some(t) => t,
            None => {
                debug!(
                    hop, class = %current_type, member = %name, raw_type,
                    "chain phase 2: normalize_type_name failed for hop result",
                );
                return None;
            }
        };
        trace!(
            hop, from_class = %current_type, member = %name,
            raw_type, normalized_type = %normalized,
            "chain phase 2: hop advanced",
        );
        current_type = normalized;
    }

    // ── Phase 3: resolve target ───────────────────────────────────────────
    let target_name = match chain.segments[chain.target_idx].name() {
        Some(n) => n,
        None => {
            debug!(target_idx = chain.target_idx, "chain phase 3: target segment has no name");
            return None;
        }
    };
    let result = find_member(wi, &current_type, target_name);
    match &result {
        Some((url, sym)) => debug!(
            class = %current_type,
            target = %target_name,
            resolved_url = %url,
            resolved_kind = ?sym.kind,
            "chain phase 3: target resolved",
        ),
        None => debug!(
            class = %current_type,
            target = %target_name,
            "chain phase 3: find_member returned None for target",
        ),
    }
    result
}

// --------------------------------------------------------------------------
// build_chain_for_receiver
// --------------------------------------------------------------------------

/// Build a [`MemberChain`] from a textual receiver chain and a method name.
///
/// Used by `resolve_method_symbol` to handle `obj.field.method(args)`:
/// the caller strips the method name from the full `hierarchical_identifier`
/// text (via `rsplit_once('.')`) and passes the receiver chain here.
///
/// * `recv_chain` — dot-separated receiver (e.g. `"a.b"` for `a.b.method`),
///   `"this"`, `"super"`, or `"obj"` for a single-segment receiver.
/// * `method_name` — the name of the method being called.
///
/// The returned chain always ends with `MethodCall(method_name)` as the
/// last segment, and `target_idx` points to it.
pub(crate) fn build_chain_for_receiver(recv_chain: &str, method_name: &str) -> MemberChain {
    let mut segments: Vec<ChainSegment> = recv_chain
        .split('.')
        .enumerate()
        .map(|(i, part)| {
            if i == 0 {
                match part {
                    "this" => ChainSegment::This,
                    "super" => ChainSegment::Super,
                    _ => ChainSegment::Root(part.to_string()),
                }
            } else {
                ChainSegment::Member(part.to_string())
            }
        })
        .collect();
    segments.push(ChainSegment::MethodCall(method_name.to_string()));
    let target_idx = segments.len() - 1;
    MemberChain { segments, target_idx }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_index::WorkspaceIndex;
    use mimir_core::{Position as MPosition, Range as MRange};
    use mimir_syntax::SymbolKind as MSymbolKind;

    // A test builder mirroring every field a `Symbol` needs; the arg count is
    // inherent to the struct, not a design smell.
    #[allow(clippy::too_many_arguments)]
    fn make_sym(
        name: &str,
        kind: MSymbolKind,
        name_line: u32,
        full_start: u32,
        full_end: u32,
        parent: Option<&str>,
        return_type: Option<&str>,
        decl_type: Option<&str>,
    ) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: MRange::new(
                MPosition::new(name_line, 0),
                MPosition::new(name_line, name.len() as u32),
            ),
            full_range: MRange::new(
                MPosition::new(full_start, 0),
                MPosition::new(full_end, 0),
            ),
            params: None,
            parent_class_name: parent.map(str::to_string),
            return_type: return_type.map(str::to_string),
            decl_type: decl_type.map(str::to_string),
        }
    }

    fn test_url(name: &str) -> Url {
        Url::parse(&format!("file:///test/{name}.sv")).unwrap()
    }

    /// Build a minimal WorkspaceIndex:
    ///
    /// - `MyClass`   (lines 0–9) with field `ap: PortClass` (line 1) and method `get(): RetClass` (line 2)
    /// - `PortClass` (lines 0–4) with method `write()` (line 1)
    /// - `RetClass`  (lines 0–4) with field `val: int` (line 1)
    ///
    /// All member ranges are contained within their parent class's full_range
    /// so `range_contains` passes.
    fn make_index() -> WorkspaceIndex {
        let my_url = test_url("myclass");
        let port_url = test_url("portclass");
        let ret_url = test_url("retclass");

        let mut idx = WorkspaceIndex::default();

        // MyClass: full_range 0–9, members at lines 1 and 2
        idx.update(my_url.clone(), &[
            make_sym("MyClass", MSymbolKind::Class,    0, 0, 9, None, None, None),
            make_sym("ap",      MSymbolKind::Variable, 1, 1, 2, None, None, Some("PortClass")),
            make_sym("get",     MSymbolKind::Method,   2, 2, 3, None, Some("RetClass"), None),
        ]);

        // PortClass: full_range 0–4, write() at line 1
        idx.update(port_url.clone(), &[
            make_sym("PortClass", MSymbolKind::Class,  0, 0, 4, None, None, None),
            make_sym("write",     MSymbolKind::Method, 1, 1, 2, None, None, None),
        ]);

        // RetClass: full_range 0–4, val at line 1
        idx.update(ret_url.clone(), &[
            make_sym("RetClass", MSymbolKind::Class,    0, 0, 4, None, None, None),
            make_sym("val",      MSymbolKind::Variable, 1, 1, 2, None, None, Some("int")),
        ]);

        idx
    }

    #[test]
    fn find_method_returns_url_and_symbol() {
        let idx = make_index();
        let (url, sym) = find_method_in_class(&idx, "MyClass", "get")
            .expect("should find get() in MyClass");
        assert_eq!(sym.name, "get");
        assert!(url.as_str().contains("myclass"));
    }

    #[test]
    fn find_field_returns_url_and_symbol() {
        let idx = make_index();
        let (url, sym) = find_field_in_class(&idx, "MyClass", "ap")
            .expect("should find ap in MyClass");
        assert_eq!(sym.name, "ap");
        assert!(url.as_str().contains("myclass"));
    }

    #[test]
    fn find_member_prefers_method() {
        let idx = make_index();
        let (_, sym) = find_member(&idx, "MyClass", "get").expect("get found");
        assert_eq!(sym.kind, MSymbolKind::Method);
    }

    #[test]
    fn build_chain_simple_receiver() {
        let chain = build_chain_for_receiver("obj", "write");
        assert_eq!(chain.segments, vec![
            ChainSegment::Root("obj".into()),
            ChainSegment::MethodCall("write".into()),
        ]);
        assert_eq!(chain.target_idx, 1);
    }

    #[test]
    fn build_chain_two_hop_receiver() {
        let chain = build_chain_for_receiver("a.b", "method");
        assert_eq!(chain.segments, vec![
            ChainSegment::Root("a".into()),
            ChainSegment::Member("b".into()),
            ChainSegment::MethodCall("method".into()),
        ]);
        assert_eq!(chain.target_idx, 2);
    }

    #[test]
    fn build_chain_this_receiver() {
        let chain = build_chain_for_receiver("this", "run");
        assert_eq!(chain.segments, vec![
            ChainSegment::This,
            ChainSegment::MethodCall("run".into()),
        ]);
        assert_eq!(chain.target_idx, 1);
    }

    #[test]
    fn build_chain_super_receiver() {
        let chain = build_chain_for_receiver("super", "build_phase");
        assert_eq!(chain.segments, vec![
            ChainSegment::Super,
            ChainSegment::MethodCall("build_phase".into()),
        ]);
        assert_eq!(chain.target_idx, 1);
    }
}
