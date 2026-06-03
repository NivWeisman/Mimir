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

use std::collections::HashMap;
use std::sync::Arc;

use mimir_ast::{DiagSeverity, MimirAst, MimirDiag, MimirPos, MimirRange};
use mimir_core::Position as MPosition;
use mimir_slang::{
    Diagnostic as SlangDiag, ElaborateParams, ExpandMacroParams, ExpandMacroResult,
    Severity as SlangSeverity,
};
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;
use tracing::{debug, error, warn};

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
    /// All diagnostics produced during compilation, adapted to the
    /// backend-agnostic [`MimirDiag`] shape. Each entry pairs the
    /// file path (as reported by the sidecar) with its diagnostic.
    pub diagnostics: Vec<(String, MimirDiag)>,
}

// --------------------------------------------------------------------------
// Slang → MimirDiag adapter
// --------------------------------------------------------------------------

/// Convert one slang [`SlangDiag`] to the backend-agnostic
/// `(file_path, MimirDiag)` pair.
///
/// Both types use `(line, UTF-16 character)` coordinates and the same
/// four-bucket severity model — the conversion is a field-by-field copy.
/// The file path is extracted from [`SlangDiag::path`] and returned
/// separately so `MimirDiag` stays file-scope (no path field).
fn slang_diag_to_mimir(d: SlangDiag) -> (String, MimirDiag) {
    let diag = MimirDiag {
        range: MimirRange {
            start: MimirPos { line: d.range.start.line, character: d.range.start.character },
            end:   MimirPos { line: d.range.end.line,   character: d.range.end.character   },
        },
        severity: match d.severity {
            SlangSeverity::Error       => DiagSeverity::Error,
            SlangSeverity::Warning     => DiagSeverity::Warning,
            SlangSeverity::Information => DiagSeverity::Information,
            SlangSeverity::Hint        => DiagSeverity::Hint,
        },
        code:    d.code,
        message: d.message,
    };
    (d.path, diag)
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
    /// Per-document cache of recent macro expansions. Keyed by URL; the
    /// stored `(version, entries)` is replaced wholesale when the document
    /// version changes. Lets the hover footer and the panel command share a
    /// single preprocessor run when both land on the same macro usage.
    expansion_cache: Arc<RwLock<HashMap<Url, CachedExpansions>>>,
}

/// All cached expansions for one document at one version.
struct CachedExpansions {
    version: i32,
    /// Each entry is one previously-expanded macro usage. Looked up by
    /// testing whether a cursor falls inside `usage_range`.
    entries: Vec<(MimirRange, ExpandMacroResult)>,
}

/// True when `pos` falls within `[range.start, range.end]` (inclusive),
/// comparing in (line, character) order.
fn range_contains(range: &MimirRange, pos: MPosition) -> bool {
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}

impl SlangAdapter {
    /// Construct the adapter from a shared [`SlangService`].
    pub(crate) fn new(slang: Arc<SlangService>) -> Self {
        Self {
            slang,
            cached_ast: Arc::new(RwLock::new(None)),
            expansion_cache: Arc::new(RwLock::new(HashMap::new())),
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
        mimir_core::time_scope!("slang.compile.adapter_total");
        let compile_result = {
            mimir_core::time_scope!("slang.compile.adapter.rpc");
            self.slang.compile(params).await
        };
        match compile_result {
            Ok(result) => {
                {
                    mimir_core::time_scope!("slang.compile.adapter.cache_ast_write");
                    *self.cached_ast.write().await = Some(Arc::new(result.ast));
                }
                mimir_core::time_scope!("slang.compile.adapter.diag_walk");

                for d in &result.diagnostics {
                    match d.severity {
                        SlangSeverity::Error => {
                            error!(
                                file = %d.path,
                                line = d.range.start.line,
                                code = %d.code,
                                message = %d.message,
                                "[SlangError] compile diagnostic",
                            );
                        }
                        SlangSeverity::Warning => {
                            warn!(
                                file = %d.path,
                                line = d.range.start.line,
                                code = %d.code,
                                message = %d.message,
                                "[SlangError] compile warning",
                            );
                        }
                        _ => {}
                    }
                }

                debug!(
                    files = params.files.len(),
                    "compile RPC succeeded; MimirAst cached"
                );
                Some(CompileOutcome {
                    files_in_request,
                    diagnostics: result.diagnostics.into_iter().map(slang_diag_to_mimir).collect(),
                })
            }
            Err(mimir_slang::ClientError::Busy) => {
                debug!("sidecar busy during compile; retaining previous MimirAst");
                None
            }
            Err(e) => {
                error!(error = %e, "[SlangError] compile RPC failed");
                None
            }
        }
    }

    /// Expand the macro usage at `position` in the document `uri` (version
    /// `version`), going through the cache.
    ///
    /// Returns:
    /// * `Some(result)` — the sidecar answered. `result.found` distinguishes
    ///   "expanded a macro" from "cursor wasn't on a macro usage".
    /// * `None` — the sidecar was busy, errored, or isn't configured.
    ///
    /// On a cache hit (a previously-expanded usage at this URL+version whose
    /// range covers `position`) no RPC is sent. On a miss, the RPC runs and a
    /// successful (`found`) result is cached for the next lookup.
    pub(crate) async fn expand_macro(
        &self,
        uri: &Url,
        version: i32,
        position: MPosition,
        params: &ExpandMacroParams,
    ) -> Option<ExpandMacroResult> {
        mimir_core::time_scope!("slang.compile.adapter.expand_macro");

        // Cache lookup: same URL + version, and the cursor falls inside a
        // previously-expanded usage range.
        {
            let cache = self.expansion_cache.read().await;
            if let Some(c) = cache.get(uri) {
                if c.version == version {
                    if let Some((_, hit)) =
                        c.entries.iter().find(|(r, _)| range_contains(r, position))
                    {
                        debug!(uri = %uri, "macro expansion cache hit");
                        return Some(hit.clone());
                    }
                }
            }
        }

        let result = match self.slang.expand_macro(params).await {
            Ok(r) => r,
            Err(mimir_slang::ClientError::Busy) => {
                debug!("sidecar busy during expand_macro");
                return None;
            }
            Err(e) => {
                error!(error = %e, "[SlangError] expand_macro RPC failed");
                return None;
            }
        };

        // Cache successful hits keyed by their usage range so a follow-up
        // hover/command anywhere on the same macro reuses this compute.
        if result.found {
            if let Some(usage) = &result.usage_range {
                let key_range = MimirRange {
                    start: MimirPos {
                        line: usage.start.line,
                        character: usage.start.character,
                    },
                    end: MimirPos {
                        line: usage.end.line,
                        character: usage.end.character,
                    },
                };
                let mut cache = self.expansion_cache.write().await;
                let bucket = cache.entry(uri.clone()).or_insert_with(|| CachedExpansions {
                    version,
                    entries: Vec::new(),
                });
                if bucket.version != version {
                    bucket.version = version;
                    bucket.entries.clear();
                }
                bucket.entries.push((key_range, result.clone()));
            }
        }

        Some(result)
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
