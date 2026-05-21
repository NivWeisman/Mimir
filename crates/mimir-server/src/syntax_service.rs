//! Syntax service layer: document store access and AST cache.
//!
//! This module encapsulates the three shared Arcs that power every
//! tree-sitter request — the document store, the parser, and the workspace
//! index — behind a clean service interface.
//!
//! [`SyntaxService`] exposes two cached-snapshot methods used by every LSP
//! feature handler that needs a parse tree: [`SyntaxService::cached_tree`]
//! and [`SyntaxService::cached_tree_and_index`]. Both apply a synchronous
//! fallback parse when the cache is cold so callers always get a result as
//! long as the document is open.
//!
//! # Relationship to `Backend`
//!
//! `Backend` creates the three Arcs (`documents`, `parser`, `workspace`)
//! and passes clones to `SyntaxService::new`. Both sides hold Arcs to the
//! **same** underlying data, so mutations made through `Backend`'s fields
//! are immediately visible to `SyntaxService` and vice versa — no
//! synchronization issues beyond the locks already protecting each Arc.

use std::collections::HashMap;
use std::sync::Arc;

use mimir_syntax::{Symbol, SyntaxParser, SyntaxTree};
use tokio::sync::{Mutex, RwLock};
use tower_lsp::lsp_types::Url;
use tracing::error;

use crate::backend::DocumentState;
use crate::workspace_index::WorkspaceState;

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

/// Thin service wrapper around the document store, parser, and workspace.
///
/// Holds the same `Arc`s as [`crate::backend::Backend`] so no data is copied —
/// all operations see the same live state.
pub(crate) struct SyntaxService {
    /// Per-document text + parse state. Written by `did_open` / `did_change` /
    /// `did_close`; read here on every LSP feature request.
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
    /// The shared tree-sitter parser — one instance, `Mutex`-protected because
    /// `tree_sitter::Parser` is not `Sync`.
    parser: Arc<Mutex<SyntaxParser>>,
    /// Workspace-wide symbol index and per-file tree cache. Read here for
    /// cross-file resolution; written by `reparse_and_publish` and the
    /// workspace hydration task.
    #[allow(dead_code)]
    workspace: Arc<RwLock<WorkspaceState>>,
}

impl SyntaxService {
    /// Construct the service from the three shared Arcs created by `Backend`.
    ///
    /// The Arcs are cloned (reference-counted pointer copies, not deep copies)
    /// so both `Backend` and `SyntaxService` operate on the same data.
    pub(crate) fn new(
        documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
        parser: Arc<Mutex<SyntaxParser>>,
        workspace: Arc<RwLock<WorkspaceState>>,
    ) -> Self {
        Self {
            documents,
            parser,
            workspace,
        }
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
        let mut parser = self.parser.lock().await;
        match parser.parse(&text, None) {
            Ok(t) => Some(t),
            Err(e) => {
                error!(error = %e, "fallback parse failed");
                None
            }
        }
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
                let mut parser = self.parser.lock().await;
                match parser.parse(&text, None) {
                    Ok(t) => t,
                    Err(e) => {
                        error!(error = %e, "fallback parse failed");
                        return None;
                    }
                }
            }
        };
        Some((tree, index))
    }
}
