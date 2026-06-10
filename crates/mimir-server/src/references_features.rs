//! Name-to-declaration resolution and workspace-wide reference collection.
//!
//! [`resolve_definition`] ranks declaration sites (same-file first,
//! workspace second); [`collect_references`] is the scan engine shared by
//! `textDocument/references` and `rename` — scope-aware in the cursor
//! file, lexical across other open buffers and filelist-hydrated trees.

use std::collections::HashSet;

use mimir_core::{Position as MPosition, Range as MRange};
use mimir_syntax::{Symbol, SyntaxTree};
use ropey::Rope;
use tower_lsp::lsp_types::{Location, Url};
use tracing::warn;

use crate::slang_service::m_range_to_lsp;
use crate::workspace_index::WorkspaceIndex;

/// Resolve a name to its declaration sites, same-file first, workspace
/// second.
///
/// Stage-2 precedence rule: if the same file declares the name, those
/// declarations are the answer — workspace hits with the same name in
/// other files are *not* added. That matches the behaviour a user
/// expects when the resolver is syntactic: a local declaration shadows
/// anything in another file. Only when same-file comes up empty do we
/// fall through to the workspace.
///
/// Workspace hits already arrive as a per-name slice from
/// [`WorkspaceIndex::lookup`]. We only need to dedup by `(url,
/// name_range)` so that a file open in the editor *and* listed in the
/// project filelist (which would have folded in twice via
/// `reparse_and_publish` plus eager hydration) doesn't return two
/// identical locations.
///
/// Pure function so it can be unit-tested without spinning up
/// `tower-lsp`.
pub(crate) fn resolve_definition(
    name: &str,
    source_uri: &Url,
    doc_index: &[Symbol],
    workspace_hits: &[(Url, Symbol)],
) -> Vec<(Url, Symbol)> {
    let same_file: Vec<(Url, Symbol)> = doc_index
        .iter()
        .filter(|s| s.name == name)
        .map(|s| (source_uri.clone(), s.clone()))
        .collect();

    if !same_file.is_empty() {
        return same_file;
    }

    let mut out: Vec<(Url, Symbol)> = workspace_hits.to_vec();
    out.sort_by(|a, b| {
        (
            a.0.as_str(),
            a.1.name_range.start.line,
            a.1.name_range.start.character,
        )
            .cmp(&(
                b.0.as_str(),
                b.1.name_range.start.line,
                b.1.name_range.start.character,
            ))
    });
    out.dedup_by(|a, b| a.0 == b.0 && a.1.name_range == b.1.name_range);
    out
}

/// Cap on the total number of `Location`s returned by
/// `textDocument/references`. Picked larger than the workspace-symbol cap
/// (200) because a popular UVM macro can legitimately have hundreds of
/// usages and silently dropping them would be misleading — but small
/// enough that the editor's peek list stays responsive. Truncation is
/// logged at `warn!`.
pub(crate) const REFERENCES_LIMIT: usize = 1000;

/// Collect `textDocument/references` results.
///
/// Pure function — takes already-resolved tree snapshots and the
/// workspace index by reference, returns LSP `Location`s. Split out from
/// the async handler so it's unit-testable without `tower-lsp`.
///
/// `other_open` carries every open document **other than** the cursor
/// file: each entry is `(url, tree)`. The cursor file is handled
/// separately via the scope-aware path.
///
/// Dedup strategy: locations are keyed by `(url, range)`. The cursor
/// file's scope-aware hits are pushed first, then open-buffer
/// whole-file hits, then workspace-index declaration sites. A file that
/// is both open *and* listed in the filelist therefore contributes its
/// (richer) open-buffer hits, and the declaration site from the index
/// is deduped away.
///
/// `include_declaration = false` filters out locations that match any
/// declaration `name_range` known to the workspace index — the LSP
/// semantics are "exclude the *definition* of the symbol", and the
/// index's declarations are the closest syntactic proxy.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_references(
    name: &str,
    cursor_uri: &Url,
    cursor_tree: &SyntaxTree,
    cursor_rope: &Rope,
    cursor_pos: MPosition,
    other_trees: &[(Url, SyntaxTree)],
    wi: &WorkspaceIndex,
    include_declaration: bool,
) -> Vec<Location> {
    // Set of (url, range) declaration sites known to the workspace index.
    // Used both to dedup against the workspace-only path and to filter
    // out declarations when the client requested it.
    let declarations: HashSet<(Url, MRange)> = wi
        .lookup(name)
        .iter()
        .map(|e| (e.url.clone(), e.symbol.name_range))
        .collect();

    let mut seen: HashSet<(Url, MRange)> = HashSet::new();
    let mut out: Vec<Location> = Vec::new();
    let mut truncated = false;

    let push = |out: &mut Vec<Location>,
                seen: &mut HashSet<(Url, MRange)>,
                truncated: &mut bool,
                u: &Url,
                r: MRange|
     -> bool {
        if !include_declaration && declarations.contains(&(u.clone(), r)) {
            return true;
        }
        if !seen.insert((u.clone(), r)) {
            return true;
        }
        if out.len() >= REFERENCES_LIMIT {
            *truncated = true;
            return false;
        }
        out.push(Location {
            uri: u.clone(),
            range: m_range_to_lsp(r),
        });
        true
    };

    // 1. Same file — scope-aware.
    for r in mimir_syntax::symbols::occurrences_of_at(cursor_tree, cursor_rope, cursor_pos) {
        if !push(&mut out, &mut seen, &mut truncated, cursor_uri, r) {
            break;
        }
    }

    // 2. Other trees (open buffers + closed filelist files) — scope-pruned
    //    file-wide match. occurrences_of_scoped skips occurrences inside
    //    nested scopes that locally re-declare `name`, so a local
    //    `int foo` inside a function won't pollute results when the caller
    //    is searching for a module-level `foo`.
    'outer: for (other_uri, other_tree) in other_trees {
        let other_rope = Rope::from_str(other_tree.source());
        for r in mimir_syntax::symbols::occurrences_of_scoped(other_tree, &other_rope, name) {
            if !push(&mut out, &mut seen, &mut truncated, other_uri, r) {
                break 'outer;
            }
        }
    }

    // 3. Declaration sites from the workspace index (for files that
    //    aren't currently open). Open buffers above already covered
    //    those entries, and dedup keeps things consistent.
    if !truncated {
        for entry in wi.lookup(name) {
            if !push(
                &mut out,
                &mut seen,
                &mut truncated,
                &entry.url,
                entry.symbol.name_range,
            ) {
                break;
            }
        }
    }

    if truncated {
        warn!(
            limit = REFERENCES_LIMIT,
            name = %name,
            "references truncated at limit",
        );
    }

    out
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use mimir_core::Range as MRange;
    use mimir_syntax::SymbolKind as MSymbolKind;
    use tower_lsp::lsp_types::{TextEdit, WorkspaceEdit};

    // --- Stage 2: go-to-definition resolver ---------------------------

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


    /// Empty doc index *and* empty workspace: the resolver returns
    /// nothing (and the caller turns that into a `None` LSP response).
    #[test]
    fn resolve_definition_both_empty_returns_no_match() {
        let here = url("file:///a.sv");
        assert!(resolve_definition("foo", &here, &[], &[]).is_empty());
    }


    /// Same-file precedence: when the doc index has the name, the
    /// workspace is ignored entirely. This is the Stage-2 shadowing
    /// rule — a syntactic resolver can't reason about scope, so we
    /// conservatively treat any same-file declaration as authoritative
    /// and don't dilute the editor's peek list with every cross-file
    /// homonym.
    #[test]
    fn resolve_definition_same_file_beats_workspace() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let doc_idx = vec![sym("foo", MSymbolKind::Module, 0)];
        let ws = vec![(other.clone(), sym("foo", MSymbolKind::Class, 9))];

        let hits = resolve_definition("foo", &here, &doc_idx, &ws);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, here);
        assert_eq!(hits[0].1.kind, MSymbolKind::Module);
    }


    /// Multiple same-file declarations (e.g. a `var x` plus a class
    /// field `x`) all come back. The editor's peek list lets the user
    /// pick — that's the v1 UX for ambiguous syntactic resolution.
    #[test]
    fn resolve_definition_multiple_same_file_returned_in_order() {
        let here = url("file:///a.sv");
        let doc_idx = vec![
            sym("x", MSymbolKind::Variable, 1),
            sym("y", MSymbolKind::Variable, 2),
            sym("x", MSymbolKind::Parameter, 5),
        ];
        let hits = resolve_definition("x", &here, &doc_idx, &[]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1.kind, MSymbolKind::Variable);
        assert_eq!(hits[0].1.name_range.start.line, 1);
        assert_eq!(hits[1].1.kind, MSymbolKind::Parameter);
        assert_eq!(hits[1].1.name_range.start.line, 5);
    }


    /// Same-file empty, workspace single match: that match is returned.
    #[test]
    fn resolve_definition_workspace_fallback_single() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let ws = vec![(other.clone(), sym("my_class", MSymbolKind::Class, 3))];

        let hits = resolve_definition("my_class", &here, &[], &ws);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, other);
        assert_eq!(hits[0].1.kind, MSymbolKind::Class);
    }


    /// Same-file empty, multiple workspace matches across URLs: all
    /// returned, deduped on `(url, name_range)`. The duplicate here
    /// simulates a file that's both open (folded in via
    /// `reparse_and_publish`) and listed in the project filelist
    /// (folded in via eager hydration).
    #[test]
    fn resolve_definition_workspace_dedups_identical_locations() {
        let here = url("file:///a.sv");
        let b = url("file:///b.sv");
        let c = url("file:///c.sv");
        let dup = sym("shared", MSymbolKind::Class, 4);
        let ws = vec![
            (b.clone(), dup.clone()),
            (b.clone(), dup),
            (c.clone(), sym("shared", MSymbolKind::Class, 7)),
        ];

        let hits = resolve_definition("shared", &here, &[], &ws);
        assert_eq!(hits.len(), 2, "duplicate (url, range) collapsed to one");
        let urls: Vec<&Url> = hits.iter().map(|(u, _)| u).collect();
        assert!(urls.contains(&&b));
        assert!(urls.contains(&&c));
    }


    // ----------------------------------------------------------------------
    // references — collect_references
    // ----------------------------------------------------------------------

    /// Parse `text` into a `SyntaxTree` for a tests-only callsite. We
    /// build a fresh parser per test rather than sharing one — these
    /// tests aren't perf-sensitive and the parser cost is small.
    fn parse_tree(text: &str) -> SyntaxTree {
        let mut parser = mimir_syntax::SyntaxParser::new().expect("grammar load");
        parser.parse(text, None).expect("parse")
    }


    /// Build a populated `WorkspaceIndex` from a slice of `(url, text)`
    /// pairs. Mirrors how the eager hydration pass folds parsed-from-disk
    /// files into the index on `initialize`.
    fn workspace_index_from(files: &[(&Url, &str)]) -> WorkspaceIndex {
        let mut wi = WorkspaceIndex::default();
        for (u, text) in files {
            let tree = parse_tree(text);
            let rope = Rope::from_str(text);
            let symbols = mimir_syntax::symbols::index(&tree, &rope);
            wi.update((*u).clone(), &symbols);
        }
        wi
    }


    /// Helper: invoke `collect_references` with the cursor positioned on
    /// the first occurrence of `name` in `cursor_text`. Other open
    /// buffers are passed as `(url, text)` pairs.
    fn run_references(
        name: &str,
        cursor_uri: &Url,
        cursor_text: &str,
        other_open: &[(Url, &str)],
        wi: &WorkspaceIndex,
        include_declaration: bool,
    ) -> Vec<Location> {
        let cursor_tree = parse_tree(cursor_text);
        let cursor_rope = Rope::from_str(cursor_text);
        // Position the cursor on the first byte-offset occurrence of `name`.
        let byte = cursor_text.find(name).expect("name appears in cursor_text");
        // Compute UTF-16 column/line for that byte. Cursor text in these
        // tests is ASCII, so byte offset == utf16 offset and we can
        // count lines/columns by character.
        let prefix = &cursor_text[..byte];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
        let col = match prefix.rfind('\n') {
            Some(nl) => (byte - nl - 1) as u32,
            None => byte as u32,
        };
        let cursor_pos = MPosition::new(line, col);

        let other_trees: Vec<(Url, SyntaxTree)> = other_open
            .iter()
            .map(|(u, t)| (u.clone(), parse_tree(t)))
            .collect();

        collect_references(
            name,
            cursor_uri,
            &cursor_tree,
            &cursor_rope,
            cursor_pos,
            &other_trees,
            wi,
            include_declaration,
        )
    }


    /// Same-file lexical occurrences come back as `Location`s. Even
    /// without a workspace index, references inside the cursor file
    /// are returned — the most basic case.
    #[test]
    fn references_returns_same_file_occurrences() {
        let here = url("file:///a.sv");
        let text = "module m;\n  int foo;\n  initial foo = 1;\nendmodule\n";
        let wi = WorkspaceIndex::default();
        let out = run_references("foo", &here, text, &[], &wi, true);
        assert_eq!(out.len(), 2, "decl + one use");
        assert!(out.iter().all(|l| l.uri == here));
    }


    /// Open buffers other than the cursor file contribute whole-file
    /// lexical matches. The cursor file's hits *and* the other file's
    /// hits both appear.
    #[test]
    fn references_includes_other_open_buffer_usages() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let cursor_text = "module a;\n  my_class c;\nendmodule\n";
        let other_text = "module b;\n  my_class c1;\n  my_class c2;\nendmodule\n";
        let wi = WorkspaceIndex::default();
        let out = run_references(
            "my_class",
            &here,
            cursor_text,
            &[(other.clone(), other_text)],
            &wi,
            true,
        );
        // 1 in cursor file + 2 in other open buffer.
        assert_eq!(out.len(), 3);
        let other_hits = out.iter().filter(|l| l.uri == other).count();
        assert_eq!(other_hits, 2);
    }


    /// Workspace-index declarations from filelist-hydrated (non-open)
    /// files are returned as a fallback. Cursor file has no occurrence
    /// of the name itself — the only result must come from the index.
    #[test]
    fn references_includes_declaration_from_filelist_not_open() {
        let here = url("file:///a.sv");
        let elsewhere = url("file:///lib.sv");
        let cursor_text = "module m;\n  my_class c;\nendmodule\n";
        let elsewhere_text = "class my_class;\nendclass\n";
        let wi = workspace_index_from(&[(&elsewhere, elsewhere_text)]);
        let out = run_references("my_class", &here, cursor_text, &[], &wi, true);
        // 1 usage in cursor file + 1 declaration site from the index.
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|l| l.uri == elsewhere));
    }


    /// A file present in `other_trees` (simulating a closed filelist file
    /// cached in `workspace_trees`) contributes all occurrence sites —
    /// declaration *and* usages — not just the declaration the workspace
    /// index knows about.
    #[test]
    fn references_returns_usages_from_closed_file() {
        let here = url("file:///a.sv");
        let closed = url("file:///lib.sv");
        let cursor_text = "module m;\n  my_class c;\nendmodule\n";
        // closed file: 1 declaration + 2 usages of my_class.
        let closed_text =
            "class my_class;\nendclass\nmodule uses;\n  my_class x;\n  my_class y;\nendmodule\n";
        let wi = workspace_index_from(&[(&closed, closed_text)]);
        // Pass closed file via other_trees (simulates workspace_trees cache).
        let out = run_references(
            "my_class",
            &here,
            cursor_text,
            &[(closed.clone(), closed_text)],
            &wi,
            true,
        );
        let closed_hits = out.iter().filter(|l| l.uri == closed).count();
        // The scoped scan must return all 3 occurrences (decl + 2 usages),
        // not just the 1 declaration the workspace index alone would provide.
        assert!(
            closed_hits > 1,
            "expected usages from closed file, not just declaration; got {closed_hits}"
        );
    }


    /// When a file is both open *and* listed in the filelist, the
    /// open-buffer scan covers it and the workspace-index declaration
    /// site is deduped away — no duplicate `Location` returned.
    #[test]
    fn references_dedupes_when_file_is_both_open_and_in_filelist() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let cursor_text = "module a;\n  my_class c;\nendmodule\n";
        let other_text = "class my_class;\nendclass\n";
        // Index has the declaration; open-buffer scan also finds it.
        let wi = workspace_index_from(&[(&other, other_text)]);
        let out = run_references(
            "my_class",
            &here,
            cursor_text,
            &[(other.clone(), other_text)],
            &wi,
            true,
        );
        // 1 use in cursor file + 1 (deduped) declaration in `other`.
        assert_eq!(out.len(), 2);
        let other_hits = out.iter().filter(|l| l.uri == other).count();
        assert_eq!(other_hits, 1, "open-buffer hit and index decl dedupe");
    }


    /// `include_declaration = false` strips out locations equal to a
    /// known declaration `name_range`, but leaves *usages* alone — even
    /// in the same file as the declaration.
    #[test]
    fn references_respects_include_declaration_false() {
        let here = url("file:///a.sv");
        let elsewhere = url("file:///lib.sv");
        let cursor_text = "module m;\n  my_class c;\nendmodule\n";
        let elsewhere_text = "class my_class;\nendclass\n";
        let wi = workspace_index_from(&[(&elsewhere, elsewhere_text)]);

        let with_decl = run_references("my_class", &here, cursor_text, &[], &wi, true);
        let without_decl = run_references("my_class", &here, cursor_text, &[], &wi, false);

        assert!(with_decl.len() > without_decl.len());
        // The usage in the cursor file is preserved.
        assert!(without_decl.iter().any(|l| l.uri == here));
        // The declaration site in the other file is gone.
        assert!(!without_decl.iter().any(|l| l.uri == elsewhere));
    }


    /// Truncation kicks in at [`REFERENCES_LIMIT`]. Synthesise a file
    /// with many usages and assert the cap, plus that we don't blow up.
    #[test]
    fn references_caps_at_limit() {
        let here = url("file:///a.sv");
        // Build a module body with REFERENCES_LIMIT + 50 references to `foo`.
        let mut text = String::from("module m;\n  int foo;\n");
        for _ in 0..(REFERENCES_LIMIT + 50) {
            text.push_str("  initial foo = 1;\n");
        }
        text.push_str("endmodule\n");
        let wi = WorkspaceIndex::default();
        let out = run_references("foo", &here, &text, &[], &wi, true);
        assert_eq!(out.len(), REFERENCES_LIMIT);
    }


    // ----------------------------------------------------------------------
    // rename — locations_to_workspace_edit
    // ----------------------------------------------------------------------

    /// Helper: build a `WorkspaceEdit` from a vec of `Location`s the same
    /// way the `rename` handler does.
    fn locations_to_workspace_edit(locs: Vec<Location>, new_name: &str) -> WorkspaceEdit {
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for loc in locs {
            changes.entry(loc.uri).or_default().push(TextEdit {
                range: loc.range,
                new_text: new_name.to_owned(),
            });
        }
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }


    /// Rename in the same file: every occurrence is replaced, producing
    /// one entry in `changes`.
    #[test]
    fn rename_single_file_produces_one_entry_per_occurrence() {
        let here = url("file:///a.sv");
        let text = "module m;\n  int foo;\n  initial foo = 1;\nendmodule\n";
        let wi = WorkspaceIndex::default();
        let locs = run_references("foo", &here, text, &[], &wi, true);
        // decl + one use = 2 occurrences
        assert_eq!(locs.len(), 2);

        let edit = locations_to_workspace_edit(locs, "bar");
        let file_edits = edit.changes.unwrap();
        assert_eq!(file_edits.len(), 1, "single file → one URL key");
        let edits = &file_edits[&here];
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|e| e.new_text == "bar"));
    }


    /// Cross-file rename: two URLs should appear in `changes`.
    #[test]
    fn rename_cross_file_produces_entry_per_file() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let cursor_text = "module a;\n  my_class c;\nendmodule\n";
        let other_text = "class my_class;\nendclass\n";
        let wi = workspace_index_from(&[(&other, other_text)]);
        let locs =
            run_references("my_class", &here, cursor_text, &[(other.clone(), other_text)], &wi, true);

        let edit = locations_to_workspace_edit(locs, "renamed_class");
        let file_edits = edit.changes.unwrap();
        assert_eq!(file_edits.len(), 2, "two URLs should appear");
        assert!(file_edits.contains_key(&here));
        assert!(file_edits.contains_key(&other));
        let all_new: Vec<&str> = file_edits
            .values()
            .flatten()
            .map(|e| e.new_text.as_str())
            .collect();
        assert!(all_new.iter().all(|&s| s == "renamed_class"));
    }
}
