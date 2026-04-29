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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mimir_syntax::{Symbol, SyntaxParser};
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
/// Internally two structures kept in sync: `by_name` for lookups, `per_url`
/// to drop a URL's previous entries on re-index without scanning the whole
/// map.
#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    by_name: HashMap<String, Vec<Entry>>,
    per_url: HashMap<Url, Vec<String>>,
}

impl WorkspaceIndex {
    /// Empty index. Backend constructs one of these in `Backend::new`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

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

        if symbols.is_empty() {
            return;
        }

        let mut new_names: Vec<String> = Vec::with_capacity(symbols.len());
        for sym in symbols {
            new_names.push(sym.name.clone());
            self.by_name
                .entry(sym.name.clone())
                .or_default()
                .push(Entry {
                    url: url.clone(),
                    symbol: sym.clone(),
                });
        }
        self.per_url.insert(url, new_names);
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
}

/// Parse every path in `paths` and build a `(Url, Vec<Symbol>)` pair for
/// each one we could read.
///
/// `read_disk` is the disk-read seam — production passes
/// `|p| std::fs::read_to_string(p).ok()`; tests pass an in-memory map.
/// Mirrors the pattern at `assemble_elaborate_params`.
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
    parser: &mut SyntaxParser,
    mut read_disk: impl FnMut(&Path) -> Option<String>,
) -> Vec<(Url, Vec<Symbol>)> {
    let mut out: Vec<(Url, Vec<Symbol>)> = Vec::with_capacity(paths.len());
    for path in paths {
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
                out.push((url, symbols));
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "workspace index: parse failed");
            }
        }
    }
    debug!(
        parsed = out.len(),
        requested = paths.len(),
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
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// `update` replaces a URL's prior entries — re-indexing the same file
    /// must not append duplicates.
    #[test]
    fn update_replaces_prior_entries_for_url() {
        let mut idx = WorkspaceIndex::new();
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
        let mut idx = WorkspaceIndex::new();
        let u = url("file:///a.sv");
        idx.update(u.clone(), &[sym("foo", SymbolKind::Module, 0)]);
        idx.update(u, &[]);
        assert!(idx.lookup("foo").is_empty());
    }

    /// Lookup returns matches across multiple URLs, in update order.
    #[test]
    fn lookup_returns_matches_across_urls() {
        let mut idx = WorkspaceIndex::new();
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
        let idx = WorkspaceIndex::new();
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

        let result = hydrate_from_paths(&[p1.clone(), p2.clone()], &mut parser, |p| {
            texts.get(p).cloned()
        });

        assert_eq!(result.len(), 2);
        // a.sv -> module 'a'
        let (u_a, syms_a) = &result[0];
        assert_eq!(u_a, &Url::from_file_path(&p1).unwrap());
        assert!(syms_a
            .iter()
            .any(|s| s.name == "a" && s.kind == SymbolKind::Module));
        // b.sv -> class 'b'
        let (u_b, syms_b) = &result[1];
        assert_eq!(u_b, &Url::from_file_path(&p2).unwrap());
        assert!(syms_b
            .iter()
            .any(|s| s.name == "b" && s.kind == SymbolKind::Class));
    }

    /// Paths the reader can't satisfy are skipped (no panic, no entry).
    #[test]
    fn hydrate_from_paths_skips_unreadable_paths() {
        let mut parser = SyntaxParser::new().unwrap();
        let ok = PathBuf::from("/proj/ok.sv");
        let missing = PathBuf::from("/proj/missing.sv");
        let texts: HashMap<PathBuf, String> =
            HashMap::from([(ok.clone(), "module ok; endmodule\n".to_string())]);

        let result = hydrate_from_paths(&[ok.clone(), missing], &mut parser, |p| {
            texts.get(p).cloned()
        });

        assert_eq!(result.len(), 1, "missing path should be dropped silently");
        assert_eq!(result[0].0, Url::from_file_path(&ok).unwrap());
    }
}
