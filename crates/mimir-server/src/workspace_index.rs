//! Workspace-wide tree-sitter symbol index.
//!
//! Stage 2 of `textDocument/definition`: aggregates the per-document
//! `Vec<Symbol>` Stage 1 already builds, plus an eager hydration pass over
//! every file declared in `.mimir.toml`'s filelist. The result is a
//! `name -> [Url, Symbol]` map the server consults when same-file
//! resolution comes up empty.
//!
//! ## Ownership
//!
//! The index is owned by [`Backend`](crate::backend::Backend) behind an
//! `Arc<RwLock<WorkspaceIndex>>`. Two write paths feed it:
//!
//! 1. **Open documents.** [`Backend::reparse_and_publish`] calls
//!    [`WorkspaceIndex::update`] every time a successful parse refreshes a
//!    document's `Vec<Symbol>`.
//! 2. **Filelist files (eager).** On `initialize`, the backend spawns a
//!    one-shot task that calls [`hydrate_from_paths`] for every entry in
//!    `ResolvedProject.files`, then folds the result into the index.
//!
//! Open buffers always win over disk: if a path is both in the filelist
//! and currently open, the `update` from `reparse_and_publish` overwrites
//! the disk-sourced entries.
//!
//! ## Limitation: external edits
//!
//! Until `workspace/didChangeWatchedFiles` is wired up, a filelist file
//! that's edited externally while *not* open in the editor stays at its
//! initialize-time contents. Restart the server to refresh. This matches
//! the README's checklist state — `didChangeWatchedFiles` is its own item.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use mimir_syntax::{Symbol, SyntaxParser, SyntaxTree};
use ropey::Rope;
use tower_lsp::lsp_types::Url;
use tracing::{debug, trace, warn};

/// One declared name pinned to the URL it lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// URL of the document this declaration came from.
    pub url: Url,
    /// The symbol itself — name, kind, ranges.
    pub symbol: Symbol,
}

/// Workspace-wide symbol map.
///
/// Internally four structures kept in sync:
/// - `by_name` for name lookups,
/// - `per_url` to drop a URL's previous entries on re-index without scanning the whole map,
/// - `by_location` for O(1) `(url, line)` → `Entry` lookup used by `try_slang_hover`,
/// - `per_url_lines` to drop location entries on re-index without a full scan.
#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    by_name: HashMap<String, Vec<Entry>>,
    per_url: HashMap<Url, Vec<String>>,
    /// Reverse index: `(url, declaration_line)` → symbol name. Enables O(1)
    /// resolution in `try_slang_hover` which receives a file URL and line
    /// number from slang and needs to find the matching `Symbol`.
    by_location: HashMap<(Url, u32), String>,
    /// Tracks which `(url, line)` pairs were registered for each `url` so
    /// `update` can evict the old location entries without a full scan.
    per_url_lines: HashMap<Url, Vec<u32>>,
}

/// Combined workspace symbol index and parse trees, held under a single lock.
///
/// By merging [`WorkspaceIndex`] and the parse-tree map into one guarded
/// struct, callers acquire exactly one lock instead of two, eliminating the
/// lock-ordering constraint that previously existed between the old
/// `workspace_index` and `workspace_trees` fields on `Backend`.
///
/// Also holds the identifier presence index used to pre-filter the
/// `references` scan: `files_per_name` maps any token that appears
/// in a file to the set of URLs for that file, so `references` can skip
/// files that provably don't mention the identifier at all.
#[derive(Debug, Default)]
pub struct WorkspaceState {
    /// Symbol index — maps declaration names to the files that declare them.
    pub index: WorkspaceIndex,
    /// Parse tree cache — maps URLs to their most recent successful tree.
    pub trees: HashMap<Url, SyntaxTree>,
    /// Presence index: identifier token → set of URLs that contain it.
    /// Built by byte-scanning the source text; much cheaper than full
    /// tree-sitter traversal and sufficient for a pre-filter.
    files_per_name: HashMap<String, HashSet<Url>>,
    /// Reverse of `files_per_name`: URL → set of identifier tokens seen
    /// in that file. Needed to evict entries when a file is re-indexed.
    per_url_names: HashMap<Url, HashSet<String>>,
}

impl WorkspaceState {
    /// Record the identifier tokens present in `url`'s source text.
    ///
    /// Called after every successful parse so the presence index stays in
    /// sync with the tree cache. Re-indexing a URL first removes its prior
    /// name set, then inserts the new one — no stale entries.
    pub fn update_presence(&mut self, url: Url, names: HashSet<String>) {
        // Evict prior entries for this URL.
        if let Some(old_names) = self.per_url_names.remove(&url) {
            for name in old_names {
                if let Some(set) = self.files_per_name.get_mut(&name) {
                    set.remove(&url);
                    if set.is_empty() {
                        self.files_per_name.remove(&name);
                    }
                }
            }
        }
        // Insert new entries.
        for name in &names {
            self.files_per_name
                .entry(name.clone())
                .or_default()
                .insert(url.clone());
        }
        self.per_url_names.insert(url, names);
    }

    /// Return the set of URLs that contain `name` as any identifier token,
    /// or `None` if `name` has never been seen in any indexed file.
    ///
    /// `None` means "no data yet — don't filter"; `Some(empty set)` means
    /// "definitely not in any file". Callers treat `None` as "scan all".
    #[must_use]
    pub fn files_containing(&self, name: &str) -> Option<&HashSet<Url>> {
        self.files_per_name.get(name)
    }
}

impl WorkspaceIndex {
    /// Replace every entry registered for `url` with the supplied
    /// `symbols`. An empty `symbols` slice removes `url` from the index
    /// entirely.
    ///
    /// O(prior_names_for_url + new_names_for_url) — both proportional to
    /// the file's declaration count, not the workspace's.
    pub fn update(&mut self, url: Url, symbols: &[Symbol]) {
        // Drop prior entries for this URL.
        if let Some(prior_names) = self.per_url.remove(&url) {
            for name in prior_names {
                if let Some(bucket) = self.by_name.get_mut(&name) {
                    bucket.retain(|e| e.url != url);
                    if bucket.is_empty() {
                        self.by_name.remove(&name);
                    }
                }
            }
        }
        if let Some(prior_lines) = self.per_url_lines.remove(&url) {
            for line in prior_lines {
                self.by_location.remove(&(url.clone(), line));
            }
        }

        if symbols.is_empty() {
            return;
        }

        let mut new_names: Vec<String> = Vec::with_capacity(symbols.len());
        let mut new_lines: Vec<u32> = Vec::with_capacity(symbols.len());
        for sym in symbols {
            new_names.push(sym.name.clone());
            let line = sym.name_range.start.line;
            new_lines.push(line);
            self.by_location
                .insert((url.clone(), line), sym.name.clone());
            self.by_name
                .entry(sym.name.clone())
                .or_default()
                .push(Entry {
                    url: url.clone(),
                    symbol: sym.clone(),
                });
        }
        self.per_url.insert(url.clone(), new_names);
        self.per_url_lines.insert(url, new_lines);
    }

    /// Look up the entry whose declaration starts at `(url, line)`.
    ///
    /// O(1) via the reverse location index. Used by `try_slang_hover` to
    /// convert a slang-returned `(path, line)` pair into the matching
    /// `Symbol` without scanning the entire index.
    #[must_use]
    pub fn lookup_by_location(&self, url: &Url, line: u32) -> Option<&Entry> {
        let name = self.by_location.get(&(url.clone(), line))?;
        self.by_name
            .get(name)?
            .iter()
            .find(|e| &e.url == url && e.symbol.name_range.start.line == line)
    }

    /// All entries registered under `name`. Empty slice on miss.
    ///
    /// Order is the order in which URLs were `update`d, with each URL's
    /// symbols in source order — stable enough for the editor's peek list,
    /// not load-bearing for correctness.
    #[must_use]
    pub fn lookup(&self, name: &str) -> &[Entry] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Iterate every entry in the index, no filtering or ordering.
    ///
    /// Used by the fuzzy-scoring completion path which needs to consider
    /// all candidates (subsequence matches that aren't prefix matches).
    /// Order is HashMap iteration order — arbitrary but stable in-session;
    /// callers that need ranking apply their own sort.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.by_name.values().flatten()
    }
}

/// Parse every path in `paths` (plus everything they `` `include`` ,
/// transitively) and build a `(Url, Vec<Symbol>, SyntaxTree)` triple for
/// each one we could read. The caller receives both the symbol index
/// (for [`WorkspaceIndex::update`]) and the raw parse tree (for the
/// workspace tree cache used by `textDocument/references`).
///
/// `include_dirs` is consulted by [`crate::includes::expand_includes`] when
/// resolving relative `` `include`` filenames; the file's own directory is
/// tried first, mirroring slang's preprocessor.
///
/// `read_disk` is the disk-read seam — production passes
/// `|p| std::fs::read_to_string(p).ok()`; tests pass an in-memory map.
/// Mirrors the pattern at `assemble_elaborate_params`. The closure is
/// invoked twice per file in the worst case (once during include
/// expansion, once during parsing) — fine for the one-shot eager hydration
/// at startup.
///
/// Paths that the reader returns `None` for are skipped with a `warn!` log
/// — usually means a stale filelist entry pointing at a deleted file.
/// Paths that fail `Url::from_file_path` (only on relative paths, which
/// `ResolvedProject` already absolutises) are also skipped.
///
/// The caller does the `WorkspaceIndex::update` sequencing under the write
/// lock; this function is pure compute on top of the parser, so it can be
/// driven from a `tokio::spawn` without holding any of `Backend`'s state.
#[must_use]
pub fn hydrate_from_paths(
    paths: &[PathBuf],
    include_dirs: &[PathBuf],
    parser: &mut SyntaxParser,
    mut read_disk: impl FnMut(&Path) -> Option<String>,
) -> Vec<(Url, Vec<Symbol>, SyntaxTree)> {
    let all_paths = crate::includes::expand_includes(paths, include_dirs, &mut read_disk);
    let mut out: Vec<(Url, Vec<Symbol>, SyntaxTree)> = Vec::with_capacity(all_paths.len());
    for path in &all_paths {
        let Some(text) = read_disk(path) else {
            warn!(path = %path.display(), "workspace index: file unreadable; skipping");
            continue;
        };
        let Ok(url) = Url::from_file_path(path) else {
            warn!(path = %path.display(), "workspace index: cannot URL-encode path; skipping");
            continue;
        };
        match parser.parse(&text, None) {
            Ok(tree) => {
                let rope = Rope::from_str(&text);
                let symbols = mimir_syntax::symbols::index(&tree, &rope);
                trace!(
                    path = %path.display(),
                    count = symbols.len(),
                    "workspace index: parsed",
                );
                out.push((url, symbols, tree));
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "workspace index: parse failed");
            }
        }
    }
    debug!(
        parsed = out.len(),
        seeds = paths.len(),
        expanded = all_paths.len(),
        "hydrate_from_paths done"
    );
    out
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::{Position, Range};
    use mimir_syntax::SymbolKind;

    /// Build an `Symbol` with arbitrary-but-distinct ranges. Tests only
    /// care about identity/order, not exact span numbers.
    fn sym(name: &str, kind: SymbolKind, line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            full_range: Range::new(Position::new(line, 0), Position::new(line, 10)),
            params: None,
            parent_class_name: None,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// `update` replaces a URL's prior entries — re-indexing the same file
    /// must not append duplicates.
    #[test]
    fn update_replaces_prior_entries_for_url() {
        let mut idx = WorkspaceIndex::default();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("foo", SymbolKind::Module, 0)]);
        idx.update(u.clone(), &[sym("bar", SymbolKind::Class, 0)]);

        // The Module 'foo' should no longer be indexed.
        assert!(idx.lookup("foo").is_empty());
        // The Class 'bar' is the only thing left for this URL.
        let bar = idx.lookup("bar");
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].url, u);
        assert_eq!(bar[0].symbol.kind, SymbolKind::Class);
    }

    /// `update` with an empty slice deletes the URL's contribution
    /// entirely. Used when a parse fails or a doc is closed-then-cleared.
    #[test]
    fn update_with_empty_removes_url_entries() {
        let mut idx = WorkspaceIndex::default();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("foo", SymbolKind::Module, 0)]);
        idx.update(u, &[]);
        assert!(idx.lookup("foo").is_empty());
    }

    /// Lookup returns matches across multiple URLs, in update order.
    #[test]
    fn lookup_returns_matches_across_urls() {
        let mut idx = WorkspaceIndex::default();
        let a = url("file:///a.sv");
        let b = url("file:///b.sv");
        idx.update(a.clone(), &[sym("shared", SymbolKind::Class, 1)]);
        idx.update(b.clone(), &[sym("shared", SymbolKind::Class, 2)]);

        let hits = idx.lookup("shared");
        assert_eq!(hits.len(), 2);
        let urls: Vec<&Url> = hits.iter().map(|e| &e.url).collect();
        assert!(urls.contains(&&a));
        assert!(urls.contains(&&b));
    }

    /// Lookup of an unknown name returns the empty slice (not a panic).
    #[test]
    fn lookup_missing_name_returns_empty() {
        let idx = WorkspaceIndex::default();
        assert!(idx.lookup("whatever").is_empty());
    }

    /// `hydrate_from_paths` parses each readable path and emits its
    /// symbols. Verifies the disk-read seam by feeding a stub.
    #[test]
    fn hydrate_from_paths_parses_each_readable_path() {
        let mut parser = SyntaxParser::new().unwrap();
        let p1 = PathBuf::from("/proj/a.sv");
        let p2 = PathBuf::from("/proj/b.sv");
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (p1.clone(), "module a; endmodule\n".to_string()),
            (p2.clone(), "class b; endclass\n".to_string()),
        ]);

        let result = hydrate_from_paths(
            &[p1.clone(), p2.clone()],
            &[],
            &mut parser,
            |p| texts.get(p).cloned(),
        );

        assert_eq!(result.len(), 2);
        // a.sv -> module 'a'
        let (u_a, syms_a, _tree_a) = &result[0];
        assert_eq!(u_a, &Url::from_file_path(&p1).unwrap());
        assert!(syms_a
            .iter()
            .any(|s| s.name == "a" && s.kind == SymbolKind::Module));
        // b.sv -> class 'b'
        let (u_b, syms_b, _tree_b) = &result[1];
        assert_eq!(u_b, &Url::from_file_path(&p2).unwrap());
        assert!(syms_b
            .iter()
            .any(|s| s.name == "b" && s.kind == SymbolKind::Class));
    }

    /// `entries()` yields every registered entry across all URLs.
    /// Order is unspecified (HashMap iteration), but every entry must
    /// appear exactly once.
    #[test]
    fn entries_yields_every_registered_entry() {
        let mut idx = WorkspaceIndex::default();
        idx.update(
            url("file:///a.sv"),
            &[
                sym("my_class", SymbolKind::Class, 0),
                sym("my_module", SymbolKind::Module, 1),
            ],
        );
        idx.update(
            url("file:///b.sv"),
            &[sym("other", SymbolKind::Package, 2)],
        );
        let names: Vec<&str> = idx.entries().map(|e| e.symbol.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"my_class"));
        assert!(names.contains(&"my_module"));
        assert!(names.contains(&"other"));
    }

    /// Paths the reader can't satisfy are skipped (no panic, no entry).
    #[test]
    fn hydrate_from_paths_skips_unreadable_paths() {
        let mut parser = SyntaxParser::new().unwrap();
        let ok = PathBuf::from("/proj/ok.sv");
        let missing = PathBuf::from("/proj/missing.sv");
        let texts: HashMap<PathBuf, String> =
            HashMap::from([(ok.clone(), "module ok; endmodule\n".to_string())]);

        let result = hydrate_from_paths(
            &[ok.clone(), missing],
            &[],
            &mut parser,
            |p| texts.get(p).cloned(),
        );

        assert_eq!(result.len(), 1, "missing path should be dropped silently");
        assert_eq!(result[0].0, Url::from_file_path(&ok).unwrap());
    }

    /// `hydrate_from_paths` follows `` `include`` directives, so a single
    /// seed file can pull `uvm_pkg.sv` (and its content) into the workspace
    /// index even though only `uvm.sv` is in the filelist.
    #[test]
    fn hydrate_from_paths_follows_includes() {
        let mut parser = SyntaxParser::new().unwrap();
        let umbrella = PathBuf::from("/proj/uvm.sv");
        let pkg = PathBuf::from("/uvm/src/uvm_pkg.sv");
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (umbrella.clone(), "`include \"uvm_pkg.sv\"\n".to_string()),
            (
                pkg.clone(),
                "package uvm_pkg; class uvm_object; endclass endpackage\n".to_string(),
            ),
        ]);

        let result = hydrate_from_paths(
            &[umbrella.clone()],
            &[PathBuf::from("/uvm/src")],
            &mut parser,
            |p| texts.get(p).cloned(),
        );

        // Both files end up in the index — uvm.sv from the seed, uvm_pkg.sv
        // from the include expansion.
        assert_eq!(result.len(), 2);
        let names: Vec<String> = result
            .iter()
            .flat_map(|(_, syms, _)| syms.iter().map(|s| s.name.clone()))
            .collect();
        assert!(
            names.contains(&"uvm_pkg".to_string()),
            "expected uvm_pkg in {names:?}"
        );
        assert!(
            names.contains(&"uvm_object".to_string()),
            "expected uvm_object in {names:?}"
        );
    }

    // ── WorkspaceState presence index tests ──────────────────────────────

    fn url_set(urls: &[&str]) -> HashSet<Url> {
        urls.iter().map(|s| Url::parse(s).unwrap()).collect()
    }

    /// After `update_presence`, `files_containing` returns the URL.
    #[test]
    fn files_containing_returns_url_after_update() {
        let mut ws = WorkspaceState::default();
        let u = Url::parse("file:///a.sv").unwrap();
        let names: HashSet<String> = ["foo", "bar"].iter().map(|s| s.to_string()).collect();
        ws.update_presence(u.clone(), names);

        let hits = ws.files_containing("foo").expect("foo should be present");
        assert!(hits.contains(&u));
    }

    /// Re-indexing a URL removes old name entries and inserts new ones.
    #[test]
    fn update_presence_replaces_prior_names() {
        let mut ws = WorkspaceState::default();
        let u = Url::parse("file:///a.sv").unwrap();
        ws.update_presence(u.clone(), ["alpha"].iter().map(|s| s.to_string()).collect());
        ws.update_presence(u.clone(), ["beta"].iter().map(|s| s.to_string()).collect());

        assert!(ws.files_containing("alpha").is_none(), "old name evicted");
        assert!(
            ws.files_containing("beta").unwrap().contains(&u),
            "new name present"
        );
    }

    /// `files_containing` returns `None` for a name not in any indexed file.
    #[test]
    fn files_containing_returns_none_for_unknown_name() {
        let ws = WorkspaceState::default();
        assert!(ws.files_containing("whatever").is_none());
    }

    /// Names shared across two URLs appear in both URL sets.
    #[test]
    fn files_containing_multiple_urls() {
        let mut ws = WorkspaceState::default();
        let a = Url::parse("file:///a.sv").unwrap();
        let b = Url::parse("file:///b.sv").unwrap();
        ws.update_presence(a.clone(), ["shared"].iter().map(|s| s.to_string()).collect());
        ws.update_presence(b.clone(), ["shared"].iter().map(|s| s.to_string()).collect());

        let hits = ws.files_containing("shared").unwrap();
        assert!(hits.contains(&a));
        assert!(hits.contains(&b));
        let _ = url_set(&[]); // suppress unused-fn warning
    }

    // ── lookup_by_location tests ──────────────────────────────────────────

    /// `lookup_by_location` finds the symbol whose declaration starts at the
    /// exact `(url, line)` pair.
    #[test]
    fn lookup_by_location_finds_exact_match() {
        let mut idx = WorkspaceIndex::default();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("my_module", SymbolKind::Module, 5)]);

        let entry = idx.lookup_by_location(&u, 5).expect("should find at line 5");
        assert_eq!(entry.symbol.name, "my_module");
        assert_eq!(entry.url, u);
    }

    /// `lookup_by_location` returns `None` for a line with no declaration.
    #[test]
    fn lookup_by_location_miss_on_unknown_line() {
        let mut idx = WorkspaceIndex::default();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("my_module", SymbolKind::Module, 5)]);

        assert!(idx.lookup_by_location(&u, 99).is_none());
    }

    /// Re-indexing a URL replaces the old location entries — the old line
    /// must no longer be found.
    #[test]
    fn lookup_by_location_replaced_on_reindex() {
        let mut idx = WorkspaceIndex::default();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("foo", SymbolKind::Module, 3)]);
        // Re-index at a different line.
        idx.update(u.clone(), &[sym("foo", SymbolKind::Module, 7)]);

        assert!(idx.lookup_by_location(&u, 3).is_none(), "old line evicted");
        assert!(idx.lookup_by_location(&u, 7).is_some(), "new line present");
    }

    /// `update` with an empty slice evicts all location entries for the URL.
    #[test]
    fn lookup_by_location_cleared_on_empty_update() {
        let mut idx = WorkspaceIndex::default();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("foo", SymbolKind::Module, 2)]);
        idx.update(u.clone(), &[]);

        assert!(idx.lookup_by_location(&u, 2).is_none(), "entry must be gone");
    }
}
