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
    /// When `edits` is non-empty and `prior_tree` is `Some`, the edits are
    /// applied to the prior tree before handing it to tree-sitter so that
    /// unchanged subtrees can be reused (incremental parse). When `edits`
    /// is empty (first open or full-sync `didChange`), a cold parse runs
    /// from scratch.
    ///
    /// Returns `None` on a parser error (already logged at `error!`).
    pub(crate) async fn parse(
        &self,
        text: &str,
        edits: &[InputEdit],
        prior_tree: Option<SyntaxTree>,
    ) -> Option<ParseResult> {
        let prev = if !edits.is_empty() {
            prior_tree.map(|mut t| {
                for edit in edits {
                    t.tree.edit(edit);
                }
                t
            })
        } else {
            None
        };

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
    pub(crate) async fn hydrate_paths(
        &self,
        paths: &[PathBuf],
        include_dirs: &[PathBuf],
    ) -> Vec<(Url, Vec<Symbol>, SyntaxTree)> {
        let mut parser = self.parser.lock().await;
        workspace_index::hydrate_from_paths(paths, include_dirs, &mut parser, |path| {
            std::fs::read_to_string(path).ok()
        })
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
        // Second parse reusing the prior tree — no edits means cold re-parse
        // with the old tree as a hint.
        let r2 = provider
            .parse(src, &[], Some(r1.tree))
            .await
            .expect("second parse");
        assert!(r2.symbols.iter().any(|s| s.name == "bar"));
    }
}
