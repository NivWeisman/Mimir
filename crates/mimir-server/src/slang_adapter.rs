//! SlangAdapter: drives the compile RPC and caches the resulting MimirAst.
//!
//! This module bridges [`SlangService`] (sidecar IPC) and the LSP feature
//! layer (Phase 5). Its single responsibility is the compile round-trip:
//! accept pre-assembled [`ElaborateParams`] and the list of URLs that were
//! in the request, send the `compile` RPC, deserialise the response into a
//! [`MimirAst`], cache it, and return a [`CompileOutcome`] that
//! [`crate::elaborate_service::ElaborateService`] can use for diagnostic
//! publishing.
//!
//! [`Backend`] reads [`SlangAdapter::cached_ast`] to answer LSP feature
//! queries (goto-definition, completion, hover, etc.) without blocking on
//! the next compile cycle.

use std::sync::Arc;

use mimir_ast::MimirAst;
use mimir_slang::{Diagnostic as SlangDiagnostic, ElaborateParams};
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;
use tracing::{debug, error};

use crate::slang_service::SlangService;

// --------------------------------------------------------------------------
// Public types
// --------------------------------------------------------------------------

/// Everything the caller needs after a successful compile round.
pub(crate) struct CompileOutcome {
    /// URLs that were included in the compile request. Used by
    /// [`crate::elaborate_service::ElaborateService`] to decide which
    /// files to clear diagnostics for.
    pub files_in_request: Vec<Url>,
    /// All diagnostics produced during compilation.
    pub diagnostics: Vec<SlangDiagnostic>,
}

// --------------------------------------------------------------------------
// SlangAdapter
// --------------------------------------------------------------------------

/// Caches the latest [`MimirAst`] from the slang sidecar `compile` RPC.
///
/// Constructed from an [`Arc<SlangService>`] that it shares with
/// [`crate::elaborate_service::ElaborateService`]. The adapter owns exactly
/// one piece of state: the cached AST. Debounce and input-hash dedup stay
/// in [`crate::elaborate_service::ElaborateService`].
pub(crate) struct SlangAdapter {
    slang: Arc<SlangService>,
    cached_ast: Arc<RwLock<Option<Arc<MimirAst>>>>,
}

impl SlangAdapter {
    /// Construct the adapter from a shared [`SlangService`].
    pub(crate) fn new(slang: Arc<SlangService>) -> Self {
        Self {
            slang,
            cached_ast: Arc::new(RwLock::new(None)),
        }
    }

    /// Borrow the underlying [`SlangService`] for configuration queries
    /// (debounce, feature toggles, project config, etc.).
    pub(crate) fn slang(&self) -> &Arc<SlangService> {
        &self.slang
    }

    /// Run the `compile` RPC with pre-assembled params, cache the resulting
    /// [`MimirAst`], and return a [`CompileOutcome`].
    ///
    /// Returns `None` on sidecar error (busy, I/O, decode). On `Busy` the
    /// cached AST from the previous round is still valid; on other errors it
    /// is left unchanged and the caller should log accordingly.
    pub(crate) async fn compile(
        &self,
        params: &ElaborateParams,
        files_in_request: Vec<Url>,
    ) -> Option<CompileOutcome> {
        match self.slang.compile(params).await {
            Ok(result) => {
                *self.cached_ast.write().await = Some(Arc::new(result.ast));
                debug!(
                    files = params.files.len(),
                    "compile RPC succeeded; MimirAst cached"
                );
                Some(CompileOutcome {
                    files_in_request,
                    diagnostics: result.diagnostics,
                })
            }
            Err(mimir_slang::ClientError::Busy) => {
                debug!("sidecar busy during compile; retaining previous MimirAst");
                None
            }
            Err(e) => {
                error!(error = %e, "slang compile RPC failed");
                None
            }
        }
    }

    /// Return the cached [`MimirAst`] from the last successful compile.
    ///
    /// Returns `None` if no compile has completed yet (e.g. on startup before
    /// the first background elaboration fires).
    pub(crate) async fn cached_ast(&self) -> Option<Arc<MimirAst>> {
        self.cached_ast.read().await.clone()
    }

    /// Discard the cached AST.
    ///
    /// Call this after a project reload so stale symbol data is not used for
    /// LSP features while the new compile is in flight.
    #[allow(dead_code)]
    pub(crate) async fn invalidate(&self) {
        *self.cached_ast.write().await = None;
        debug!("cached MimirAst invalidated");
    }
}
