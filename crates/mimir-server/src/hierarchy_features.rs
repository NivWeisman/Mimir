//! Pure helpers for `callHierarchy/*` and `typeHierarchy/*` LSP features.
//!
//! All functions are sync — handlers in `backend.rs` snapshot data under
//! locks, drop the locks, then delegate to these functions.

use std::collections::HashMap;

use mimir_ast::{DeclKind, MimirAst};
use mimir_core::{Position as MPosition, Range as MRange};
use mimir_syntax::{
    calls::{call_sites_in, find_enclosing_callable},
    SymbolKind as MSymbolKind, SyntaxTree,
};
use ropey::Rope;
use tower_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Range, SymbolKind,
    TypeHierarchyItem, Url,
};

use crate::ast_features;
use crate::slang_service::m_range_to_lsp;
use crate::workspace_index::WorkspaceIndex;

/// Maximum number of results returned by any hierarchy query.
const MAX_HIERARCHY_ITEMS: usize = 500;

// --------------------------------------------------------------------------
// Shared helpers
// --------------------------------------------------------------------------

fn m_kind_to_lsp(kind: MSymbolKind) -> SymbolKind {
    match kind {
        MSymbolKind::Module | MSymbolKind::Program => SymbolKind::MODULE,
        MSymbolKind::Interface => SymbolKind::INTERFACE,
        MSymbolKind::Package => SymbolKind::PACKAGE,
        MSymbolKind::Class => SymbolKind::CLASS,
        MSymbolKind::Function | MSymbolKind::Task => SymbolKind::FUNCTION,
        MSymbolKind::Method => SymbolKind::METHOD,
        _ => SymbolKind::OBJECT,
    }
}

fn lsp_range_to_m(r: Range) -> MRange {
    MRange::new(
        MPosition::new(r.start.line, r.start.character),
        MPosition::new(r.end.line, r.end.character),
    )
}

// --------------------------------------------------------------------------
// Call hierarchy
// --------------------------------------------------------------------------

/// Build a [`CallHierarchyItem`] from raw symbol data.
///
/// Both `name_range` and `full_range` use `mimir_core::Range` coordinates;
/// this function converts them to LSP wire format.
pub(crate) fn call_hierarchy_item(
    name: &str,
    kind: MSymbolKind,
    name_range: MRange,
    full_range: MRange,
    uri: &Url,
) -> CallHierarchyItem {
    CallHierarchyItem {
        name: name.to_string(),
        kind: m_kind_to_lsp(kind),
        tags: None,
        detail: None,
        uri: uri.clone(),
        range: m_range_to_lsp(full_range),
        selection_range: m_range_to_lsp(name_range),
        data: None,
    }
}

/// Scan every `(url, tree)` pair for call sites matching `callee_name`.
///
/// For each matching call site the nearest enclosing function or task is
/// located. Results are grouped by enclosing caller: multiple calls from
/// the same caller collapse into one [`CallHierarchyIncomingCall`] with
/// multiple `from_ranges`.
///
/// Call sites at module scope (no enclosing callable) are skipped because
/// LSP requires a named callable as the "from" item.
///
/// At most [`MAX_HIERARCHY_ITEMS`] distinct callers are returned.
pub(crate) fn collect_incoming_calls(
    callee_name: &str,
    trees: &[(Url, SyntaxTree)],
    wi: &WorkspaceIndex,
) -> Vec<CallHierarchyIncomingCall> {
    // Key: (Url, enclosing-name-range start line, start character)
    // to group multiple call sites in the same function into one result.
    let mut groups: HashMap<(Url, u32, u32), (CallHierarchyItem, Vec<Range>)> = HashMap::new();

    'trees: for (url, tree) in trees {
        let rope = Rope::from_str(tree.source());
        // len_lines() counts the implicit empty line after a trailing newline;
        // saturate to stay within bounds accepted by to_byte_offset.
        let last_line = rope.len_lines().saturating_sub(1) as u32;
        let full_file = MRange::new(MPosition::new(0, 0), MPosition::new(last_line, 0));

        for site in call_sites_in(tree, &rope, full_file)
            .into_iter()
            .filter(|s| s.name == callee_name)
        {
            let Some(enc) = find_enclosing_callable(tree, &rope, site.name_range.start) else {
                continue;
            };

            let key = (
                url.clone(),
                enc.name_range.start.line,
                enc.name_range.start.character,
            );
            let call_range = m_range_to_lsp(site.name_range);

            let entry = groups.entry(key).or_insert_with(|| {
                // Prefer workspace-index data for the caller (accurate full_range
                // even when the call site is in a closed file). Fall through to the
                // tree-derived ranges when the caller isn't indexed.
                let (full_r, kind) = wi
                    .lookup(&enc.name)
                    .iter()
                    .find(|e| e.url == *url && e.symbol.kind == enc.kind)
                    .map(|e| (e.symbol.full_range, e.symbol.kind))
                    .unwrap_or((enc.full_range, enc.kind));
                let item = call_hierarchy_item(&enc.name, kind, enc.name_range, full_r, url);
                (item, Vec::new())
            });
            entry.1.push(call_range);

            if groups.len() >= MAX_HIERARCHY_ITEMS {
                break 'trees;
            }
        }
    }

    groups
        .into_values()
        .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
        .collect()
}

/// Return all calls made *within* the function or task at `item_range` inside
/// the parse tree for `item_uri`.
///
/// Each unique callee that resolves to a known function, task, or method in
/// `wi` becomes one [`CallHierarchyOutgoingCall`]. Multiple call sites with
/// the same callee name merge into `from_ranges`.
///
/// Callee names that do not appear in `wi` are silently skipped (built-in
/// system tasks, macros, unresolved imports, …).
pub(crate) fn collect_outgoing_calls(
    item_range: Range,
    tree: &SyntaxTree,
    wi: &WorkspaceIndex,
) -> Vec<CallHierarchyOutgoingCall> {
    let rope = Rope::from_str(tree.source());
    let m_range = lsp_range_to_m(item_range);
    let sites = call_sites_in(tree, &rope, m_range);

    // Group call-site ranges by callee name.
    let mut by_callee: HashMap<String, Vec<Range>> = HashMap::new();
    for site in sites {
        by_callee
            .entry(site.name.clone())
            .or_default()
            .push(m_range_to_lsp(site.name_range));
    }

    let mut out = Vec::new();
    for (callee_name, from_ranges) in by_callee {
        let Some(entry) = wi.lookup(&callee_name).iter().find(|e| {
            matches!(
                e.symbol.kind,
                MSymbolKind::Function | MSymbolKind::Task | MSymbolKind::Method
            )
        }) else {
            continue;
        };
        let to = call_hierarchy_item(
            &entry.symbol.name,
            entry.symbol.kind,
            entry.symbol.name_range,
            entry.symbol.full_range,
            &entry.url,
        );
        out.push(CallHierarchyOutgoingCall { to, from_ranges });
        if out.len() >= MAX_HIERARCHY_ITEMS {
            break;
        }
    }
    out
}

// --------------------------------------------------------------------------
// Type hierarchy
// --------------------------------------------------------------------------

/// Build a [`TypeHierarchyItem`] from raw symbol data.
pub(crate) fn type_hierarchy_item(
    name: &str,
    name_range: MRange,
    full_range: MRange,
    uri: &Url,
) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: name.to_string(),
        kind: SymbolKind::CLASS,
        tags: None,
        detail: None,
        uri: uri.clone(),
        range: m_range_to_lsp(full_range),
        selection_range: m_range_to_lsp(name_range),
        data: None,
    }
}

/// Return the direct parent class(es) of `class_name`.
///
/// The MimirAst (slang) path is preferred because it resolves parameterised
/// and imported base class names accurately. Falls back to the tree-sitter
/// workspace index when slang is not available.
///
/// LSP clients call `typeHierarchy/supertypes` recursively to walk the full
/// chain; this function returns only the *immediate* parent(s).
pub(crate) fn collect_supertypes(
    class_name: &str,
    wi: &WorkspaceIndex,
    ast: Option<&MimirAst>,
) -> Vec<TypeHierarchyItem> {
    // --- slang path ---
    if let Some(ast) = ast {
        if let Some((decl, _uri)) = ast_features::find_decl_global(ast, class_name) {
            if decl.kind == DeclKind::Class {
                if let Some(parent_name) = &decl.parent_class {
                    return resolve_class_item(parent_name, wi).into_iter().collect();
                }
            }
        }
    }

    // --- tree-sitter fallback ---
    for entry in wi.lookup(class_name) {
        if entry.symbol.kind == MSymbolKind::Class {
            if let Some(parent_name) = &entry.symbol.parent_class_name {
                return resolve_class_item(parent_name, wi).into_iter().collect();
            }
        }
    }
    vec![]
}

/// Find the first Class entry for `class_name` in `wi` and build an item.
fn resolve_class_item(class_name: &str, wi: &WorkspaceIndex) -> Option<TypeHierarchyItem> {
    wi.lookup(class_name)
        .iter()
        .find(|e| e.symbol.kind == MSymbolKind::Class)
        .map(|e| {
            type_hierarchy_item(
                &e.symbol.name,
                e.symbol.name_range,
                e.symbol.full_range,
                &e.url,
            )
        })
}

/// Return all classes in the workspace that directly extend `class_name`.
///
/// LSP clients call `typeHierarchy/subtypes` recursively; this function
/// returns only the *immediate* subclasses. At most [`MAX_HIERARCHY_ITEMS`]
/// results are returned.
pub(crate) fn collect_subtypes(
    class_name: &str,
    wi: &WorkspaceIndex,
) -> Vec<TypeHierarchyItem> {
    wi.entries()
        .filter(|e| {
            e.symbol.kind == MSymbolKind::Class
                && e.symbol
                    .parent_class_name
                    .as_deref()
                    .is_some_and(|p| p == class_name)
        })
        .map(|e| {
            type_hierarchy_item(
                &e.symbol.name,
                e.symbol.name_range,
                e.symbol.full_range,
                &e.url,
            )
        })
        .take(MAX_HIERARCHY_ITEMS)
        .collect()
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::logging::init_for_tests;
    use mimir_syntax::{Symbol, SymbolKind as MSymbolKind, SyntaxParser};

    fn make_sym(name: &str, kind: MSymbolKind, line: u32, parent: Option<&str>) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, name.len() as u32)),
            full_range: MRange::new(MPosition::new(line, 0), MPosition::new(line + 1, 0)),
            params: None,
            parent_class_name: parent.map(str::to_string),
            return_type: None,
            decl_type: None,
        }
    }

    fn test_url(path: &str) -> Url {
        Url::from_file_path(path).unwrap()
    }

    fn index_with(url: &Url, syms: &[Symbol]) -> WorkspaceIndex {
        let mut wi = WorkspaceIndex::default();
        wi.update(url.clone(), syms);
        wi
    }

    // --- collect_subtypes ---

    #[test]
    fn subtypes_finds_direct_subclass() {
        init_for_tests();
        let url = test_url("/tmp/test.sv");
        let base = make_sym("Base", MSymbolKind::Class, 0, None);
        let child = make_sym("Child", MSymbolKind::Class, 10, Some("Base"));
        let wi = index_with(&url, &[base, child]);

        let result = collect_subtypes("Base", &wi);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Child");
    }

    #[test]
    fn subtypes_ignores_non_subclasses() {
        init_for_tests();
        let url = test_url("/tmp/test.sv");
        let base = make_sym("Base", MSymbolKind::Class, 0, None);
        let unrelated = make_sym("Other", MSymbolKind::Class, 10, None);
        let wi = index_with(&url, &[base, unrelated]);

        let result = collect_subtypes("Base", &wi);
        assert!(result.is_empty());
    }

    // --- collect_supertypes ---

    #[test]
    fn supertypes_finds_parent_class() {
        init_for_tests();
        let url = test_url("/tmp/test.sv");
        let base = make_sym("Base", MSymbolKind::Class, 0, None);
        let child = make_sym("Child", MSymbolKind::Class, 10, Some("Base"));
        let wi = index_with(&url, &[base, child]);

        let result = collect_supertypes("Child", &wi, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Base");
    }

    #[test]
    fn supertypes_returns_empty_for_root_class() {
        init_for_tests();
        let url = test_url("/tmp/test.sv");
        let base = make_sym("Base", MSymbolKind::Class, 0, None);
        let wi = index_with(&url, &[base]);

        let result = collect_supertypes("Base", &wi, None);
        assert!(result.is_empty());
    }

    // --- collect_incoming_calls ---

    #[test]
    fn incoming_calls_finds_caller() {
        init_for_tests();
        // tree-sitter-sv only creates tf_call in expression context (RHS of
        // assignment), not as a standalone statement — use assignment form.
        let src = "\
class c;
  function int callee();
    return 0;
  endfunction
  function void caller();
    int r = callee();
  endfunction
endclass
";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let url = test_url("/tmp/test.sv");

        // Index the two symbols so the enclosing-callable lookup can find full_range.
        let syms = mimir_syntax::symbols::index(&tree, &Rope::from_str(src));
        let wi = index_with(&url, &syms);

        let trees = vec![(url.clone(), tree)];
        let result = collect_incoming_calls("callee", &trees, &wi);
        assert_eq!(result.len(), 1, "expected one caller, got {:?}", result.len());
        assert_eq!(result[0].from.name, "caller");
    }

    // --- collect_outgoing_calls ---

    #[test]
    fn outgoing_calls_finds_callee() {
        init_for_tests();
        let src = "\
class c;
  function int callee();
    return 0;
  endfunction
  function void caller();
    int r = callee();
  endfunction
endclass
";
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(src, None).unwrap();
        let url = test_url("/tmp/test.sv");

        let syms = mimir_syntax::symbols::index(&tree, &Rope::from_str(src));
        let wi = index_with(&url, &syms);

        // The caller function spans lines 4-7 (0-indexed). Use a generous range.
        let caller_range = Range {
            start: tower_lsp::lsp_types::Position { line: 4, character: 0 },
            end: tower_lsp::lsp_types::Position { line: 8, character: 0 },
        };
        let result = collect_outgoing_calls(caller_range, &tree, &wi);
        assert!(
            result.iter().any(|c| c.to.name == "callee"),
            "expected 'callee' in outgoing calls, got: {:?}",
            result.iter().map(|c| &c.to.name).collect::<Vec<_>>()
        );
    }
}
