//! Tree-sitter parse provider: the single owner of the [`SyntaxParser`].
//!
//! All parse operations route through [`TreeSitterProvider`]:
//! incremental single-file parse (with prior-tree reuse), cold parse,
//! and bulk path hydration for workspace index construction.
//!
//! Keeping the parser here — rather than scattered across `Backend`,
//! `SyntaxService`, and the hydration free-function — means there is
//! exactly one `Mutex` lock site for tree-sitter across the entire server.

use std::path::PathBuf;
use std::sync::Arc;

use mimir_syntax::{Symbol, SyntaxParser, SyntaxTree};
use ropey::Rope;
use tokio::sync::Mutex;
use tower_lsp::lsp_types::Url;
use tree_sitter::InputEdit;
use tracing::error;

use crate::workspace_index;

/// The output of a successful single-file parse.
pub(crate) struct ParseResult {
    /// Parse tree produced by tree-sitter.
    pub tree: SyntaxTree,
    /// Parse-error and `MISSING`-node diagnostics extracted from the tree.
    pub diagnostics: Vec<mimir_syntax::Diagnostic>,
    /// Symbol index (declarations, definitions) extracted from the tree.
    pub symbols: Vec<Symbol>,
}

/// Owns the [`SyntaxParser`] and exposes all parse operations.
///
/// `SyntaxParser` is not `Sync`, so it lives behind a `Mutex`.
/// Routing all operations through this type means `Backend` and
/// `SyntaxService` never hold the mutex directly — there is one lock
/// site, one place to reason about parse concurrency.
pub(crate) struct TreeSitterProvider {
    parser: Arc<Mutex<SyntaxParser>>,
}

impl TreeSitterProvider {
    /// Construct the provider. Panics if the SV grammar fails to load —
    /// that is a build-configuration bug, not a recoverable runtime error.
    pub(crate) fn new() -> Self {
        Self {
            parser: Arc::new(Mutex::new(
                SyntaxParser::new().expect("tree-sitter SV grammar failed to load"),
            )),
        }
    }

    /// Parse `text`, optionally reusing `prior_tree` after applying `edits`.
    ///
    /// `prior_tree`, when given, **must** be the tree of the text that
    /// `edits` — applied in order — turn into `text`. The edits are replayed
    /// onto it and it is handed to tree-sitter as the incremental base, so
    /// unchanged subtrees are reused. An empty `edits` slice is valid and
    /// means "`text` is exactly what `prior_tree` was parsed from".
    ///
    /// Pass `None` for a cold parse whenever that precondition can't be
    /// guaranteed (first open, full-document sync, an edit that couldn't be
    /// expressed as an `InputEdit`). tree-sitter does not verify the edit
    /// history: given a base tree that doesn't match, it silently reuses
    /// nodes at stale byte offsets and returns a tree whose ranges can lie
    /// outside `text`.
    ///
    /// Returns `None` on a parser error (already logged at `error!`).
    pub(crate) async fn parse(
        &self,
        text: &str,
        edits: &[InputEdit],
        prior_tree: Option<SyntaxTree>,
    ) -> Option<ParseResult> {
        let prev = prior_tree.map(|mut t| {
            for edit in edits {
                t.tree.edit(edit);
            }
            t
        });

        let tree = {
            let mut parser = self.parser.lock().await;
            match parser.parse(text, prev.as_ref().map(|t| &t.tree)) {
                Ok(t) => t,
                Err(e) => {
                    error!(error = %e, "tree-sitter parse failed");
                    return None;
                }
            }
        };

        let rope = Rope::from_str(text);
        let diagnostics = mimir_syntax::diagnostics::collect(&tree, &rope);
        let symbols = mimir_syntax::symbols::index(&tree, &rope);
        Some(ParseResult {
            tree,
            diagnostics,
            symbols,
        })
    }

    /// Bulk-parse a list of on-disk paths for workspace index hydration.
    ///
    /// Returns `(url, symbols, tree)` for every file that was successfully
    /// read and parsed. Files that fail to read or parse are silently
    /// skipped — a partial workspace index is better than no index at all.
    ///
    /// Runs on the blocking pool with a **private** parser. Hydrating a real
    /// project is seconds of disk I/O and parsing; doing that on the shared
    /// parser meant holding its mutex the whole time, so every interactive
    /// `didOpen` / `didChange` parse — and with it diagnostics, outline and
    /// highlighting — stalled until hydration was done. A `SyntaxParser` is
    /// cheap to build, and the trees it produces are plain data, so there is
    /// nothing to share.
    pub(crate) async fn hydrate_paths(
        &self,
        paths: &[PathBuf],
        include_dirs: &[PathBuf],
    ) -> Vec<(Url, Vec<Symbol>, SyntaxTree)> {
        let paths = paths.to_vec();
        let include_dirs = include_dirs.to_vec();
        let job = tokio::task::spawn_blocking(move || {
            let mut parser = match SyntaxParser::new() {
                Ok(p) => p,
                Err(e) => {
                    error!(error = %e, "hydration: could not create a parser");
                    return Vec::new();
                }
            };
            workspace_index::hydrate_from_paths(&paths, &include_dirs, &mut parser, |path| {
                crate::source_io::read_source_lossy(path)
            })
        });
        match job.await {
            Ok(entries) => entries,
            Err(e) => {
                error!(error = %e, "hydration task failed");
                Vec::new()
            }
        }
    }
}

impl TreeSitterProvider {
    /// Parse exactly one on-disk file — without following its `` `include ``
    /// chain — for a targeted index refresh.
    ///
    /// Returns `None` when the file can't be read. Used when a document is
    /// closed: only that file's entry needs to go back to its on-disk state,
    /// and re-parsing everything it includes (the whole UVM macro tree, for
    /// a typical testbench file) would be pure waste.
    pub(crate) async fn hydrate_file_only(
        &self,
        path: &std::path::Path,
    ) -> Option<(Url, Vec<Symbol>, SyntaxTree)> {
        let path = path.to_path_buf();
        let job = tokio::task::spawn_blocking(move || {
            let mut parser = SyntaxParser::new().ok()?;
            // A reader that only knows the one file makes every include
            // probe miss, so nothing beyond the seed is parsed.
            workspace_index::hydrate_from_paths(
                std::slice::from_ref(&path),
                &[],
                &mut parser,
                |p| (p == path).then(|| crate::source_io::read_source_lossy(p)).flatten(),
            )
            .into_iter()
            .next()
        });
        job.await.ok().flatten()
    }
}

#[cfg(test)]
impl TreeSitterProvider {
    /// Test hook: hold the parser mutex so a test can park in-flight parses
    /// and reproduce the interleavings that parser contention (or tokio's
    /// cooperative yielding) produces in the field.
    pub(crate) async fn lock_parser_for_test(&self) -> tokio::sync::MutexGuard<'_, SyntaxParser> {
        self.parser.lock().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parse_empty_text_succeeds() {
        let provider = TreeSitterProvider::new();
        let result = provider.parse("", &[], None).await;
        assert!(result.is_some(), "empty text should parse without error");
    }

    #[tokio::test]
    async fn parse_returns_symbols_and_clean_diagnostics() {
        let provider = TreeSitterProvider::new();
        let src = "module foo; endmodule\n";
        let result = provider.parse(src, &[], None).await.expect("parse failed");
        assert!(
            result.symbols.iter().any(|s| s.name == "foo"),
            "expected 'foo' in symbol index"
        );
        assert!(
            result.diagnostics.is_empty(),
            "clean module should produce no parse diagnostics"
        );
    }

    #[tokio::test]
    async fn parse_with_prior_tree_succeeds() {
        let provider = TreeSitterProvider::new();
        let src = "module bar; endmodule\n";
        let r1 = provider.parse(src, &[], None).await.expect("first parse");
        // Second parse reusing the prior tree — no edits means the text is
        // unchanged, so the prior tree is a valid incremental base as-is.
        let r2 = provider
            .parse(src, &[], Some(r1.tree))
            .await
            .expect("second parse");
        assert!(r2.symbols.iter().any(|s| s.name == "bar"));
    }

    /// Regression: bulk hydration used to run on the *shared* parser while
    /// holding its mutex for the whole workspace — seconds on a real
    /// project — so every `didOpen` / `didChange` parse queued behind it
    /// and the editor got no diagnostics, outline or highlighting until
    /// hydration finished. Hydration must not need the interactive parser.
    #[tokio::test]
    async fn regression_hydration_does_not_wait_for_the_interactive_parser() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.sv");
        std::fs::write(&file, "module hydrated; endmodule\n").unwrap();

        let provider = TreeSitterProvider::new();
        // Simulate an interactive parse in progress.
        let _interactive = provider.lock_parser_for_test().await;
        let hydrated = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.hydrate_paths(&[file], &[]),
        )
        .await
        .expect("hydration must not block on the interactive parser mutex");
        assert_eq!(hydrated.len(), 1);
        assert!(hydrated[0].1.iter().any(|s| s.name == "hydrated"));
    }

    /// Regression: source files that aren't valid UTF-8 (Latin-1 `©`, `°`,
    /// `ü` in comments are everywhere in legacy RTL) used to be treated as
    /// unreadable and silently dropped from the workspace index — no
    /// go-to-definition, no completion, no references for anything they
    /// declare.
    #[tokio::test]
    async fn regression_non_utf8_file_is_still_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("legacy.sv");
        // "// Copyright \xA9 M\xFCller\nmodule legacy_mod; endmodule\n" in Latin-1.
        let mut bytes = b"// Copyright \xA9 M\xFCller\n".to_vec();
        bytes.extend_from_slice(b"module legacy_mod; endmodule\n");
        std::fs::write(&file, &bytes).unwrap();
        assert!(String::from_utf8(bytes).is_err(), "fixture must be invalid UTF-8");

        let provider = TreeSitterProvider::new();
        let hydrated = provider.hydrate_paths(&[file], &[]).await;
        assert_eq!(hydrated.len(), 1, "file must be read despite the Latin-1 bytes");
        let module = hydrated[0]
            .1
            .iter()
            .find(|s| s.name == "legacy_mod")
            .expect("module indexed");
        assert_eq!(module.name_range.start.line, 1, "line numbers are unaffected");
    }

    /// `hydrate_file_only` parses the named file and nothing it includes.
    #[tokio::test]
    async fn hydrate_file_only_does_not_follow_includes() {
        let dir = tempfile::tempdir().unwrap();
        let header = dir.path().join("h.svh");
        let file = dir.path().join("a.sv");
        std::fs::write(&header, "`define FROM_HEADER 1\n").unwrap();
        std::fs::write(&file, "`include \"h.svh\"\nmodule only_me; endmodule\n").unwrap();

        let provider = TreeSitterProvider::new();
        let (url, symbols, _tree) = provider.hydrate_file_only(&file).await.expect("readable");
        assert_eq!(url, Url::from_file_path(&file).unwrap());
        assert!(symbols.iter().any(|s| s.name == "only_me"));
        assert!(!symbols.iter().any(|s| s.name == "FROM_HEADER"));

        // The full hydration *does* follow the include.
        let all = provider.hydrate_paths(&[file], &[]).await;
        assert_eq!(all.len(), 2);

        assert!(provider.hydrate_file_only(&dir.path().join("nope.sv")).await.is_none());
    }
}
