//! Slang sidecar service layer: project config, connection management,
//! closed-file disk cache, and all IPC helpers.
//!
//! This module owns the three pieces of state that were previously scattered
//! across [`crate::backend::Backend`]:
//!
//! * The optional [`mimir_slang::Client`] connection to the sidecar binary.
//! * The resolved [`crate::project::ResolvedProject`] config (filelist,
//!   include dirs, defines, debounce, feature toggles, formatter config).
//! * The [`ClosedFileDiskCache`] that memoises on-disk reads for files not
//!   currently open in the editor.
//!
//! [`SlangService`] exposes a coarse-grained async API so `Backend` only
//! needs to call named methods rather than holding locks directly.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mimir_core::{Position as MPosition, Range as MRange};
use mimir_slang::{
    Client as SlangClient, CompleteParams as SlangCompleteParams,
    CompletionRequestKind as SlangCompletionRequestKind,
    DefinitionLocation as SlangDefinitionLocation, DefinitionParams as SlangDefinitionParams,
    ElaborateParams, ElaborateResult,
    ImplementationLocation as SlangImplementationLocation,
    ImplementationParams as SlangImplementationParams,
    SignatureHelpParams as SlangSignatureHelpParams, SlangCompletionItem, SourceFile,
    TypeDefinitionLocation as SlangTypeDefinitionLocation,
    TypeDefinitionParams as SlangTypeDefinitionParams,
};
use ropey::Rope;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, CompletionResponse, Location, Range, Position};
use tower_lsp::lsp_types::Url;
use tracing::{debug, error, warn};

use crate::backend::DocumentState;
use crate::project::{FeatureToggles, FormatterConfig, ResolvedProject};

// --------------------------------------------------------------------------
// ClosedFileDiskCache
// --------------------------------------------------------------------------

/// Cached on-disk source texts for closed project files.
///
/// `build_definition_params` and `build_elaborate_params` send every project
/// file's text to the slang sidecar on each hover/definition/completion call.
/// For files that are not open in the editor the only way to get their text
/// is a `std::fs::read_to_string` call — expensive when repeated N times per
/// request. This cache stores those texts so the disk read happens at most
/// once per file per session.
///
/// ## Correctness invariant
///
/// The cache **only covers files that are not currently open**. Open files
/// are always sourced from the live rope (`open_text` in
/// `build_definition_params`), which takes precedence over the disk cache
/// inside `assemble_elaborate_params`. Unsaved edits are therefore always
/// reflected correctly.
///
/// ## Invalidation events
///
/// | Event | Action |
/// |---|---|
/// | `did_open` | Drop entire cache — file moves closed→open; a future `did_close` must re-read disk |
/// | `did_close` | Drop entire cache — file moves open→closed; disk may have changed via another editor |
/// | `did_change` | **No invalidation** — changed file is in `open_text`, not the cache |
/// | `did_change_watched_files` CHANGED/CREATED | Evict the specific `PathBuf` entry |
/// | Project reload | Drop entire cache — filelist may have changed |
#[derive(Debug, Clone, Default)]
pub(crate) struct ClosedFileDiskCache {
    /// `path → source_text` for every closed project file successfully read
    /// from disk. Files that returned `None` from `read_to_string` are absent;
    /// the caller treats absence the same as an empty string (matching
    /// `assemble_elaborate_params`'s `unwrap_or_default` semantics).
    pub(crate) texts: HashMap<PathBuf, String>,
}

// --------------------------------------------------------------------------
// SlangService
// --------------------------------------------------------------------------

/// Owned by [`crate::backend::Backend`]; single point of contact for all
/// slang-sidecar IPC.
///
/// Wraps three pieces of shared state in their own `RwLock`s (rather than
/// one big lock) so reads of one don't block writes to another.
pub(crate) struct SlangService {
    /// Live document store, borrowed (not owned) so `assemble_elaborate_params`
    /// can snapshot open-buffer texts without a separate copy.
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,

    /// Optional sidecar connection. `None` when slang isn't configured.
    slang: Arc<RwLock<Option<Arc<SlangClient>>>>,

    /// Resolved project config from `.mimir.toml`. `None` when no config
    /// file was discovered in the workspace root.
    project: Arc<RwLock<Option<ResolvedProject>>>,

    /// Closed-file disk cache. `None` means "not yet populated or deliberately
    /// invalidated". See [`ClosedFileDiskCache`] for the full invalidation
    /// contract.
    closed_file_cache: Arc<RwLock<Option<ClosedFileDiskCache>>>,
}

impl SlangService {
    /// Construct a new `SlangService`.
    ///
    /// `documents` is a shared reference to Backend's document store so this
    /// service can snapshot open-buffer texts on each request. `slang` is the
    /// optional pre-spawned sidecar client (from `MIMIR_SLANG_PATH`).
    pub(crate) fn new(
        documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
        slang: Option<Arc<SlangClient>>,
    ) -> Self {
        Self {
            documents,
            slang: Arc::new(RwLock::new(slang)),
            project: Arc::new(RwLock::new(None)),
            closed_file_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Replace the sidecar client. Typically called from `initialize` when
    /// the project config's `[env]` table provides `MIMIR_SLANG_PATH`.
    pub(crate) async fn set_client(&self, client: Option<Arc<SlangClient>>) {
        *self.slang.write().await = client;
    }

    /// Replace the resolved project config. Drops the closed-file cache so
    /// the next request re-reads all closed project files (the filelist may
    /// have changed).
    pub(crate) async fn set_project(&self, project: Option<ResolvedProject>) {
        *self.project.write().await = project;
        *self.closed_file_cache.write().await = None;
    }

    /// Returns `true` when a sidecar client is currently configured.
    pub(crate) async fn is_configured(&self) -> bool {
        self.slang.read().await.is_some()
    }

    /// Return the current [`FeatureToggles`] from the resolved project config.
    ///
    /// Falls back to [`FeatureToggles::default`] when no project config is
    /// loaded.
    pub(crate) async fn current_features(&self) -> FeatureToggles {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.features.clone())
            .unwrap_or_default()
    }

    /// Return the current [`FormatterConfig`] from the resolved project config.
    ///
    /// Falls back to [`FormatterConfig::default`] when no project config is
    /// loaded.
    pub(crate) async fn current_formatter_config(&self) -> FormatterConfig {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.formatter.clone())
            .unwrap_or_default()
    }

    /// Return the current debounce duration, or `None` when either slang
    /// isn't configured or no project is loaded (both are required before
    /// elaboration is meaningful).
    pub(crate) async fn current_debounce(&self) -> Option<Duration> {
        if self.slang.read().await.is_none() {
            return None;
        }
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| Duration::from_millis(p.debounce_ms))
    }

    /// Drop the entire closed-file disk cache. Called on `did_open` and
    /// `did_close` when a file transitions between the closed and open states.
    pub(crate) async fn clear_closed_file_cache(&self) {
        *self.closed_file_cache.write().await = None;
    }

    /// Evict a single path from the closed-file cache. Called on
    /// `did_change_watched_files` CHANGED/CREATED events so the next request
    /// re-reads the updated file from disk while keeping all other cached
    /// entries.
    pub(crate) async fn evict_closed_file(&self, path: &Path) {
        let mut guard = self.closed_file_cache.write().await;
        if let Some(cache) = guard.as_mut() {
            if cache.texts.remove(path).is_some() {
                debug!(
                    path = %path.display(),
                    "closed_file_cache: evicted stale entry"
                );
            }
        }
    }

    /// Build the elaborate request envelope from the current project config
    /// and open-buffer snapshots.
    ///
    /// Returns `None` when either slang or the project are not configured —
    /// both are required to build a meaningful elaborate request. Returns
    /// `Some((params, files_in_request))` where `files_in_request` is the
    /// ordered list of URLs sent in the request (used by `publish_slang_result`
    /// to determine which files need a diagnostic publish).
    pub(crate) async fn build_elaborate_params(&self)
    -> Option<(ElaborateParams, Vec<Url>)>
    {
        if self.slang.read().await.is_none() {
            return None;
        }
        let project = self.project.read().await.clone()?;

        let open_text: HashMap<PathBuf, (Url, String)> = {
            let docs = self.documents.read().await;
            docs.iter()
                .filter_map(|(uri, state)| {
                    uri.to_file_path()
                        .ok()
                        .map(|p| (p, (uri.clone(), state.document.text())))
                })
                .collect()
        };

        Some(assemble_with_cache(&project, &open_text, &self.closed_file_cache).await)
    }

    /// Hash the inputs of an [`ElaborateParams`] for cache-keying.
    ///
    /// Deterministic within one process (sufficient for an in-memory equality
    /// check). Any change to a file's text, the filelist, include paths, or
    /// defines produces a different hash.
    pub(crate) fn hash_inputs(params: &ElaborateParams) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for f in &params.files {
            f.path.hash(&mut h);
            f.text.hash(&mut h);
            f.is_compilation_unit.hash(&mut h);
        }
        for d in &params.include_dirs {
            d.hash(&mut h);
        }
        for d in &params.defines {
            d.name.hash(&mut h);
            d.value.hash(&mut h);
        }
        params.top.hash(&mut h);
        h.finish()
    }

    /// Forward an elaborate request to the sidecar.
    pub(crate) async fn elaborate(&self, params: &ElaborateParams)
    -> Result<ElaborateResult, mimir_slang::ClientError>
    {
        let slang = self.slang.read().await.clone()
            .expect("elaborate called without a configured sidecar");
        slang.elaborate(params).await
    }

    /// Try to resolve a definition via the slang sidecar.
    ///
    /// Returns:
    /// * `None` — slang not configured, no project loaded, or cursor URI has
    ///   no filesystem path. The caller should fall through to the syntax path.
    /// * `Some(Resolved(locs))` — slang ran and returned an answer.
    /// * `Some(TransportError)` — IO/protocol error; caller should fall back.
    pub(crate) async fn definition(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<SlangDefinitionOutcome> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (params, _files_in_request) =
            build_definition_params(&project, &self.documents, &self.closed_file_cache, uri, target).await?;

        match slang.definition(&params).await {
            Ok(result) => {
                let locations: Vec<Location> = result
                    .locations
                    .into_iter()
                    .filter_map(slang_location_to_lsp)
                    .collect();
                debug!(count = locations.len(), "slang definition resolved");
                Some(SlangDefinitionOutcome::Resolved(locations))
            }
            Err(e) => {
                error!(error = %e, "slang definition transport error; falling back to syntax");
                Some(SlangDefinitionOutcome::TransportError)
            }
        }
    }

    /// Try to resolve the type of the symbol under the cursor via slang.
    pub(crate) async fn type_definition(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<SlangTypeDefinitionOutcome> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (def_params, _) =
            build_definition_params(&project, &self.documents, &self.closed_file_cache, uri, target).await?;
        let params = SlangTypeDefinitionParams {
            files: def_params.files,
            include_dirs: def_params.include_dirs,
            defines: def_params.defines,
            top: def_params.top,
            target_path: def_params.target_path,
            target_position: def_params.target_position,
        };

        match slang.type_definition(&params).await {
            Ok(result) => {
                let locations: Vec<Location> = result
                    .locations
                    .into_iter()
                    .filter_map(slang_type_definition_location_to_lsp)
                    .collect();
                Some(SlangTypeDefinitionOutcome::Resolved(locations))
            }
            Err(e) => {
                error!(error = %e, "slang type_definition transport error");
                Some(SlangTypeDefinitionOutcome::TransportError)
            }
        }
    }

    /// Try to resolve implementations via slang.
    pub(crate) async fn implementation(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<SlangImplementationOutcome> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (def_params, _) =
            build_definition_params(&project, &self.documents, &self.closed_file_cache, uri, target).await?;
        let params = SlangImplementationParams {
            files: def_params.files,
            include_dirs: def_params.include_dirs,
            defines: def_params.defines,
            top: def_params.top,
            target_path: def_params.target_path,
            target_position: def_params.target_position,
        };

        match slang.implementation(&params).await {
            Ok(result) => {
                let locations: Vec<Location> = result
                    .locations
                    .into_iter()
                    .filter_map(slang_implementation_location_to_lsp)
                    .collect();
                Some(SlangImplementationOutcome::Resolved(locations))
            }
            Err(e) => {
                error!(error = %e, "slang implementation transport error");
                Some(SlangImplementationOutcome::TransportError)
            }
        }
    }

    /// Try slang-backed member-access or package-scope completion.
    pub(crate) async fn member_completion(
        &self,
        uri: &Url,
        pos: MPosition,
    ) -> Option<CompletionResponse> {
        let text = {
            let docs = self.documents.read().await;
            docs.get(uri)?.document.text()
        };
        let rope = Rope::from_str(&text);
        let (is_pkg_scope, member_prefix) = detect_member_access(&rope, pos)?;
        let kind = if is_pkg_scope {
            SlangCompletionRequestKind::PackageScope
        } else {
            SlangCompletionRequestKind::MemberAccess
        };
        let wire_prefix = (!member_prefix.is_empty()).then(|| member_prefix.clone());

        let patch = (!is_pkg_scope && member_prefix.is_empty())
            .then(|| build_member_completion_sentinel(&rope, pos))
            .flatten();
        let send_pos = patch.as_ref().map_or(pos, |p| p.adjusted_position);

        let items = self
            .complete_request_with_patch(uri, send_pos, kind, wire_prefix, patch)
            .await?;
        if items.is_empty() {
            debug!(
                is_pkg_scope,
                prefix = %member_prefix,
                "slang member completion: 0 items; popup will be empty (no syntax fallback for member-access)",
            );
            return None;
        }
        let prefix_lower = member_prefix.to_ascii_lowercase();
        let filtered: Vec<SlangCompletionItem> = items
            .into_iter()
            .filter(|it| {
                prefix_lower.is_empty() || it.label.to_ascii_lowercase().starts_with(&prefix_lower)
            })
            .collect();
        debug!(count = filtered.len(), "slang member completion");
        Some(slang_items_to_response(filtered))
    }

    /// Try slang-backed scope-aware identifier completion.
    pub(crate) async fn identifier_completion(
        &self,
        uri: &Url,
        pos: MPosition,
    ) -> Option<CompletionResponse> {
        let text = {
            let docs = self.documents.read().await;
            docs.get(uri)?.document.text()
        };
        let rope = Rope::from_str(&text);
        let prefix = mimir_syntax::symbols::prefix_at(&rope, pos).unwrap_or_default();
        let wire_prefix = (!prefix.is_empty()).then(|| prefix.to_owned());

        let items = self
            .complete_request(uri, pos, SlangCompletionRequestKind::Identifier, wire_prefix)
            .await?;
        if items.is_empty() {
            debug!("slang identifier completion: no items; falling back to syntax");
            return None;
        }
        debug!(count = items.len(), "slang identifier completion");
        Some(slang_items_to_response(items))
    }

    /// Try slang-backed macro-name completion.
    pub(crate) async fn macro_completion(
        &self,
        uri: &Url,
        pos: MPosition,
        macro_prefix: &str,
    ) -> Option<CompletionResponse> {
        let wire_prefix = (!macro_prefix.is_empty()).then(|| macro_prefix.to_owned());
        let items = self
            .complete_request(uri, pos, SlangCompletionRequestKind::Macro, wire_prefix)
            .await?;
        debug!(count = items.len(), "slang macro completion");
        Some(slang_items_to_response(items))
    }

    /// Try slang-backed signature help.
    pub(crate) async fn signature_help(
        &self,
        uri: &Url,
        pos: MPosition,
    ) -> Option<Vec<mimir_slang::SignatureItem>> {
        let client = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (def_params, _) =
            build_definition_params(&project, &self.documents, &self.closed_file_cache, uri, pos).await?;
        let params = SlangSignatureHelpParams {
            files: def_params.files,
            include_dirs: def_params.include_dirs,
            defines: def_params.defines,
            top: def_params.top,
            target_path: def_params.target_path,
            target_position: def_params.target_position,
        };

        match client.signature_help(&params).await {
            Ok(result) if !result.signatures.is_empty() => {
                debug!(
                    count = result.signatures.len(),
                    "slang signature help returned"
                );
                Some(result.signatures)
            }
            Ok(_) => {
                debug!("slang signature help: empty — falling back to tree-sitter");
                None
            }
            Err(e) => {
                debug!(error = %e, "slang signature help transport error — falling back");
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    /// Common scaffolding for the three slang-backed completion routes.
    async fn complete_request(
        &self,
        uri: &Url,
        pos: MPosition,
        kind: SlangCompletionRequestKind,
        prefix: Option<String>,
    ) -> Option<Vec<SlangCompletionItem>> {
        self.complete_request_with_patch(uri, pos, kind, prefix, None)
            .await
    }

    /// Variant that lets the caller patch the target file's text before the
    /// request is sent. Used by member-access completion to insert a
    /// placeholder identifier after the trigger `.`.
    async fn complete_request_with_patch(
        &self,
        uri: &Url,
        pos: MPosition,
        kind: SlangCompletionRequestKind,
        prefix: Option<String>,
        patch: Option<TargetTextPatch>,
    ) -> Option<Vec<SlangCompletionItem>> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;
        let (mut def_params, _) =
            build_definition_params(&project, &self.documents, &self.closed_file_cache, uri, pos).await?;
        if let Some(patch) = patch {
            apply_target_text_patch(&mut def_params, &patch);
        }
        let params = into_complete_params(def_params, kind, prefix);
        match slang.complete(&params).await {
            Ok(result) => Some(result.items),
            Err(e) => {
                warn!(error = %e, "slang complete error; falling back to syntax");
                None
            }
        }
    }
}

// --------------------------------------------------------------------------
// Slang outcome types and routing decisions
// --------------------------------------------------------------------------

/// Outcome of a slang definition request, used by `goto_definition` to
/// decide whether to short-circuit (Resolved, including empty) or fall
/// through to the syntax path (TransportError).
#[derive(Debug)]
pub(crate) enum SlangDefinitionOutcome {
    /// Slang answered. The vector may be empty — that's "no decl found"
    /// and the server returns `None` to the editor without falling
    /// back to syntax.
    Resolved(Vec<Location>),
    /// IO / protocol error talking to the sidecar. The caller falls
    /// back to syntax.
    TransportError,
}

/// Routing decision for `goto_definition`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DefinitionRoute {
    /// Use slang's locations (may be empty — trust-slang short-circuits
    /// the empty case to `None` in the response, never the syntax index).
    UseSlangResult(Vec<Location>),
    /// Slang isn't configured, no project loaded, or the sidecar request
    /// hit a transport error. The handler should consult the syntax
    /// index instead.
    UseSyntaxFallback,
}

/// Pure routing policy for `goto_definition`.
///
/// Policy: slang is primary; an *empty* slang answer falls back to the
/// tree-sitter workspace index. Unlike `goto_type_definition` /
/// `goto_implementation`, definition has a meaningful syntax fallback.
pub(crate) fn route_definition(outcome: Option<SlangDefinitionOutcome>) -> DefinitionRoute {
    match outcome {
        Some(SlangDefinitionOutcome::Resolved(locs)) if !locs.is_empty() => {
            DefinitionRoute::UseSlangResult(locs)
        }
        _ => DefinitionRoute::UseSyntaxFallback,
    }
}

/// Outcome of a slang `typeDefinition` request.
#[derive(Debug)]
pub(crate) enum SlangTypeDefinitionOutcome {
    /// Slang answered (may be empty — no fallback in either case).
    Resolved(Vec<Location>),
    /// I/O / protocol error. The handler returns `None` to the editor.
    TransportError,
}

/// Routing decision for `goto_type_definition`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TypeDefinitionRoute {
    /// Use slang's locations (trust-slang-on-empty: empty → `None`).
    UseSlangResult(Vec<Location>),
    /// Slang not configured, untitled buffer, or transport error.
    UseEmpty,
}

/// Pure routing policy for `goto_type_definition`.
pub(crate) fn route_type_definition(
    outcome: Option<SlangTypeDefinitionOutcome>,
) -> TypeDefinitionRoute {
    match outcome {
        Some(SlangTypeDefinitionOutcome::Resolved(locs)) => {
            TypeDefinitionRoute::UseSlangResult(locs)
        }
        Some(SlangTypeDefinitionOutcome::TransportError) | None => TypeDefinitionRoute::UseEmpty,
    }
}

/// Outcome of a slang `implementation` request.
#[derive(Debug)]
pub(crate) enum SlangImplementationOutcome {
    /// Slang answered (may be empty — no fallback in either case).
    Resolved(Vec<Location>),
    /// I/O / protocol error. The handler returns `None` to the editor.
    TransportError,
}

/// Routing decision for `goto_implementation`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ImplementationRoute {
    /// Use slang's locations (trust-slang-on-empty: empty → `None`).
    UseSlangResult(Vec<Location>),
    /// Slang not configured, untitled buffer, or transport error.
    UseEmpty,
}

/// Pure routing policy for `goto_implementation`.
pub(crate) fn route_implementation(
    outcome: Option<SlangImplementationOutcome>,
) -> ImplementationRoute {
    match outcome {
        Some(SlangImplementationOutcome::Resolved(locs)) => {
            ImplementationRoute::UseSlangResult(locs)
        }
        Some(SlangImplementationOutcome::TransportError) | None => ImplementationRoute::UseEmpty,
    }
}

// --------------------------------------------------------------------------
// Completion sentinel patch
// --------------------------------------------------------------------------

/// Patch applied to a slang request so the sidecar sees a parseable
/// version of the buffer. The completion-sentinel inserts a dummy
/// identifier at the cursor; the LSP buffer is unchanged.
#[derive(Debug, Clone)]
pub(crate) struct TargetTextPatch {
    /// Byte offset (UTF-8) at which `insert` is spliced into the file
    /// identified by `SlangDefinitionParams::target_path`.
    pub(crate) insert_byte_offset: usize,
    /// Text inserted at `insert_byte_offset`.
    pub(crate) insert: &'static str,
    /// Cursor position to send to slang AFTER the splice — i.e. the
    /// post-insert location of the original cursor (shifted right by
    /// `insert.encode_utf16().count()`). LSP positions are UTF-16 units.
    pub(crate) adjusted_position: MPosition,
}

/// Reserved identifier inserted by [`build_member_completion_sentinel`].
pub(crate) const COMPLETION_SENTINEL: &str = "__mimir_complete__";

/// Build a [`TargetTextPatch`] that inserts [`COMPLETION_SENTINEL`] at the
/// cursor so a `.`-triggered completion produces a parseable
/// `MemberAccessExpression`.
pub(crate) fn build_member_completion_sentinel(
    rope: &Rope,
    pos: MPosition,
) -> Option<TargetTextPatch> {
    let byte = pos.to_byte_offset(rope).ok()?;
    let utf16_shift = COMPLETION_SENTINEL.encode_utf16().count() as u32;
    Some(TargetTextPatch {
        insert_byte_offset: byte,
        insert: COMPLETION_SENTINEL,
        adjusted_position: MPosition::new(pos.line, pos.character + utf16_shift),
    })
}

/// Splice `patch.insert` into the request's target file at the recorded
/// byte offset.
pub(crate) fn apply_target_text_patch(
    def: &mut SlangDefinitionParams,
    patch: &TargetTextPatch,
) {
    let target = def.target_path.clone();
    for sf in def.files.iter_mut() {
        if sf.path == target && patch.insert_byte_offset <= sf.text.len() {
            sf.text.insert_str(patch.insert_byte_offset, patch.insert);
            break;
        }
    }
}

// --------------------------------------------------------------------------
// Free helpers (pub(crate) so backend.rs can call them if needed)
// --------------------------------------------------------------------------

/// Convert our internal `Range` into the `lsp_types` shape.
pub(crate) fn m_range_to_lsp(r: MRange) -> Range {
    Range {
        start: Position {
            line: r.start.line,
            character: r.start.character,
        },
        end: Position {
            line: r.end.line,
            character: r.end.character,
        },
    }
}

/// Scan the rope line up to `pos` for a member-access or package-scope trigger.
///
/// Returns `Some((is_package_scope, prefix_after_trigger))` when a `.` or
/// `::` trigger is found immediately before any partial identifier at the
/// cursor. `is_package_scope` is `true` for `::`, `false` for `.`.
/// Returns `None` when no trigger is found.
pub(crate) fn detect_member_access(rope: &Rope, pos: MPosition) -> Option<(bool, String)> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line = rope.line(pos.line as usize);

    let mut buf = String::new();
    let mut utf16: u32 = 0;
    for ch in line.chars() {
        if matches!(ch, '\n' | '\r') || utf16 >= pos.character {
            break;
        }
        buf.push(ch);
        utf16 += ch.len_utf16() as u32;
    }

    let chars: Vec<char> = buf.chars().collect();
    let mut i = chars.len();

    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$') {
        i -= 1;
    }
    let prefix: String = chars[i..].iter().collect();

    if i > 0 && chars[i - 1] == '.' {
        return Some((false, prefix));
    }
    if i >= 2 && chars[i - 2] == ':' && chars[i - 1] == ':' {
        return Some((true, prefix));
    }

    None
}

/// Detect a `` ` `` macro trigger before the cursor.
///
/// Returns `Some(prefix)` when the identifier (or empty string) immediately
/// before the cursor is preceded by a backtick. Returns `None` otherwise.
pub(crate) fn detect_macro_trigger(rope: &Rope, pos: MPosition) -> Option<String> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line = rope.line(pos.line as usize);

    let mut buf = String::new();
    let mut utf16: u32 = 0;
    for ch in line.chars() {
        if matches!(ch, '\n' | '\r') || utf16 >= pos.character {
            break;
        }
        buf.push(ch);
        utf16 += ch.len_utf16() as u32;
    }

    let chars: Vec<char> = buf.chars().collect();
    let mut i = chars.len();

    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_') {
        i -= 1;
    }
    let prefix: String = chars[i..].iter().collect();

    if i > 0 && chars[i - 1] == '`' {
        return Some(prefix);
    }

    None
}

/// Convert a path string from the slang sidecar back to a `file:` URL.
pub(crate) fn path_to_url(path: &str) -> Option<Url> {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        Url::from_file_path(&p).ok()
    } else {
        std::fs::canonicalize(&p)
            .ok()
            .and_then(|abs| Url::from_file_path(abs).ok())
    }
}

/// Convert a vector of sidecar [`SlangCompletionItem`]s into an LSP
/// [`CompletionResponse::Array`].
pub(crate) fn slang_items_to_response(items: Vec<SlangCompletionItem>) -> CompletionResponse {
    let mut seen: HashSet<String> = HashSet::new();
    let mapped: Vec<CompletionItem> = items
        .into_iter()
        .filter(|it| seen.insert(it.label.clone()))
        .map(|it| CompletionItem {
            label: it.label,
            kind: Some(slang_completion_kind_to_lsp(it.kind)),
            detail: it.detail,
            ..Default::default()
        })
        .collect();
    debug!(
        count = mapped.len(),
        labels = ?mapped.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
        "slang completion response",
    );
    CompletionResponse::Array(mapped)
}

// --------------------------------------------------------------------------
// Private helpers (not pub — used internally)
// --------------------------------------------------------------------------

/// Map a sidecar numeric `kind` code to an LSP [`CompletionItemKind`].
fn slang_completion_kind_to_lsp(kind: u8) -> CompletionItemKind {
    match kind {
        2 => CompletionItemKind::METHOD,
        5 => CompletionItemKind::FIELD,
        6 => CompletionItemKind::VARIABLE,
        20 => CompletionItemKind::ENUM_MEMBER,
        21 => CompletionItemKind::CONSTANT,
        _ => CompletionItemKind::VARIABLE,
    }
}

/// Build a `definition` request envelope for the slang sidecar, including
/// all open-buffer texts and disk-cached closed-file texts.
async fn build_definition_params(
    project: &ResolvedProject,
    documents: &Arc<RwLock<HashMap<Url, DocumentState>>>,
    closed_file_cache: &Arc<RwLock<Option<ClosedFileDiskCache>>>,
    target_uri: &Url,
    target_position: MPosition,
) -> Option<(SlangDefinitionParams, Vec<Url>)> {
    let target_path = target_uri.to_file_path().ok()?;

    let open_text: HashMap<PathBuf, (Url, String)> = {
        let docs = documents.read().await;
        docs.iter()
            .filter_map(|(uri, state)| {
                uri.to_file_path()
                    .ok()
                    .map(|p| (p, (uri.clone(), state.document.text())))
            })
            .collect()
    };
    let (elab, files_in_request) =
        assemble_with_cache(project, &open_text, closed_file_cache).await;
    let params = SlangDefinitionParams {
        files: elab.files,
        include_dirs: elab.include_dirs,
        defines: elab.defines,
        top: elab.top,
        target_path: target_path.display().to_string(),
        target_position: MPosition::new(target_position.line, target_position.character),
    };
    Some((params, files_in_request))
}

/// Assemble [`ElaborateParams`] using the [`ClosedFileDiskCache`].
///
/// Three-phase approach so no lock is held across disk I/O:
/// 1. **Read phase** — snapshot cached texts under `read().await`.
/// 2. **Work phase** — assemble params, reading from snapshot first, falling
///    back to `std::fs::read_to_string` for misses, collecting new texts.
/// 3. **Write phase** — merge newly-read texts into cache under `write().await`.
pub(crate) async fn assemble_with_cache(
    project: &ResolvedProject,
    open_text: &HashMap<PathBuf, (Url, String)>,
    closed_file_cache: &Arc<RwLock<Option<ClosedFileDiskCache>>>,
) -> (ElaborateParams, Vec<Url>) {
    // Phase 1: snapshot existing cached texts.
    let cached_texts: HashMap<PathBuf, String> = closed_file_cache
        .read()
        .await
        .as_ref()
        .map(|c| c.texts.clone())
        .unwrap_or_default();

    // Phase 2: assemble params — no lock held; disk reads happen here.
    let mut new_entries: HashMap<PathBuf, String> = HashMap::new();
    let result = assemble_elaborate_params(project, open_text, |path| {
        if let Some(text) = cached_texts.get(path) {
            return Some(text.clone());
        }
        let text = std::fs::read_to_string(path).ok()?;
        new_entries.insert(path.to_path_buf(), text.clone());
        Some(text)
    });

    // Phase 3: merge newly-read texts into the cache.
    {
        let mut guard = closed_file_cache.write().await;
        let cache = guard.get_or_insert_with(ClosedFileDiskCache::default);
        cache.texts.extend(new_entries);
        debug!(
            count = cache.texts.len(),
            "closed_file_cache: warm after merge"
        );
    }

    result
}

/// Pure version of `build_elaborate_params`: assemble the request envelope
/// from the project, a snapshot of open documents, and an injectable disk
/// reader. Split out so unit tests can drive it without a real filesystem.
pub(crate) fn assemble_elaborate_params(
    project: &ResolvedProject,
    open_text: &HashMap<PathBuf, (Url, String)>,
    mut read_disk: impl FnMut(&Path) -> Option<String>,
) -> (ElaborateParams, Vec<Url>) {
    let mut files: Vec<SourceFile> = Vec::new();
    let mut files_in_request: Vec<Url> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for project_file in &project.files {
        if !seen.insert(project_file.clone()) {
            continue;
        }
        let (url, text) = match open_text.get(project_file) {
            Some((url, text)) => (url.clone(), text.clone()),
            None => {
                let text = read_disk(project_file).unwrap_or_default();
                let url = Url::from_file_path(project_file)
                    .unwrap_or_else(|()| placeholder_url(project_file));
                (url, text)
            }
        };
        files.push(SourceFile {
            path: project_file.display().to_string(),
            text,
            is_compilation_unit: true,
        });
        files_in_request.push(url);
    }

    for (path, (url, text)) in open_text {
        if seen.insert(path.clone()) {
            files.push(SourceFile {
                path: path.display().to_string(),
                text: text.clone(),
                is_compilation_unit: false,
            });
            files_in_request.push(url.clone());
        }
    }

    let include_dirs = project
        .include_dirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    let params = ElaborateParams {
        files,
        include_dirs,
        defines: project.defines.clone(),
        top: project.top.clone(),
    };
    (params, files_in_request)
}

/// Last-ditch fallback when `Url::from_file_path` rejects a path.
fn placeholder_url(p: &Path) -> Url {
    Url::parse(&format!("file://{}", p.display()))
        .unwrap_or_else(|_| Url::parse("file:///").expect("file:/// is always valid"))
}

/// Convert a slang `DefinitionLocation` into LSP's `Location`.
fn slang_location_to_lsp(loc: SlangDefinitionLocation) -> Option<Location> {
    let url = path_to_url(&loc.path)?;
    Some(Location {
        uri: url,
        range: m_range_to_lsp(loc.range),
    })
}

/// Convert a slang `TypeDefinitionLocation` into LSP's `Location`.
fn slang_type_definition_location_to_lsp(loc: SlangTypeDefinitionLocation) -> Option<Location> {
    let url = path_to_url(&loc.path)?;
    Some(Location {
        uri: url,
        range: m_range_to_lsp(loc.range),
    })
}

/// Convert a slang `ImplementationLocation` into LSP's `Location`.
fn slang_implementation_location_to_lsp(loc: SlangImplementationLocation) -> Option<Location> {
    let url = path_to_url(&loc.path)?;
    Some(Location {
        uri: url,
        range: m_range_to_lsp(loc.range),
    })
}

/// Build a [`SlangCompleteParams`] from a resolved [`SlangDefinitionParams`].
fn into_complete_params(
    def: SlangDefinitionParams,
    kind: SlangCompletionRequestKind,
    prefix: Option<String>,
) -> SlangCompleteParams {
    SlangCompleteParams {
        files: def.files,
        include_dirs: def.include_dirs,
        defines: def.defines,
        top: def.top,
        target_path: def.target_path,
        target_position: def.target_position,
        kind,
        prefix,
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::{Position as MPosition, Range as MRange};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tower_lsp::lsp_types::Url;

    use crate::project::ResolvedProject;

    fn project_with_files(files: Vec<PathBuf>) -> ResolvedProject {
        ResolvedProject {
            root: PathBuf::from("/proj"),
            files,
            include_dirs: vec![PathBuf::from("/proj/inc")],
            defines: vec![],
            top: Some("tb_top".into()),
            debounce_ms: 350,
            env: HashMap::new(),
            features: crate::project::FeatureToggles::default(),
            formatter: crate::project::FormatterConfig::default(),
        }
    }

    // ------------------------------------------------------------------
    // m_range_to_lsp
    // ------------------------------------------------------------------

    /// `Range` round-trips through `m_range_to_lsp` losslessly.
    #[test]
    fn m_range_to_lsp_preserves_endpoints() {
        let r = MRange::new(MPosition::new(3, 7), MPosition::new(4, 12));
        let lsp = m_range_to_lsp(r);
        assert_eq!(lsp.start.line, 3);
        assert_eq!(lsp.start.character, 7);
        assert_eq!(lsp.end.line, 4);
        assert_eq!(lsp.end.character, 12);
    }

    // ------------------------------------------------------------------
    // route_definition
    // ------------------------------------------------------------------

    fn sample_location() -> tower_lsp::lsp_types::Location {
        tower_lsp::lsp_types::Location {
            uri: Url::parse("file:///proj/a.sv").unwrap(),
            range: tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position {
                    line: 1,
                    character: 0,
                },
                end: tower_lsp::lsp_types::Position {
                    line: 1,
                    character: 8,
                },
            },
        }
    }

    #[test]
    fn route_definition_uses_slang_when_resolved_non_empty() {
        let locs = vec![sample_location()];
        let route = route_definition(Some(SlangDefinitionOutcome::Resolved(locs.clone())));
        assert_eq!(route, DefinitionRoute::UseSlangResult(locs));
    }

    #[test]
    fn route_definition_falls_back_when_slang_resolved_empty() {
        let route = route_definition(Some(SlangDefinitionOutcome::Resolved(Vec::new())));
        assert_eq!(route, DefinitionRoute::UseSyntaxFallback);
    }

    #[test]
    fn route_definition_falls_back_on_transport_error() {
        let route = route_definition(Some(SlangDefinitionOutcome::TransportError));
        assert_eq!(route, DefinitionRoute::UseSyntaxFallback);
    }

    #[test]
    fn route_definition_falls_back_when_slang_not_run() {
        let route = route_definition(None);
        assert_eq!(route, DefinitionRoute::UseSyntaxFallback);
    }

    // ------------------------------------------------------------------
    // slang_location_to_lsp
    // ------------------------------------------------------------------

    #[test]
    fn slang_location_to_lsp_round_trip() {
        let loc = SlangDefinitionLocation {
            path: "/proj/b.sv".into(),
            range: MRange::new(MPosition::new(2, 6), MPosition::new(2, 12)),
        };
        let lsp = slang_location_to_lsp(loc).expect("path_to_url should accept absolute path");
        assert_eq!(lsp.uri.scheme(), "file");
        assert!(lsp.uri.path().ends_with("/proj/b.sv"));
        assert_eq!(lsp.range.start.line, 2);
        assert_eq!(lsp.range.start.character, 6);
        assert_eq!(lsp.range.end.character, 12);
    }

    #[test]
    fn slang_location_to_lsp_returns_none_on_unparseable_path() {
        let loc = SlangDefinitionLocation {
            path: String::new(),
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 0)),
        };
        let _ = slang_location_to_lsp(loc);
    }

    // ------------------------------------------------------------------
    // route_type_definition
    // ------------------------------------------------------------------

    #[test]
    fn route_type_definition_uses_slang_when_resolved_non_empty() {
        let locs = vec![sample_location()];
        let route = route_type_definition(Some(SlangTypeDefinitionOutcome::Resolved(locs.clone())));
        assert_eq!(route, TypeDefinitionRoute::UseSlangResult(locs));
    }

    #[test]
    fn route_type_definition_uses_slang_when_resolved_empty() {
        let route = route_type_definition(Some(SlangTypeDefinitionOutcome::Resolved(Vec::new())));
        assert_eq!(route, TypeDefinitionRoute::UseSlangResult(Vec::new()));
    }

    #[test]
    fn route_type_definition_returns_empty_on_transport_error() {
        let route = route_type_definition(Some(SlangTypeDefinitionOutcome::TransportError));
        assert_eq!(route, TypeDefinitionRoute::UseEmpty);
    }

    #[test]
    fn route_type_definition_returns_empty_when_slang_not_run() {
        let route = route_type_definition(None);
        assert_eq!(route, TypeDefinitionRoute::UseEmpty);
    }

    // ------------------------------------------------------------------
    // route_implementation
    // ------------------------------------------------------------------

    #[test]
    fn route_implementation_uses_slang_when_resolved_non_empty() {
        let locs = vec![sample_location()];
        let route = route_implementation(Some(SlangImplementationOutcome::Resolved(locs.clone())));
        assert_eq!(route, ImplementationRoute::UseSlangResult(locs));
    }

    #[test]
    fn route_implementation_uses_slang_when_resolved_empty() {
        let route = route_implementation(Some(SlangImplementationOutcome::Resolved(Vec::new())));
        assert_eq!(route, ImplementationRoute::UseSlangResult(Vec::new()));
    }

    #[test]
    fn route_implementation_returns_empty_on_transport_error() {
        let route = route_implementation(Some(SlangImplementationOutcome::TransportError));
        assert_eq!(route, ImplementationRoute::UseEmpty);
    }

    #[test]
    fn route_implementation_returns_empty_when_slang_not_run() {
        let route = route_implementation(None);
        assert_eq!(route, ImplementationRoute::UseEmpty);
    }

    // ------------------------------------------------------------------
    // assemble_elaborate_params
    // ------------------------------------------------------------------

    #[test]
    fn assemble_uses_open_text_then_disk() {
        let f1 = PathBuf::from("/proj/a.sv");
        let f2 = PathBuf::from("/proj/b.sv");
        let project = project_with_files(vec![f1.clone(), f2.clone()]);

        let url1 = Url::from_file_path(&f1).unwrap();
        let mut open_text = HashMap::new();
        open_text.insert(f1.clone(), (url1.clone(), "module a; endmodule".into()));

        let (params, files_in_request) = assemble_elaborate_params(&project, &open_text, |p| {
            if p == f2 {
                Some("module b; endmodule".into())
            } else {
                None
            }
        });

        assert_eq!(params.files.len(), 2);
        assert_eq!(params.files[0].path, "/proj/a.sv");
        assert_eq!(params.files[0].text, "module a; endmodule");
        assert!(params.files[0].is_compilation_unit);
        assert_eq!(params.files[1].path, "/proj/b.sv");
        assert_eq!(params.files[1].text, "module b; endmodule");
        assert!(params.files[1].is_compilation_unit);
        assert_eq!(params.include_dirs, vec!["/proj/inc"]);
        assert_eq!(params.top.as_deref(), Some("tb_top"));
        assert_eq!(files_in_request.len(), 2);
        assert_eq!(files_in_request[0], url1);
    }

    #[test]
    fn assemble_appends_open_docs_not_in_filelist() {
        let f1 = PathBuf::from("/proj/a.sv");
        let project = project_with_files(vec![f1.clone()]);

        let scratch = PathBuf::from("/tmp/scratch.sv");
        let scratch_url = Url::from_file_path(&scratch).unwrap();
        let mut open_text = HashMap::new();
        open_text.insert(
            scratch.clone(),
            (scratch_url.clone(), "module s; endmodule".into()),
        );

        let (params, files_in_request) =
            assemble_elaborate_params(&project, &open_text, |_| Some(String::new()));

        assert_eq!(params.files.len(), 2);
        assert_eq!(params.files[0].path, "/proj/a.sv");
        assert!(params.files[0].is_compilation_unit);
        assert_eq!(params.files[1].path, "/tmp/scratch.sv");
        assert!(!params.files[1].is_compilation_unit);
        assert_eq!(files_in_request.len(), 2);
        assert!(files_in_request.contains(&scratch_url));
    }

    #[test]
    fn assemble_deduplicates_repeated_files() {
        let f = PathBuf::from("/proj/a.sv");
        let project = project_with_files(vec![f.clone(), f.clone()]);

        let (params, files_in_request) =
            assemble_elaborate_params(&project, &HashMap::new(), |_| Some(String::new()));

        assert_eq!(params.files.len(), 1);
        assert_eq!(files_in_request.len(), 1);
    }

    #[test]
    fn assemble_does_not_expand_includes_into_request() {
        let umbrella = PathBuf::from("/proj/uvm.sv");
        let mut project = project_with_files(vec![umbrella.clone()]);
        project.include_dirs = vec![PathBuf::from("/uvm/src")];

        let pkg = PathBuf::from("/uvm/src/uvm_pkg.sv");
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (umbrella.clone(), "`include \"uvm_pkg.sv\"\n".into()),
            (pkg.clone(), "package uvm_pkg; endpackage\n".into()),
        ]);

        let (params, _) =
            assemble_elaborate_params(&project, &HashMap::new(), |p| texts.get(p).cloned());

        let paths: Vec<&str> = params.files.iter().map(|sf| sf.path.as_str()).collect();
        assert_eq!(paths, vec!["/proj/uvm.sv"]);
    }

    // ------------------------------------------------------------------
    // ClosedFileDiskCache via assemble_with_cache
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn closed_file_cache_populated_on_first_call_skipped_on_second() {
        let f = PathBuf::from("/proj/a.sv");
        let project = project_with_files(vec![f.clone()]);
        let open_text: HashMap<PathBuf, (Url, String)> = HashMap::new();
        let cache: Arc<RwLock<Option<ClosedFileDiskCache>>> = Arc::new(RwLock::new(None));

        assert!(cache.read().await.is_none(), "cache starts empty");
        let _ = assemble_with_cache(&project, &open_text, &cache).await;

        assert!(
            cache.read().await.is_some(),
            "cache should be Some after first call"
        );

        let text_before = cache.read().await.as_ref().unwrap().texts.get(&f).cloned();
        let _ = assemble_with_cache(&project, &open_text, &cache).await;
        let text_after = cache.read().await.as_ref().unwrap().texts.get(&f).cloned();
        assert_eq!(
            text_before, text_after,
            "cache entry must be identical on second call (no re-read)"
        );
    }

    #[tokio::test]
    async fn closed_file_cache_invalidation_resets_state() {
        let f = PathBuf::from("/proj/a.sv");
        let project = project_with_files(vec![f]);
        let open_text: HashMap<PathBuf, (Url, String)> = HashMap::new();
        let cache: Arc<RwLock<Option<ClosedFileDiskCache>>> = Arc::new(RwLock::new(None));

        let _ = assemble_with_cache(&project, &open_text, &cache).await;
        assert!(cache.read().await.is_some());

        *cache.write().await = None;
        assert!(cache.read().await.is_none(), "cache must be None after invalidation");

        let _ = assemble_with_cache(&project, &open_text, &cache).await;
        assert!(
            cache.read().await.is_some(),
            "cache must be re-populated after next call"
        );
    }

    #[tokio::test]
    async fn closed_file_cache_targeted_eviction_preserves_other_entries() {
        let fa = PathBuf::from("/proj/a.sv");
        let fb = PathBuf::from("/proj/b.sv");

        let mut initial = ClosedFileDiskCache::default();
        initial.texts.insert(fa.clone(), "module a; endmodule".into());
        initial.texts.insert(fb.clone(), "module b; endmodule".into());
        let cache: Arc<RwLock<Option<ClosedFileDiskCache>>> =
            Arc::new(RwLock::new(Some(initial)));

        {
            let mut guard = cache.write().await;
            if let Some(c) = guard.as_mut() {
                c.texts.remove(&fa);
            }
        }

        let guard = cache.read().await;
        let c = guard.as_ref().unwrap();
        assert!(!c.texts.contains_key(&fa), "evicted entry must be absent");
        assert!(c.texts.contains_key(&fb), "non-evicted entry must still be present");
    }

    // ------------------------------------------------------------------
    // member_completion_sentinel
    // ------------------------------------------------------------------

    #[test]
    fn member_completion_sentinel_inserts_at_cursor() {
        let rope = Rope::from_str("  a.");
        let pos = MPosition::new(0, 4);
        let patch = build_member_completion_sentinel(&rope, pos).expect("cursor in bounds");
        assert_eq!(patch.insert, COMPLETION_SENTINEL);
        assert_eq!(patch.insert_byte_offset, 4);
        assert_eq!(patch.adjusted_position.line, 0);
        assert_eq!(
            patch.adjusted_position.character,
            4 + COMPLETION_SENTINEL.encode_utf16().count() as u32,
        );
    }

    #[test]
    fn apply_target_text_patch_only_touches_target() {
        let mut def = SlangDefinitionParams {
            files: vec![
                SourceFile {
                    path: "/proj/a.sv".into(),
                    text: "module a; endmodule".into(),
                    is_compilation_unit: true,
                },
                SourceFile {
                    path: "/proj/b.sv".into(),
                    text: "module b; endmodule".into(),
                    is_compilation_unit: true,
                },
            ],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            target_path: "/proj/b.sv".into(),
            target_position: MPosition::new(0, 9),
        };
        let patch = TargetTextPatch {
            insert_byte_offset: 9,
            insert: COMPLETION_SENTINEL,
            adjusted_position: MPosition::new(0, 9),
        };
        apply_target_text_patch(&mut def, &patch);
        assert_eq!(def.files[0].text, "module a; endmodule");
        assert!(def.files[1].text.contains(COMPLETION_SENTINEL));
        assert!(def.files[1].text.starts_with("module b;"));
    }

    // ------------------------------------------------------------------
    // detect_member_access
    // ------------------------------------------------------------------

    fn rope_from(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn detect_member_access_dot_empty_prefix() {
        let rope = rope_from("my_obj.");
        let pos = MPosition::new(0, 7);
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((false, String::new())));
    }

    #[test]
    fn detect_member_access_dot_with_prefix() {
        let rope = rope_from("my_obj.run_p");
        let pos = MPosition::new(0, 12);
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((false, "run_p".to_string())));
    }

    #[test]
    fn detect_member_access_scope_empty_prefix() {
        let rope = rope_from("my_pkg::");
        let pos = MPosition::new(0, 8);
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((true, String::new())));
    }

    #[test]
    fn detect_member_access_scope_with_prefix() {
        let rope = rope_from("uvm_pkg::uvm_seq");
        let pos = MPosition::new(0, 16);
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((true, "uvm_seq".to_string())));
    }

    #[test]
    fn detect_member_access_no_trigger() {
        let rope = rope_from("my_var");
        let pos = MPosition::new(0, 6);
        assert!(detect_member_access(&rope, pos).is_none());
    }

    #[test]
    fn detect_member_access_out_of_bounds_line() {
        let rope = rope_from("x.y");
        let pos = MPosition::new(99, 0);
        assert!(detect_member_access(&rope, pos).is_none());
    }

    #[test]
    fn detect_member_access_single_colon_not_a_trigger() {
        let rope = rope_from("foo:bar");
        let pos = MPosition::new(0, 7);
        assert!(detect_member_access(&rope, pos).is_none());
    }

    // ------------------------------------------------------------------
    // detect_macro_trigger
    // ------------------------------------------------------------------

    #[test]
    fn detect_macro_trigger_empty_prefix() {
        let rope = rope_from("`");
        let pos = MPosition::new(0, 1);
        assert_eq!(detect_macro_trigger(&rope, pos), Some(String::new()));
    }

    #[test]
    fn detect_macro_trigger_with_prefix() {
        let rope = rope_from("`MY_MACRO");
        let pos = MPosition::new(0, 4);
        assert_eq!(detect_macro_trigger(&rope, pos), Some("MY_".to_string()));
    }

    #[test]
    fn detect_macro_trigger_full_name() {
        let rope = rope_from("`UVM_INFO");
        let pos = MPosition::new(0, 9);
        assert_eq!(
            detect_macro_trigger(&rope, pos),
            Some("UVM_INFO".to_string())
        );
    }

    #[test]
    fn detect_macro_trigger_no_backtick() {
        let rope = rope_from("my_signal");
        let pos = MPosition::new(0, 9);
        assert!(detect_macro_trigger(&rope, pos).is_none());
    }

    #[test]
    fn detect_macro_trigger_dot_not_macro() {
        let rope = rope_from("obj.field");
        let pos = MPosition::new(0, 9);
        assert!(detect_macro_trigger(&rope, pos).is_none());
    }

    #[test]
    fn detect_macro_trigger_oob_line() {
        let rope = rope_from("`M");
        let pos = MPosition::new(99, 0);
        assert!(detect_macro_trigger(&rope, pos).is_none());
    }
}
