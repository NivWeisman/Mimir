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
    /// Per-document cache of recent macro expansions, keyed by URL. Unlike the
    /// AST cache this is *not* wiped on every edit: entries from older document
    /// versions are retained (up to [`MAX_EXPANSIONS_PER_DOC`]) so a hover
    /// during a busy or stuck elaborate can fall back to the last-good
    /// expansion. Lets the hover footer and the panel command share a single
    /// preprocessor run when both land on the same macro usage.
    expansion_cache: Arc<RwLock<HashMap<Url, CachedExpansions>>>,
}

/// Max expansions retained per document. Bounds memory while keeping enough
/// history that a stale-fallback lookup almost always still covers the macro
/// the user is hovering. Oldest entries are evicted first.
const MAX_EXPANSIONS_PER_DOC: usize = 64;

/// Recent macro expansions for one document, oldest first.
struct CachedExpansions {
    entries: Vec<CachedExpansion>,
}

/// One previously-expanded macro usage, tagged with the document version it
/// was computed against so a lookup can distinguish a *fresh* hit (same
/// version) from a *stale* fallback (any version).
struct CachedExpansion {
    /// Document version this expansion was computed against.
    version: i32,
    /// Macro-usage range, in `version`'s coordinates, used to match a cursor.
    range: MimirRange,
    /// The expansion result.
    result: ExpandMacroResult,
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

        if let Some(hit) = self.cached_expansion(uri, version, position).await {
            debug!(uri = %uri, "macro expansion cache hit");
            return Some(hit);
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

        if result.found {
            self.store_expansion(uri, version, &result).await;
        }
        Some(result)
    }

    /// Non-blocking counterpart to [`Self::expand_macro`] for the
    /// opportunistic hover footer.
    ///
    /// Identical caching, but on a cache miss it routes through
    /// [`SlangService::expand_macro_if_idle`], which returns immediately when
    /// the sidecar connection is busy with a background elaborate. A hover must
    /// never block on a multi-second compile; when the sidecar is occupied this
    /// returns `None` and the hover simply omits the expansion footer (the next
    /// idle hover fills it in, and the result is then cached for instant reuse).
    pub(crate) async fn expand_macro_if_idle(
        &self,
        uri: &Url,
        version: i32,
        position: MPosition,
        params: &ExpandMacroParams,
    ) -> Option<ExpandMacroResult> {
        mimir_core::time_scope!("slang.compile.adapter.expand_macro");

        if let Some(hit) = self.cached_expansion(uri, version, position).await {
            debug!(uri = %uri, "macro expansion cache hit");
            return Some(hit);
        }

        let result = match self.slang.expand_macro_if_idle(params).await {
            Ok(r) => r,
            Err(mimir_slang::ClientError::Busy) => {
                debug!("sidecar busy; skipping opportunistic hover macro footer");
                return None;
            }
            Err(e) => {
                error!(error = %e, "[SlangError] expand_macro RPC failed");
                return None;
            }
        };

        if result.found {
            self.store_expansion(uri, version, &result).await;
        }
        Some(result)
    }

    /// Look up a **fresh** previously-expanded usage: same `uri`, same document
    /// `version`, and a cached range covering `position`. Returns `None` on a
    /// version mismatch (the document was edited) or when no current-version
    /// usage contains the cursor. Newest matching entry wins.
    pub(crate) async fn cached_expansion(
        &self,
        uri: &Url,
        version: i32,
        position: MPosition,
    ) -> Option<ExpandMacroResult> {
        let cache = self.expansion_cache.read().await;
        let bucket = cache.get(uri)?;
        bucket
            .entries
            .iter()
            .rev()
            .find(|e| e.version == version && range_contains(&e.range, position))
            .map(|e| e.result.clone())
    }

    /// Best-effort **stale** fallback for when the sidecar can't produce a
    /// fresh expansion (busy with an elaborate, or unresponsive): return the
    /// most recent cached expansion whose usage range still covers `position`,
    /// *regardless of document version*. The expansion may be out of date if
    /// the macro — or a macro it expands to — changed since it was computed, so
    /// callers must mark it as possibly-stale. Returns `None` when nothing in
    /// the cache covers the cursor (e.g. the macro moved after an edit above
    /// it, or it was never expanded this session).
    pub(crate) async fn stale_expansion(
        &self,
        uri: &Url,
        position: MPosition,
    ) -> Option<ExpandMacroResult> {
        let cache = self.expansion_cache.read().await;
        let bucket = cache.get(uri)?;
        bucket
            .entries
            .iter()
            .rev()
            .find(|e| range_contains(&e.range, position))
            .map(|e| e.result.clone())
    }

    /// Cache a successful expansion keyed by its usage range so a follow-up
    /// hover/command on the same macro reuses this preprocessor run.
    ///
    /// Unlike the AST cache, this does **not** clear on a version bump: prior
    /// entries are kept so [`Self::stale_expansion`] can serve a last-good
    /// result while a new elaborate is in flight. A re-expansion at the same
    /// range replaces the old entry (so the newest result for a usage wins and
    /// duplicates don't accumulate); the bucket is capped at
    /// [`MAX_EXPANSIONS_PER_DOC`], evicting oldest-first.
    async fn store_expansion(&self, uri: &Url, version: i32, result: &ExpandMacroResult) {
        let Some(usage) = &result.usage_range else {
            return;
        };
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
        let bucket = cache
            .entry(uri.clone())
            .or_insert_with(|| CachedExpansions { entries: Vec::new() });
        // One entry per usage range: drop any prior expansion at the same range
        // (from this or an earlier version) and append the newest.
        bucket.entries.retain(|e| e.range != key_range);
        bucket.entries.push(CachedExpansion {
            version,
            range: key_range,
            result: result.clone(),
        });
        let len = bucket.entries.len();
        if len > MAX_EXPANSIONS_PER_DOC {
            bucket.entries.drain(0..len - MAX_EXPANSIONS_PER_DOC);
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

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::Range as CoreRange;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    /// An adapter with no configured sidecar — enough to exercise the
    /// expansion cache, which never touches the connection.
    fn adapter() -> SlangAdapter {
        let docs = Arc::new(RwLock::new(HashMap::new()));
        SlangAdapter::new(Arc::new(SlangService::new(docs, None)))
    }

    /// A `found` expansion whose usage spans `[start, end]` (line, character).
    fn result_spanning(start: (u32, u32), end: (u32, u32)) -> ExpandMacroResult {
        result_spanning_text(start, end, "((x)+1)")
    }

    /// Like [`result_spanning`] but with a caller-chosen `expanded_text`, so a
    /// test can tell two expansions at the same range apart.
    fn result_spanning_text(start: (u32, u32), end: (u32, u32), text: &str) -> ExpandMacroResult {
        ExpandMacroResult {
            found: true,
            expanded_text: text.into(),
            macro_name: "B".into(),
            usage_range: Some(CoreRange::new(
                MPosition::new(start.0, start.1),
                MPosition::new(end.0, end.1),
            )),
            line_count: 1,
            diagnostics: Vec::new(),
        }
    }

    /// A stored expansion is served back for any cursor inside its usage
    /// range and missed for one outside it.
    #[tokio::test]
    async fn cached_expansion_round_trips_within_usage_range() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        a.store_expansion(&uri, 1, &result_spanning((3, 10), (3, 16))).await;

        let hit = a.cached_expansion(&uri, 1, MPosition::new(3, 12)).await;
        assert_eq!(hit.expect("cursor inside range should hit").macro_name, "B");

        assert!(
            a.cached_expansion(&uri, 1, MPosition::new(4, 0)).await.is_none(),
            "cursor outside the usage range must miss"
        );
    }

    /// A document edit (version bump) makes the *fresh* lookup miss — it only
    /// matches the current version — even though the entry is retained for the
    /// stale fallback.
    #[tokio::test]
    async fn cached_expansion_misses_on_version_bump() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        a.store_expansion(&uri, 1, &result_spanning((0, 4), (0, 10))).await;

        assert!(
            a.cached_expansion(&uri, 2, MPosition::new(0, 6)).await.is_none(),
            "a newer version must not see the version-1 entry as fresh"
        );
    }

    /// The stale fallback serves a prior-version expansion (the fresh lookup
    /// can't), so the hover footer survives an edit while slang re-elaborates.
    #[tokio::test]
    async fn stale_expansion_serves_across_version_bump() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        a.store_expansion(&uri, 1, &result_spanning((3, 10), (3, 16))).await;

        assert!(
            a.cached_expansion(&uri, 2, MPosition::new(3, 12)).await.is_none(),
            "fresh lookup must miss at the newer version"
        );
        let stale = a.stale_expansion(&uri, MPosition::new(3, 12)).await;
        assert_eq!(
            stale.expect("stale fallback should serve the version-1 entry").macro_name,
            "B"
        );
    }

    /// The stale fallback still respects the usage range — a cursor outside any
    /// cached range gets nothing.
    #[tokio::test]
    async fn stale_expansion_misses_outside_range() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        a.store_expansion(&uri, 1, &result_spanning((3, 10), (3, 16))).await;

        assert!(a.stale_expansion(&uri, MPosition::new(9, 0)).await.is_none());
    }

    /// Re-expanding the same usage range replaces the prior entry (no
    /// duplicate accumulation); the newest result wins.
    #[tokio::test]
    async fn store_expansion_dedups_same_range_keeping_newest() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        a.store_expansion(&uri, 1, &result_spanning_text((0, 4), (0, 10), "OLD")).await;
        a.store_expansion(&uri, 2, &result_spanning_text((0, 4), (0, 10), "NEW")).await;

        let r = a.stale_expansion(&uri, MPosition::new(0, 6)).await.unwrap();
        assert_eq!(r.expanded_text, "NEW", "newest expansion for a range must win");
    }

    /// The per-document cap evicts oldest-first so the cache can't grow without
    /// bound under many distinct usages.
    #[tokio::test]
    async fn store_expansion_caps_entries_evicting_oldest() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        // Oldest entry, on line 0.
        a.store_expansion(&uri, 1, &result_spanning((0, 0), (0, 4))).await;
        // Fill exactly to the cap with distinct ranges on later lines, pushing
        // the line-0 entry out.
        for line in 1..=(MAX_EXPANSIONS_PER_DOC as u32) {
            a.store_expansion(&uri, 1, &result_spanning((line, 0), (line, 4))).await;
        }
        assert!(
            a.stale_expansion(&uri, MPosition::new(0, 2)).await.is_none(),
            "the oldest entry should have been evicted past the cap"
        );
        assert!(
            a.stale_expansion(&uri, MPosition::new(MAX_EXPANSIONS_PER_DOC as u32, 2)).await.is_some(),
            "the newest entry should still be present"
        );
    }

    /// A result with no `usage_range` can't be keyed, so it's not cached.
    #[tokio::test]
    async fn store_expansion_without_usage_range_is_noop() {
        let a = adapter();
        let uri = Url::parse("file:///x.sv").unwrap();
        let mut r = result_spanning((0, 0), (0, 1));
        r.usage_range = None;
        a.store_expansion(&uri, 1, &r).await;

        assert!(a.cached_expansion(&uri, 1, MPosition::new(0, 0)).await.is_none());
        assert!(a.stale_expansion(&uri, MPosition::new(0, 0)).await.is_none());
    }
}
