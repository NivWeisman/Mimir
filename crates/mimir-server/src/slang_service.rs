//! Slang sidecar service layer: project config, connection management,
//! closed-file disk cache, and all IPC helpers.
//!
//! This module owns the state that was previously scattered across
//! [`crate::backend::Backend`]:
//!
//! * The optional [`mimir_slang::Client`] connection to the sidecar binary.
//! * The resolved [`crate::project::ResolvedProject`] config (filelist,
//!   include dirs, defines, debounce, feature toggles, formatter config).
//! * The [`ClosedFileDiskCache`] that memoises on-disk reads for files not
//!   currently open in the editor.
//! * A shared (borrowed) handle to the live document store, read to
//!   snapshot open-buffer texts when assembling sidecar requests.
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
    Client as SlangClient, CompileResult, ElaborateParams, ExpandMacroParams, ExpandMacroResult,
    SourceFile,
};
use ropey::Rope;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{Position, Range};
use tower_lsp::lsp_types::Url;
use tracing::{debug, warn};

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
    ///
    /// Values are `Arc<str>` so the per-elaborate snapshot in
    /// [`assemble_with_cache`] clones reference counts, not every file's
    /// full text (which used to copy the whole project per keystroke-
    /// debounced compile).
    pub(crate) texts: HashMap<PathBuf, Arc<str>>,
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

    /// Return the project's path-based diagnostic policy (demote / ignore by
    /// path). A no-op default when no project / no `[diagnostics]` table.
    pub(crate) async fn current_diagnostic_policy(
        &self,
    ) -> crate::diag_policy::DiagnosticPolicy {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.diagnostics.clone())
            .unwrap_or_default()
    }

    /// Return the project's resolved UVM-lint settings (from `[diagnostics]`).
    /// Falls back to [`crate::project::UvmLintConfig::default`] (check on, `warning`, the
    /// common phases) when no project config is loaded.
    pub(crate) async fn current_uvm_lint_config(&self) -> crate::project::UvmLintConfig {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.uvm_lint.clone())
            .unwrap_or_default()
    }

    /// Return the project's resolved CodeLens override mode (from
    /// `[code_lens] overrides`). Falls back to the default (`Uvm`) when no
    /// project config is loaded.
    pub(crate) async fn current_code_lens_mode(
        &self,
    ) -> crate::code_lens::OverrideLensMode {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.code_lens_overrides)
            .unwrap_or_default()
    }

    /// Return the project's `` `include `` search directories, in order.
    ///
    /// Used by `textDocument/documentLink` to resolve `` `include "..." ``
    /// targets the same way slang's preprocessor would. Empty when no
    /// project config is loaded (the document-link handler then only resolves
    /// includes relative to the file's own directory).
    pub(crate) async fn current_include_dirs(&self) -> Vec<PathBuf> {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.include_dirs.clone())
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

    /// Return the current [`mimir_syntax::MethodHintMode`] from the resolved
    /// project config.
    ///
    /// Falls back to [`mimir_syntax::MethodHintMode::default`] (`Name`) when
    /// no project config is loaded.
    pub(crate) async fn current_method_hint_mode(&self) -> mimir_syntax::MethodHintMode {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.method_hint_mode)
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
        mimir_core::time_scope!("elaborate.build_params");
        if self.slang.read().await.is_none() {
            return None;
        }
        let project = self.project.read().await.clone()?;

        let open_text: HashMap<PathBuf, (Url, String)> = {
            mimir_core::time_scope!("elaborate.build_params.snapshot_open_docs");
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
        for arg in &params.extra_args {
            arg.hash(&mut h);
        }
        h.finish()
    }

    /// Forward a compile request to the sidecar.
    ///
    /// Returns the full elaborated symbol table as a [`CompileResult`]
    /// (MimirAst JSON + flat diagnostics). The caller is responsible for
    /// caching the resulting [`mimir_ast::MimirAst`].
    pub(crate) async fn compile(&self, params: &ElaborateParams)
    -> Result<CompileResult, mimir_slang::ClientError>
    {
        mimir_core::time_scope!("slang.compile.service_total");
        let Some(slang) = self.slang.read().await.clone() else {
            // The caller's is_configured() check can race a config reload
            // that removed the sidecar — a recoverable state, not a panic.
            warn!("compile requested but no slang sidecar is configured");
            return Err(mimir_slang::ClientError::NotConfigured);
        };
        slang.compile(params).await
    }

    /// Assemble [`ExpandMacroParams`] for the macro usage at `position` in
    /// `target_path`. Reuses the exact same file / include-dir / define
    /// assembly as a compile (via [`Self::build_elaborate_params`]) so the
    /// expansion sees identical preprocessor state — without that, a
    /// `` `uvm_* `` macro defined in an earlier-included header would expand
    /// differently (or not at all) than it elaborates.
    ///
    /// Returns `None` when slang isn't configured or no project is loaded.
    pub(crate) async fn build_expand_macro_params(
        &self,
        target_path: String,
        position: MPosition,
    ) -> Option<ExpandMacroParams> {
        let (ep, _urls) = self.build_elaborate_params().await?;
        Some(ExpandMacroParams {
            files: ep.files,
            include_dirs: ep.include_dirs,
            defines: ep.defines,
            extra_args: ep.extra_args,
            single_unit: ep.single_unit,
            timescale: ep.timescale,
            target_path,
            position,
        })
    }

    /// Forward an `expandMacro` request to the sidecar, blocking on the
    /// connection if a compile is in flight. Used by the explicit "Expand
    /// Macro" command.
    pub(crate) async fn expand_macro(&self, params: &ExpandMacroParams)
    -> Result<ExpandMacroResult, mimir_slang::ClientError>
    {
        mimir_core::time_scope!("slang.expand_macro.service_total");
        let Some(slang) = self.slang.read().await.clone() else {
            warn!("expand_macro requested but no slang sidecar is configured");
            return Err(mimir_slang::ClientError::NotConfigured);
        };
        slang.expand_macro(params).await
    }

    /// Non-blocking `expandMacro` for the opportunistic hover footer: returns
    /// [`mimir_slang::ClientError::Busy`] without queuing when the sidecar
    /// connection is occupied by a background elaborate, so a hover never
    /// stalls behind a slow compile.
    pub(crate) async fn expand_macro_if_idle(&self, params: &ExpandMacroParams)
    -> Result<ExpandMacroResult, mimir_slang::ClientError>
    {
        mimir_core::time_scope!("slang.expand_macro.service_total");
        let Some(slang) = self.slang.read().await.clone() else {
            warn!("expand_macro_if_idle requested but no slang sidecar is configured");
            return Err(mimir_slang::ClientError::NotConfigured);
        };
        slang.try_expand_macro(params).await
    }

}

// --------------------------------------------------------------------------
// Free helpers (pub(crate) so backend.rs can call them)
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
    let buf = mimir_syntax::symbols::line_prefix_at(rope, pos)?;
    let start = mimir_syntax::symbols::trailing_ident_start(&buf);
    let (before, prefix) = buf.split_at(start);

    if before.ends_with('.') {
        return Some((false, prefix.to_owned()));
    }
    if before.ends_with("::") {
        return Some((true, prefix.to_owned()));
    }

    None
}

/// Detect a `` ` `` macro trigger before the cursor.
///
/// Returns `Some(prefix)` when the identifier (or empty string) immediately
/// before the cursor is preceded by a backtick. Returns `None` otherwise.
pub(crate) fn detect_macro_trigger(rope: &Rope, pos: MPosition) -> Option<String> {
    let buf = mimir_syntax::symbols::line_prefix_at(rope, pos)?;
    // `trailing_ident_start` includes `$` in its identifier set; macro names
    // can't contain `$`, but a `$` directly after a backtick isn't valid SV
    // either way, so sharing the scanner only changes behaviour on input
    // that never completes to anything.
    let start = mimir_syntax::symbols::trailing_ident_start(&buf);
    let (before, prefix) = buf.split_at(start);

    if before.ends_with('`') {
        return Some(prefix.to_owned());
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
    mimir_core::time_scope!("elaborate.assemble_with_cache");

    // Phase 1: snapshot existing cached texts. `Arc<str>` values make this
    // a pointer-clone per entry, not a copy of every file's text.
    let cached_texts: HashMap<PathBuf, Arc<str>> = {
        mimir_core::time_scope!("elaborate.assemble.cache_snapshot");
        closed_file_cache
            .read()
            .await
            .as_ref()
            .map(|c| c.texts.clone())
            .unwrap_or_default()
    };

    // Phase 2: assemble params — no lock held; disk reads happen here.
    let mut new_entries: HashMap<PathBuf, Arc<str>> = HashMap::new();
    let result = {
        mimir_core::time_scope!("elaborate.assemble.disk_reads_and_params");
        assemble_elaborate_params(project, open_text, |path| {
            if let Some(text) = cached_texts.get(path) {
                return Some(text.clone());
            }
            let text: Arc<str> = Arc::from(std::fs::read_to_string(path).ok()?);
            new_entries.insert(path.to_path_buf(), text.clone());
            Some(text)
        })
    };

    // Phase 3: merge newly-read texts into the cache.
    {
        mimir_core::time_scope!("elaborate.assemble.cache_merge");
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
    mut read_disk: impl FnMut(&Path) -> Option<Arc<str>>,
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
                // One copy into the wire type is unavoidable (`SourceFile.text`
                // is an owned `String`); the cached `Arc<str>` itself is shared.
                let text = read_disk(project_file)
                    .map(|t| t.to_string())
                    .unwrap_or_default();
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
        extra_args: project.slang_extra_args.clone(),
        single_unit: project.single_unit,
        timescale: project.timescale.clone(),
    };
    (params, files_in_request)
}

/// Last-ditch fallback when `Url::from_file_path` rejects a path.
fn placeholder_url(p: &Path) -> Url {
    Url::parse(&format!("file://{}", p.display()))
        .unwrap_or_else(|_| Url::parse("file:///").expect("file:/// is always valid"))
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
            method_hint_mode: mimir_syntax::MethodHintMode::default(),
            slang_extra_args: vec![],
            single_unit: false,
            timescale: None,
            diagnostics: crate::diag_policy::DiagnosticPolicy::default(),
            uvm_lint: crate::project::UvmLintConfig::default(),
            code_lens_overrides: crate::code_lens::OverrideLensMode::default(),
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
            assemble_elaborate_params(&project, &open_text, |_| Some(Arc::from("")));

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
            assemble_elaborate_params(&project, &HashMap::new(), |_| Some(Arc::from("")));

        assert_eq!(params.files.len(), 1);
        assert_eq!(files_in_request.len(), 1);
    }

    #[test]
    fn assemble_does_not_expand_includes_into_request() {
        let umbrella = PathBuf::from("/proj/uvm.sv");
        let mut project = project_with_files(vec![umbrella.clone()]);
        project.include_dirs = vec![PathBuf::from("/uvm/src")];

        let pkg = PathBuf::from("/uvm/src/uvm_pkg.sv");
        let texts: HashMap<PathBuf, Arc<str>> = HashMap::from([
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

    // ------------------------------------------------------------------
    // hash_inputs
    // ------------------------------------------------------------------

    fn minimal_params() -> ElaborateParams {
        ElaborateParams {
            files: vec![],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: false,
            timescale: None,
        }
    }

    /// Different `extra_args` produce a different hash so a TOML change
    /// triggers a fresh sidecar compile rather than being skipped as unchanged.
    #[test]
    fn hash_changes_when_extra_args_differ() {
        let base = minimal_params();
        let mut with_ts = base.clone();
        with_ts.extra_args = vec!["--timescale".into(), "1ns/1ps".into()];
        assert_ne!(SlangService::hash_inputs(&base), SlangService::hash_inputs(&with_ts));
    }

    /// Two calls with identical `extra_args` (in the same order) hash the same.
    #[test]
    fn hash_stable_with_same_extra_args() {
        let mut p = minimal_params();
        p.extra_args = vec!["--timescale".into(), "1ns/1ps".into()];
        assert_eq!(SlangService::hash_inputs(&p), SlangService::hash_inputs(&p));
    }
}
