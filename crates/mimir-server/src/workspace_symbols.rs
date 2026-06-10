//! `workspace/symbol` filtering and fuzzy ranking.
//!
//! Owns the kind filter (which declaration kinds belong in the picker) and
//! the score-ordered ranking over the workspace index, capped at
//! [`WORKSPACE_SYMBOL_LIMIT`] entries.

use mimir_syntax::SymbolKind as MSymbolKind;
use tower_lsp::lsp_types::{Location, SymbolInformation};

use crate::completion_score;
use crate::lsp_convert::symbol_kind_to_lsp;
use crate::slang_service::m_range_to_lsp;
use crate::workspace_index;

/// Maximum number of entries returned by `workspace/symbol`. Picked to
/// match completion's 200-item cap — same trade-off (the picker becomes
/// unusable above a few hundred items anyway, and the fuzzy ranker
/// surfaces the best matches first).
pub(crate) const WORKSPACE_SYMBOL_LIMIT: usize = 200;

/// Decide whether a workspace symbol kind should appear in the
/// `workspace/symbol` picker.
///
/// IDE pickers like VS Code's `Ctrl+T` are top-of-mind navigation —
/// users expect declarations they'd hand-author or jump to by name
/// (modules, classes, functions, macros, …), not every `logic [7:0] x`
/// variable in the project. We exclude port/variable/parameter/enum-
/// member kinds because surfacing them swamps the picker on any
/// real-sized UVM testbench. See [`README.md`](../../../README.md)
/// `workspace/symbol` checklist entry for the v1 user-facing contract.
pub(crate) fn is_workspace_symbol_kind(kind: MSymbolKind) -> bool {
    !matches!(
        kind,
        MSymbolKind::Variable
            | MSymbolKind::Port
            | MSymbolKind::Parameter
            | MSymbolKind::EnumMember
    )
}

/// Fuzzy-rank workspace-index entries against `query` and convert the
/// surviving hits to LSP [`SymbolInformation`].
///
/// Pulled out of the `workspace/symbol` handler as a pure function so the unit
/// tests below can drive it without spinning up a `Backend`. Iteration
/// order of `entries` is HashMap-arbitrary (see
/// [`WorkspaceIndex::entries`](crate::workspace_index::WorkspaceIndex::entries)),
/// so the final sort is the load-bearing ordering — we sort by score
/// descending and break ties by `(name, url, start_line, start_char)`
/// for determinism in tests and in the user-visible picker.
///
/// Kind filtering and the [`WORKSPACE_SYMBOL_LIMIT`] cap are both
/// applied here, so a caller that swaps in a different scorer later
/// won't drift from the documented v1 contract.
pub(crate) fn rank_workspace_symbols<'a>(
    query: &str,
    entries: impl Iterator<Item = &'a workspace_index::Entry>,
) -> Vec<SymbolInformation> {
    let mut matcher = completion_score::matcher();
    let mut scored: Vec<(u32, &workspace_index::Entry)> = Vec::new();
    for entry in entries {
        if !is_workspace_symbol_kind(entry.symbol.kind) {
            continue;
        }
        let Some(score) = completion_score::score(&mut matcher, query, &entry.symbol.name) else {
            continue;
        };
        scored.push((score, entry));
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.symbol.name.cmp(&b.1.symbol.name))
            .then_with(|| a.1.url.as_str().cmp(b.1.url.as_str()))
            .then_with(|| {
                a.1.symbol
                    .name_range
                    .start
                    .line
                    .cmp(&b.1.symbol.name_range.start.line)
                    .then(
                        a.1.symbol
                            .name_range
                            .start
                            .character
                            .cmp(&b.1.symbol.name_range.start.character),
                    )
            })
    });

    scored
        .into_iter()
        .take(WORKSPACE_SYMBOL_LIMIT)
        .map(|(_, entry)| {
            #[allow(deprecated)]
            SymbolInformation {
                name: entry.symbol.name.clone(),
                kind: symbol_kind_to_lsp(entry.symbol.kind),
                tags: None,
                deprecated: None,
                location: Location {
                    uri: entry.url.clone(),
                    range: m_range_to_lsp(entry.symbol.name_range),
                },
                container_name: entry.symbol.parent_class_name.clone(),
            }
        })
        .collect()
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use mimir_core::{Position as MPosition, Range as MRange};
    use mimir_syntax::Symbol;
    use tower_lsp::lsp_types::Url;

    /// Helper: build a `Symbol` of the given name + kind. Range values
    /// are arbitrary — the tests only care about identity/order.
    fn sym(name: &str, kind: MSymbolKind, line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 1)),
            full_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 10)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    // ----------------------------------------------------------------------
    // workspace/symbol — rank_workspace_symbols
    // ----------------------------------------------------------------------

    /// Build a `workspace_index::Entry` from a URL and a freshly-built
    /// `Symbol`. Co-located with the `workspace/symbol` tests because no
    /// other test path here needs it.
    fn entry(u: &Url, s: Symbol) -> workspace_index::Entry {
        workspace_index::Entry {
            url: u.clone(),
            symbol: s,
        }
    }


    /// Empty query returns every kept-kind entry — that's the picker's
    /// initial state when the user first opens it.
    #[test]
    fn workspace_symbol_empty_query_returns_all_visible_kinds() {
        let u = url("file:///a.sv");
        let entries = [
            entry(&u, sym("my_module", MSymbolKind::Module, 0)),
            entry(&u, sym("my_class", MSymbolKind::Class, 1)),
        ];
        let out = rank_workspace_symbols("", entries.iter());
        assert_eq!(out.len(), 2);
    }


    /// Fuzzy matching: subsequence of the candidate matches, unrelated
    /// queries don't. Mirrors completion's behaviour.
    #[test]
    fn workspace_symbol_fuzzy_matches_subsequence() {
        let u = url("file:///a.sv");
        let entries = [
            entry(&u, sym("uvm_foo_bar", MSymbolKind::Class, 0)),
            entry(&u, sym("unrelated", MSymbolKind::Module, 1)),
        ];
        let out = rank_workspace_symbols("ufoo", entries.iter());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "uvm_foo_bar");
    }


    /// `Variable`/`Port`/`Parameter`/`EnumMember` are too noisy for a
    /// project-wide picker — every UVM port and enum value would swamp
    /// the list — so they're filtered out. Documented v1 contract.
    #[test]
    fn workspace_symbol_excludes_noisy_kinds() {
        let u = url("file:///a.sv");
        let entries = [
            entry(&u, sym("clk", MSymbolKind::Port, 0)),
            entry(&u, sym("counter", MSymbolKind::Variable, 1)),
            entry(&u, sym("WIDTH", MSymbolKind::Parameter, 2)),
            entry(&u, sym("RED", MSymbolKind::EnumMember, 3)),
            entry(&u, sym("my_mod", MSymbolKind::Module, 4)),
        ];
        let out = rank_workspace_symbols("", entries.iter());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "my_mod");
    }


    /// Same name in two files yields two distinct `SymbolInformation`
    /// results — the picker shows both rows and the user picks. We
    /// must not collapse them.
    #[test]
    fn workspace_symbol_same_name_in_two_files_yields_two_results() {
        let a = url("file:///a.sv");
        let b = url("file:///b.sv");
        let entries = [
            entry(&a, sym("twin", MSymbolKind::Class, 0)),
            entry(&b, sym("twin", MSymbolKind::Class, 0)),
        ];
        let out = rank_workspace_symbols("twin", entries.iter());
        assert_eq!(out.len(), 2);
        let mut urls: Vec<_> = out.iter().map(|s| s.location.uri.as_str()).collect();
        urls.sort();
        assert_eq!(urls, vec!["file:///a.sv", "file:///b.sv"]);
    }


    /// The 200-item cap kicks in for over-cap result sets so the picker
    /// stays responsive. Test with an empty query so every candidate is
    /// kept by the scorer.
    #[test]
    fn workspace_symbol_enforces_cap() {
        let u = url("file:///a.sv");
        let entries: Vec<_> = (0..(WORKSPACE_SYMBOL_LIMIT + 50))
            .map(|i| entry(&u, sym(&format!("c{i:04}"), MSymbolKind::Class, i as u32)))
            .collect();
        let out = rank_workspace_symbols("", entries.iter());
        assert_eq!(out.len(), WORKSPACE_SYMBOL_LIMIT);
    }


    /// `container_name` is populated for class methods (the workspace
    /// index already carries `parent_class_name`) and `None` for free
    /// functions / modules. v1 deliberately doesn't synthesise a
    /// container for non-method symbols — README documents the gap.
    #[test]
    fn workspace_symbol_container_name_for_methods_only() {
        let u = url("file:///a.sv");
        let mut method = sym("do_thing", MSymbolKind::Method, 0);
        method.parent_class_name = Some("my_class".into());
        let entries = [
            entry(&u, method),
            entry(&u, sym("free_func", MSymbolKind::Function, 1)),
        ];
        let out = rank_workspace_symbols("", entries.iter());
        let by_name: HashMap<&str, &SymbolInformation> =
            out.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(
            by_name["do_thing"].container_name.as_deref(),
            Some("my_class"),
        );
        assert_eq!(by_name["free_func"].container_name, None);
    }


    /// Higher-scoring matches sort before lower ones — prefix beats
    /// subsequence, mirroring completion's UX.
    #[test]
    fn workspace_symbol_sorts_by_score_descending() {
        let u = url("file:///a.sv");
        let entries = [
            entry(&u, sym("my_class", MSymbolKind::Class, 0)),
            entry(&u, sym("class", MSymbolKind::Class, 1)),
        ];
        let out = rank_workspace_symbols("clas", entries.iter());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "class");
        assert_eq!(out[1].name, "my_class");
    }
}
