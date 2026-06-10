//! Syntax service layer: document-store snapshot access.
//!
//! [`SyntaxService`] holds the shared document store and exposes two
//! cached-snapshot methods used by every LSP feature handler that needs
//! a parse tree: [`SyntaxService::cached_tree`] and
//! [`SyntaxService::cached_tree_and_index`].
//!
//! Parse operations are delegated to [`crate::parse_provider::TreeSitterProvider`]
//! so this module's single responsibility is document state access, not
//! parsing.
//!
//! # Relationship to `Backend`
//!
//! `Backend` creates the shared Arcs and passes clones here. Both sides
//! operate on the **same** underlying data — mutations through `Backend`'s
//! fields are immediately visible to `SyntaxService` and vice versa.

use std::collections::HashMap;
use std::sync::Arc;

use mimir_syntax::{Symbol, SyntaxTree};
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;

use crate::backend::DocumentState;
use crate::parse_provider::TreeSitterProvider;

/// Same-file vs cross-file syntax-side completion candidates.
///
/// Returned by [`crate::backend::Backend::gather_syntax_candidates`]. Streams
/// are kept separate so callers can prioritise same-file hits in their dedup
/// pass.
pub(crate) struct SyntaxCandidates {
    /// Symbols from the document currently being edited (higher priority).
    pub(crate) same_file: Vec<Symbol>,
    /// Symbols from all other files in the workspace index (lower priority).
    pub(crate) cross_file: Vec<(Url, Symbol)>,
}

/// Thin service wrapper around the document store.
///
/// Holds the same `Arc`s as [`crate::backend::Backend`] so no data is copied —
/// all operations see the same live state. Parse operations are delegated to
/// [`TreeSitterProvider`] rather than held here.
pub(crate) struct SyntaxService {
    /// Per-document text + parse state. Written by `did_open` / `did_change` /
    /// `did_close`; read here on every LSP feature request.
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
    /// Tree-sitter parse provider. Used for cache-miss fallback parses.
    ts: Arc<TreeSitterProvider>,
}

impl SyntaxService {
    /// Construct the service from the shared Arcs created by `Backend`.
    ///
    /// The Arcs are cloned (reference-counted pointer copies, not deep copies)
    /// so both `Backend` and `SyntaxService` operate on the same data.
    pub(crate) fn new(
        documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
        ts: Arc<TreeSitterProvider>,
    ) -> Self {
        Self { documents, ts }
    }

    /// Snapshot the cached parse tree for `uri`.
    ///
    /// Returns `None` when the document isn't open. When the document
    /// *is* open but the cache hasn't been populated yet (rare — only
    /// between `did_open` and the first `reparse_and_publish` finishing,
    /// or after every parse so far has errored) we fall back to a
    /// synchronous full parse so the caller always gets a tree if the
    /// document exists.
    pub(crate) async fn cached_tree(&self, uri: &Url) -> Option<SyntaxTree> {
        let (cached, fallback_text) = {
            let docs = self.documents.read().await;
            let state = docs.get(uri)?;
            match &state.tree {
                Some(t) => (Some(t.clone()), None),
                None => (None, Some(state.document.text())),
            }
        };
        if let Some(t) = cached {
            return Some(t);
        }
        let text = fallback_text?;
        self.ts.parse(&text, &[], None).await.map(|r| r.tree)
    }

    /// Snapshot the cached tree *and* symbol index for `uri` together.
    ///
    /// Acquires the doc-store read lock once for both. Falls back to a
    /// synchronous parse if the tree cache is empty; the returned
    /// `index` may be empty in that case (it is only populated by
    /// `reparse_and_publish`).
    pub(crate) async fn cached_tree_and_index(
        &self,
        uri: &Url,
    ) -> Option<(SyntaxTree, Vec<Symbol>)> {
        let (cached, fallback_text, index) = {
            let docs = self.documents.read().await;
            let state = docs.get(uri)?;
            let cached = state.tree.clone();
            let fallback_text = if cached.is_none() {
                Some(state.document.text())
            } else {
                None
            };
            (cached, fallback_text, state.index.clone())
        };
        let tree = match cached {
            Some(t) => t,
            None => {
                let text = fallback_text?;
                self.ts.parse(&text, &[], None).await?.tree
            }
        };
        Some((tree, index))
    }
}
