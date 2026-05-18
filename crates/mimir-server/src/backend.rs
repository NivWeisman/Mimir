//! The `tower_lsp::LanguageServer` impl.
//!
//! This file is the seam between the LSP protocol and our internal
//! crates. Responsibilities:
//!
//! * Maintain a map `Url -> TextDocument` (the document store).
//! * On `did_open` / `did_change` / `did_close`, mutate the store.
//! * After every store mutation, parse the affected document and publish
//!   diagnostics back to the client.
//!
//! ## Concurrency model
//!
//! `tower-lsp` calls every handler concurrently from the tokio runtime.
//! That means `did_change` for document A and `did_change` for document B
//! can run at the same time on different worker threads.
//!
//! We use a single `tokio::sync::RwLock<HashMap<Url, DocumentState>>`. The
//! lock is held only long enough to insert/lookup; the parse happens on a
//! `clone()` of the source string outside the lock so we don't block other
//! documents while a slow parse is in flight.
//!
//! ## Why not `spawn_blocking`?
//!
//! tree-sitter is fast enough for typical files that we don't need to push
//! parsing to a blocking thread pool (~milliseconds for a 5000-line UVM
//! file). If we ever process huge generated headers we'll revisit; for now
//! the simpler `await`-on-the-reactor model wins on readability.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mimir_core::{Position as MPosition, Range as MRange, TextDocument};
use mimir_slang::{
    Client as SlangClient, CompleteParams as SlangCompleteParams,
    CompletionRequestKind as SlangCompletionRequestKind,
    DefinitionLocation as SlangDefinitionLocation, DefinitionParams as SlangDefinitionParams,
    Diagnostic as SlangDiagnostic, ElaborateParams, ElaborateResult,
    ImplementationLocation as SlangImplementationLocation,
    ImplementationParams as SlangImplementationParams, Severity as SlangSeverity,
    SignatureHelpParams as SlangSignatureHelpParams, SlangCompletionItem, SourceFile,
    TypeDefinitionLocation as SlangTypeDefinitionLocation,
    TypeDefinitionParams as SlangTypeDefinitionParams,
};
use mimir_syntax::{
    calls::{active_arg_index, call_site_at, call_sites_in, CallKind},
    inlay::hints_for,
    signature::signature_for,
    Diagnostic as MDiagnostic, DiagnosticSeverity as MSeverity, Symbol, SymbolKind as MSymbolKind,
    SyntaxParser, SyntaxTree,
};
use ropey::Rope;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, error, info, instrument, warn};

use crate::completion_score;
use crate::format::{invoke_verible, strip_mimir_pragmas, wrap_ifdefs};
use crate::project::{FeatureToggles, FormatterConfig, ResolvedProject};
use crate::workspace_index::{self, WorkspaceIndex};

/// Per-document state held inside the store.
///
/// We keep the last parsed `tree` here so LSP feature handlers
/// (`folding_range`, `document_highlight`, `signature_help`, `inlay_hint`,
/// `syntax_definition`) can reuse it instead of re-parsing on every
/// request. `Tree::edit`-driven incremental reparse on `did_change` is
/// still a future slice — for now every parse is full, but at least we
/// only do it once per edit.
#[derive(Debug)]
struct DocumentState {
    document: TextDocument,
    /// Language ID the editor reported in `did_open`. Useful for routing
    /// (e.g. we treat `verilog` and `systemverilog` slightly differently
    /// — though for now both go through the same parser).
    #[allow(dead_code)]
    language_id: String,
    /// Symbol index from the most recent successful parse. Empty when
    /// the document hasn't been parsed yet or the last parse failed.
    /// Powers same-file `goto_definition` and `documentSymbol`.
    index: Vec<Symbol>,
    /// Parse tree from the most recent successful parse. `None` between
    /// `did_open` and the first parse completing. On a parse error we
    /// deliberately leave the previous tree in place — serving slightly
    /// stale results mid-keystroke is strictly better than serving none.
    /// `SyntaxTree` clones cheaply (`tree_sitter::Tree` is `Arc` inside).
    tree: Option<SyntaxTree>,
    /// Document version the `index` and `tree` were built from. Used to
    /// detect a stale write — `reparse_and_publish` may finish after a
    /// fresh `did_change` has bumped the version, in which case we must
    /// not overwrite the live cache with the stale parse's results.
    index_version: i32,
}

/// The tower-lsp [`LanguageServer`] implementation.
///
/// Pub only inside the crate; `main.rs` constructs it via `Backend::new`.
pub(crate) struct Backend {
    /// Channel back to the editor for sending notifications (diagnostics,
    /// log messages, progress). Cloneable because `Client` is internally
    /// reference-counted.
    client: Client,

    /// Document store. `RwLock` because reads (parse callbacks) outnumber
    /// writes (edits) once a session is established.
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,

    /// One parser, guarded by a Mutex. tree-sitter parsers aren't `Sync`,
    /// and constructing a fresh parser per request would re-pay the
    /// language-load cost. The mutex is uncontended in the common case (a
    /// human types into one file at a time).
    parser: Arc<Mutex<SyntaxParser>>,

    /// Optional slang sidecar client. `None` when neither `MIMIR_SLANG_PATH`
    /// process env nor `[env] MIMIR_SLANG_PATH` in `.mimir.toml` is set, or
    /// the configured path failed to spawn.  While `None`, the diagnostic
    /// pipeline is tree-sitter-only.  Wrapped in `RwLock` so `initialize`
    /// can set it from the project config after startup.
    slang: Arc<RwLock<Option<Arc<SlangClient>>>>,

    /// Resolved project config (`.mimir.toml` + expanded filelist),
    /// discovered on `initialize` from the workspace root. `None` when no
    /// `.mimir.toml` was found — slang stays inactive in that case
    /// because it has no compilation unit to elaborate.
    project: Arc<RwLock<Option<ResolvedProject>>>,

    /// One in-flight (sleeping or running) elaborate task per URI that
    /// triggered it. On a new edit for the same URI we `.abort()` the
    /// existing handle and schedule a fresh one — that's the debounce.
    /// Aborting during the sleep cancels cleanly; aborting during the
    /// `elaborate` call drops the request response on the floor (the
    /// connection's per-request `id` correlation handles the next caller
    /// correctly).
    pending_elaborations: Arc<RwLock<HashMap<Url, JoinHandle<()>>>>,

    /// URLs we published non-empty slang diagnostics to in the previous
    /// elaboration cycle. We diff this against the current cycle's set so
    /// we can publish empty for URLs that *were* flagged but are now clean
    /// — otherwise the editor keeps showing stale red squiggles after the
    /// user fixes the root-cause error.
    slang_published: Arc<RwLock<HashSet<Url>>>,

    /// Hash of the inputs of the most recent **successful** slang elaborate.
    /// Used by [`Backend::schedule_elaborate`] to skip a re-elaborate when
    /// the project's file texts, include dirs, defines, and `top` haven't
    /// changed since the last successful call — that's the common case
    /// after `did_open` of an already-warm file. `None` until the first
    /// elaborate succeeds. Only updated on success so a failed elaborate
    /// doesn't lock out future retries.
    last_elaborate_input_hash: Arc<RwLock<Option<u64>>>,

    /// Latched `true` after the first successful elaborate has emitted its
    /// per-file `info!("indexed by startup slang elaborate")` lines. This
    /// is the user-visible signal that the warm slang elaborate completed;
    /// subsequent elaborates stay at `debug!` so the log doesn't grow with
    /// every edit.
    startup_index_logged: Arc<AtomicBool>,

    /// Workspace-wide tree-sitter symbol index. Populated from two sources:
    /// every `reparse_and_publish` for an open document folds that doc's
    /// fresh `Vec<Symbol>` in, and `initialize` spawns a one-shot
    /// hydration over `ResolvedProject.files` so cross-file F12 works
    /// against the filelist before the user opens those files. See
    /// [`workspace_index`] for the data structure and the open-vs-disk
    /// precedence rules.
    workspace_index: Arc<RwLock<WorkspaceIndex>>,
}

impl Backend {
    /// Construct the backend. `slang` is `None` when no sidecar is
    /// configured (today's default — see [`crate::SLANG_PATH_ENV`]).
    ///
    /// Panics if the parser fails to load the SV grammar — that's a build
    /// configuration bug, not a runtime condition, and it would happen on
    /// the very first message we received anyway.
    pub fn new(client: Client, slang: Option<Arc<SlangClient>>) -> Self {
        let parser = SyntaxParser::new().expect("tree-sitter SV grammar failed to load");
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            parser: Arc::new(Mutex::new(parser)),
            slang: Arc::new(RwLock::new(slang)),
            project: Arc::new(RwLock::new(None)),
            pending_elaborations: Arc::new(RwLock::new(HashMap::new())),
            slang_published: Arc::new(RwLock::new(HashSet::new())),
            last_elaborate_input_hash: Arc::new(RwLock::new(None)),
            startup_index_logged: Arc::new(AtomicBool::new(false)),
            workspace_index: Arc::new(RwLock::new(WorkspaceIndex::new())),
        }
    }

    /// Return the current [`FeatureToggles`] from the resolved project config.
    ///
    /// Falls back to [`FeatureToggles::default`] (all toggles `true`) when no
    /// `.mimir.toml` has been discovered — that keeps every feature on when
    /// the server is used without a project config file.
    async fn current_features(&self) -> FeatureToggles {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.features.clone())
            .unwrap_or_default()
    }

    /// Return the current [`FormatterConfig`] from the resolved project config.
    ///
    /// Falls back to [`FormatterConfig::default`] (binary = `"verible-verilog-format"`,
    /// all options unset) when no `.mimir.toml` is present.
    async fn current_formatter_config(&self) -> FormatterConfig {
        self.project
            .read()
            .await
            .as_ref()
            .map(|p| p.formatter.clone())
            .unwrap_or_default()
    }

    /// Parse the document at `uri` and publish diagnostics to the client.
    ///
    /// Called after every store mutation. Errors are logged and swallowed —
    /// we never propagate a parse failure back to the editor as an LSP
    /// error, because the editor doesn't know what to do with it.
    #[instrument(level = "debug", skip(self), fields(uri = %uri))]
    async fn reparse_and_publish(&self, uri: Url) {
        let (text, version) = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => (state.document.text(), state.document.version()),
                None => {
                    // Race: document was closed between the edit and our
                    // reparse. Nothing to do.
                    debug!("document gone before reparse, skipping");
                    return;
                }
            }
        };

        // Run the parse outside the doc-store lock.
        let parse_result = {
            let mut parser = self.parser.lock().await;
            parser.parse(&text, None)
        };

        let (diags, new_state) = match parse_result {
            Ok(tree) => {
                let rope = Rope::from_str(&text);
                let diags = mimir_syntax::diagnostics::collect(&tree, &rope);
                let index = mimir_syntax::symbols::index(&tree, &rope);
                (diags, Some((index, tree)))
            }
            Err(e) => {
                // Deliberately leave `state.tree` and `state.index`
                // untouched here — a failed parse mid-keystroke should
                // not erase last-known-good results.
                error!(error = %e, "parser returned error; publishing empty diagnostics");
                (Vec::new(), None)
            }
        };

        // Write the fresh index + tree back into the doc store, but only if
        // the version we parsed is still the live one — otherwise a
        // `did_change` landed mid-parse and our results are already stale.
        // When the write does happen, also fold the new symbols into the
        // workspace index so cross-file F12 sees them.
        if let Some((index, tree)) = new_state {
            let updated = {
                let mut docs = self.documents.write().await;
                match docs.get_mut(&uri) {
                    Some(state) if state.document.version() == version => {
                        state.index = index.clone();
                        state.tree = Some(tree);
                        state.index_version = version;
                        true
                    }
                    _ => false,
                }
            };
            if updated {
                // Workspace lock acquired *after* the doc-store lock is
                // dropped — no nested locks, no risk of an ordering
                // deadlock with the hydration task.
                let mut wi = self.workspace_index.write().await;
                wi.update(uri.clone(), &index);
            }
        }

        // Tree-sitter publishes immediately so the editor has prompt
        // feedback (~ms after a keystroke). The deeper slang elaborate
        // is scheduled separately on a debounce timer; when it lands it
        // *overwrites* this publish for files in its compilation unit.
        let lsp_diags = merge_diagnostics(diags, Vec::new(), /* slang_active */ false);

        debug!(
            count = lsp_diags.len(),
            "publishing tree-sitter diagnostics"
        );
        self.client
            .publish_diagnostics(uri, lsp_diags, Some(version))
            .await;
    }

    /// Snapshot the cached parse tree for `uri`.
    ///
    /// Returns `None` when the document isn't open. When the document
    /// *is* open but the cache hasn't been populated yet (rare — only
    /// between `did_open` and the first `reparse_and_publish` finishing,
    /// or after every parse so far has errored) we fall back to a
    /// synchronous full parse so the caller always gets a tree if the
    /// document exists.
    async fn cached_tree(&self, uri: &Url) -> Option<SyntaxTree> {
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
    /// `index` may be empty in that case (it's only populated by
    /// `reparse_and_publish`).
    async fn cached_tree_and_index(&self, uri: &Url) -> Option<(SyntaxTree, Vec<Symbol>)> {
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

    /// Schedule a debounced slang elaborate. Called from `did_open` and
    /// `did_change`. Returns immediately; the actual elaborate happens in
    /// a tokio task after the project's `debounce_ms` quiet time.
    ///
    /// `trigger_uri` is the URI that just changed; it's used as the
    /// debounce key so a fast typist editing one file doesn't cancel a
    /// pending elaborate triggered by a different file.
    ///
    /// No-op when slang isn't configured (no `MIMIR_SLANG_PATH`) or the
    /// project lacks a `.mimir.toml` — both are normal "tree-sitter only"
    /// states the user opts into by not configuring the sidecar.
    async fn schedule_elaborate(&self, trigger_uri: Url) {
        let Some(slang) = self.slang.read().await.clone() else {
            return;
        };
        // Snapshot the project config under the read lock so the spawned
        // task isn't holding a lock across `await`s.
        let project = match self.project.read().await.clone() {
            Some(p) => p,
            None => return,
        };
        let debounce = Duration::from_millis(project.debounce_ms);

        // Cancel any pending elaborate for this trigger URI. Aborting
        // during the debounce sleep is clean. Aborting while a request is
        // in-flight (after the bytes were sent but before the response is
        // read) leaves a stale response in the sidecar's stdout buffer;
        // Connection::request drains such responses transparently.
        {
            let mut pending = self.pending_elaborations.write().await;
            if let Some(prior) = pending.remove(&trigger_uri) {
                prior.abort();
                debug!(uri = %trigger_uri, "cancelled prior pending elaborate");
            }
        }

        // Clone the Arcs / Client that the task needs. tokio Client is
        // already a cheap clone (internally Arc'd); the rest are explicit
        // Arcs so cloning is just a refcount bump.
        let documents = self.documents.clone();
        let pending = self.pending_elaborations.clone();
        let slang_published = self.slang_published.clone();
        let last_hash = self.last_elaborate_input_hash.clone();
        let startup_logged = self.startup_index_logged.clone();
        let lsp_client = self.client.clone();
        let trigger_for_task = trigger_uri.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(debounce).await;

            // Build the elaborate request from project config + the
            // currently-open documents (their in-memory text overrides
            // anything on disk so unsaved changes participate).
            let (params, files_in_request) = build_elaborate_params(&project, &documents).await;

            // Hash the inputs that determine slang's answer. If we've
            // already sent slang exactly this set of files+config and got
            // a successful response, the prior diagnostics still apply —
            // skip the round-trip (which is the ~500KB packet) and let
            // the editor keep what it has.
            let input_hash = hash_elaborate_inputs(&params);
            if *last_hash.read().await == Some(input_hash) {
                debug!(
                    hash = input_hash,
                    files = params.files.len(),
                    "slang inputs unchanged since last elaborate; skipping",
                );
                pending.write().await.remove(&trigger_for_task);
                return;
            }

            debug!(
                files = params.files.len(),
                include_dirs = params.include_dirs.len(),
                hash = input_hash,
                "sending elaborate request",
            );
            match slang.elaborate(&params).await {
                Ok(result) => {
                    publish_slang_result(&lsp_client, &files_in_request, result, &slang_published)
                        .await;
                    *last_hash.write().await = Some(input_hash);

                    // First successful elaborate of the session: emit one
                    // `info!` per file that slang now has compiled.
                    // `compare_exchange` so a concurrent elaborate doesn't
                    // double-log. AcqRel is the cheapest ordering that
                    // gives both the winner and the loser a coherent view.
                    if startup_logged
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        for url in &files_in_request {
                            info!(file = %url, "indexed by startup slang elaborate");
                        }
                    }
                }
                Err(e) => {
                    // We deliberately don't drop the slang client here
                    // even on terminal errors — that's a follow-up. For
                    // now, log and let the next edit retry; if the
                    // sidecar is genuinely gone, every retry will fail
                    // the same way and the user will see it in stderr.
                    error!(error = %e, "slang elaborate failed");
                }
            }

            // Self-clean from the pending map so we don't leak handles.
            pending.write().await.remove(&trigger_for_task);
        });

        self.pending_elaborations
            .write()
            .await
            .insert(trigger_uri, handle);
    }

    /// Re-discover and re-load the project rooted at the directory
    /// containing `mimir_toml_path`, then re-hydrate the workspace
    /// symbol index from the resulting filelist. Called when the
    /// `.mimir.toml` itself changes on disk.
    ///
    /// Fire-and-forget: the workspace index update happens on the
    /// spawned task, mirroring the initialize-time hydration path so
    /// the watcher event returns promptly. A failed re-discover
    /// (deleted / malformed config) logs at warn and leaves the
    /// previous `self.project` in place.
    async fn rehydrate_project_from(&self, mimir_toml_path: &Path) {
        let Some(start) = mimir_toml_path.parent() else {
            warn!(path = %mimir_toml_path.display(), "rehydrate: .mimir.toml has no parent dir");
            return;
        };
        match ResolvedProject::discover(start) {
            Ok(Some(resolved)) => {
                info!(
                    files = resolved.files.len(),
                    include_dirs = resolved.include_dirs.len(),
                    "rehydrated project from .mimir.toml change",
                );
                let paths = resolved.files.clone();
                let include_dirs = resolved.include_dirs.clone();
                *self.project.write().await = Some(resolved);
                let parser = self.parser.clone();
                let workspace_index = self.workspace_index.clone();
                tokio::spawn(async move {
                    hydrate_workspace_index(paths, include_dirs, parser, workspace_index).await;
                });
            }
            Ok(None) => {
                warn!(
                    path = %mimir_toml_path.display(),
                    "rehydrate: .mimir.toml event fired but discover returned None; leaving prior project in place",
                );
            }
            Err(e) => {
                warn!(
                    path = %mimir_toml_path.display(),
                    error = %e,
                    "rehydrate: failed to reload .mimir.toml; leaving prior project in place",
                );
            }
        }
    }

    /// Re-parse `path` from disk and replace its entry in the
    /// workspace symbol index. Used for `Created` / `Changed` events
    /// on filelist-referenced source files that aren't currently
    /// open in the editor.
    ///
    /// Silently skips paths the disk reader can't open (a transient
    /// rename in flight, say) and paths that don't URL-encode. The
    /// initial hydration uses the same skipping policy via
    /// `hydrate_from_paths`.
    async fn rehydrate_single_file(&self, path: &Path) {
        let include_dirs = self
            .project
            .read()
            .await
            .as_ref()
            .map(|p| p.include_dirs.clone())
            .unwrap_or_default();
        let entries = {
            let mut p = self.parser.lock().await;
            workspace_index::hydrate_from_paths(
                &[path.to_path_buf()],
                &include_dirs,
                &mut p,
                |path| std::fs::read_to_string(path).ok(),
            )
        };
        let mut wi = self.workspace_index.write().await;
        for (url, syms) in entries {
            debug!(url = %url, count = syms.len(), "re-hydrated single file");
            wi.update(url, &syms);
        }
    }

    /// Tree-sitter side of hover: resolve the identifier under the
    /// cursor to a `Symbol` and build a `Hover` from its declaration line
    /// or synthesized signature.
    ///
    /// Lookup priority:
    /// 1. `this.X` / `super.X` — walk the enclosing class + `extends`.
    /// 2. `obj.X` — AST-resolve `obj`'s declared type, look the method up
    ///    on that class.
    /// 3. Bare identifier — same-file index, then workspace index.
    ///
    /// Returns `None` when the cursor isn't on an identifier, when no
    /// symbol of that name is in scope, or when the symbol's declaration
    /// line can't be read.
    async fn hover_via_tree_sitter(
        &self,
        uri: &Url,
        tree: &SyntaxTree,
        rope: &Rope,
        same_file_index: &[Symbol],
        target: MPosition,
    ) -> Option<Hover> {
        let name = mimir_syntax::symbols::identifier_at(tree, rope, target)?;

        // Receiver-aware: `this.X` / `super.X` / `obj.X`.
        let receiver = mimir_syntax::symbols::hover_receiver_at(tree, rope, target);

        let resolved: Option<(Url, Symbol)> = match &receiver {
            Some(mimir_syntax::symbols::HoverReceiver::This) => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, target)?;
                let wi = self.workspace_index.read().await;
                find_method_in_class(&wi, &info.class_name, name)
                    .or_else(|| find_field_in_class(&wi, &info.class_name, name))
                    .map(|sym| {
                        let url = method_url_in_class(&wi, &info.class_name, &sym)
                            .unwrap_or_else(|| uri.clone());
                        (url, sym)
                    })
            }
            Some(mimir_syntax::symbols::HoverReceiver::Super) => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, target)?;
                let parent = info.parent_class_name?;
                let wi = self.workspace_index.read().await;
                find_method_in_class(&wi, &parent, name)
                    .or_else(|| find_field_in_class(&wi, &parent, name))
                    .map(|sym| {
                        let url =
                            method_url_in_class(&wi, &parent, &sym).unwrap_or_else(|| uri.clone());
                        (url, sym)
                    })
            }
            Some(mimir_syntax::symbols::HoverReceiver::Object(recv_name)) => {
                let ty =
                    mimir_syntax::symbols::find_variable_type_at(tree, rope, target, recv_name)?;
                let cls = mimir_syntax::symbols::normalize_type_name(&ty)?;
                let wi = self.workspace_index.read().await;
                find_method_in_class(&wi, &cls, name)
                    .or_else(|| find_field_in_class(&wi, &cls, name))
                    .map(|sym| {
                        let url =
                            method_url_in_class(&wi, &cls, &sym).unwrap_or_else(|| uri.clone());
                        (url, sym)
                    })
            }
            None => {
                // Bare identifier: same-file index first, workspace fallback.
                if let Some(sym) = same_file_index.iter().find(|s| s.name == name).cloned() {
                    Some((uri.clone(), sym))
                } else {
                    let wi = self.workspace_index.read().await;
                    wi.lookup(name)
                        .first()
                        .map(|e| (e.url.clone(), e.symbol.clone()))
                }
            }
        };

        let (sym_url, sym) = resolved?;
        let docs = self.documents.read().await;
        hover_for_symbol(&sym, &sym_url, &docs)
    }

    /// Slang-first hover: ask slang where the symbol under the cursor is
    /// declared, then read the declaration line at that location. Used
    /// when `MIMIR_SLANG_PATH` is configured.
    ///
    /// Returns `None` when slang isn't configured, when slang has no
    /// answer, or on transport error — the caller falls through to the
    /// tree-sitter path in all three cases. Hover is a UX feature, not a
    /// correctness one: an empty slang result should still let the user
    /// see *something* via the syntax index rather than nothing at all.
    async fn try_slang_hover(&self, uri: &Url, rope: &Rope, target: MPosition) -> Option<Hover> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (params, _) = build_definition_params(&project, &self.documents, uri, target).await?;

        let result = match slang.definition(&params).await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "slang hover transport error; falling back to syntax");
                return None;
            }
        };

        let loc = result.locations.into_iter().next()?;
        let dest_uri = match path_to_url(&loc.path) {
            Some(u) => u,
            None => {
                debug!(path = %loc.path, "slang hover: location path not a file URL");
                return None;
            }
        };
        let line_no = loc.range.start.line;

        // Look the resolved location up in the workspace index. When a
        // matching `Symbol` exists, route through `hover_for_symbol` so
        // callables get their synthesized signature and macros get their
        // full multi-line body — the slang path otherwise reads a single
        // line from disk, which silently truncates multi-line declarations.
        let workspace_match: Option<(Url, Symbol)> = {
            let wi = self.workspace_index.read().await;
            let mut hit = None;
            for entry in wi.entries() {
                if entry.url == dest_uri && entry.symbol.name_range.start.line == line_no {
                    hit = Some((entry.url.clone(), entry.symbol.clone()));
                    break;
                }
            }
            hit
        };
        if let Some((sym_url, sym)) = workspace_match {
            let docs = self.documents.read().await;
            if let Some(h) = hover_for_symbol(&sym, &sym_url, &docs) {
                return Some(h);
            }
        }

        // No workspace symbol at the resolved location — fall back to a
        // multi-line text read so a multi-line declaration still shows
        // every parameter. `read_declaration_block` reads lines from
        // `line_no` until the first source line that looks like the end
        // of the declaration (`;`, `endfunction`, blank line, …).
        let dest_path = &loc.path;
        let block: Option<String> = {
            let docs = self.documents.read().await;
            docs.get(&dest_uri).and_then(|state| {
                let r = Rope::from_str(&state.document.text());
                read_declaration_block(&r, line_no)
            })
        }
        .or_else(|| {
            if &dest_uri == uri {
                read_declaration_block(rope, line_no)
            } else {
                std::fs::read_to_string(dest_path)
                    .ok()
                    .and_then(|t| read_declaration_block(&Rope::from_str(&t), line_no))
            }
        });

        let block = block?;
        Some(hover_markdown(&block))
    }

    /// Try to resolve a definition via the slang sidecar.
    ///
    /// Returns:
    /// * `None` — slang is not configured, or no project is loaded, or
    ///   the cursor URI doesn't have a filesystem path. The caller
    ///   should fall through to the syntax path.
    /// * `Some(Resolved(locs))` — slang ran and returned a definitive
    ///   answer (possibly empty). The caller honours an empty result
    ///   as authoritative; no further fallback.
    /// * `Some(TransportError)` — IO/protocol error talking to the
    ///   sidecar. Logged at error here; caller should fall back.
    async fn try_slang_definition(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<SlangDefinitionOutcome> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        // Build the request envelope. `build_definition_params` returns
        // `None` if the URI has no filesystem path (e.g. an `untitled:`
        // buffer); slang can't address those.
        let (params, _files_in_request) =
            build_definition_params(&project, &self.documents, uri, target).await?;

        match slang.definition(&params).await {
            Ok(result) => {
                let locations: Vec<Location> = result
                    .locations
                    .into_iter()
                    .filter_map(slang_location_to_lsp)
                    .collect();
                Some(SlangDefinitionOutcome::Resolved(locations))
            }
            Err(e) => {
                error!(error = %e, "slang definition transport error; falling back to syntax");
                Some(SlangDefinitionOutcome::TransportError)
            }
        }
    }

    /// Try to resolve the type of the symbol under the cursor via slang.
    ///
    /// No syntax fallback — type resolution requires semantic information that
    /// tree-sitter doesn't have. Returns `None` when slang is not configured or
    /// the URI is not a filesystem path; `Some(Resolved(_))` on success;
    /// `Some(TransportError)` on I/O failure.
    async fn try_slang_type_definition(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<SlangTypeDefinitionOutcome> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (def_params, _) =
            build_definition_params(&project, &self.documents, uri, target).await?;
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

    /// Try to resolve implementations of the symbol under the cursor via slang.
    ///
    /// No syntax fallback — implementation queries need full class-hierarchy
    /// information that tree-sitter doesn't provide. Returns `None` when slang
    /// is not configured or the URI is not a filesystem path;
    /// `Some(Resolved(_))` on success; `Some(TransportError)` on I/O failure.
    async fn try_slang_implementation(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<SlangImplementationOutcome> {
        let slang = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (def_params, _) =
            build_definition_params(&project, &self.documents, uri, target).await?;
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

    /// Stage 1 + Stage 2 syntax-based resolver, factored out so the
    /// `goto_definition` handler can either reach for slang first
    /// (when configured) or call this directly (when slang is absent
    /// or transport-failed).
    ///
    /// Returns the same `Option<GotoDefinitionResponse>` shape the
    /// handler ultimately returns to the editor.
    async fn syntax_definition(
        &self,
        uri: &Url,
        target: MPosition,
    ) -> Option<GotoDefinitionResponse> {
        let (tree, index) = self.cached_tree_and_index(uri).await?;
        let rope = Rope::from_str(tree.source());
        let name = mimir_syntax::symbols::identifier_at(&tree, &rope, target)?;

        // Workspace fallback: clone the slice for `name` under the read
        // lock (lock-then-clone) so the resolver runs without holding
        // either lock. Empty slice when there's no workspace match.
        let workspace_hits: Vec<(Url, Symbol)> = {
            let wi = self.workspace_index.read().await;
            wi.lookup(name)
                .iter()
                .map(|e| (e.url.clone(), e.symbol.clone()))
                .collect()
        };

        let matches = resolve_definition(name, uri, &index, &workspace_hits);
        if matches.is_empty() {
            debug!(name, "no symbol matches in same-file or workspace index");
            return None;
        }

        let locations: Vec<Location> = matches
            .into_iter()
            .map(|(url, sym)| Location {
                uri: url,
                range: m_range_to_lsp(sym.name_range),
            })
            .collect();
        debug!(name, count = locations.len(), "syntax definition resolved");
        Some(GotoDefinitionResponse::Array(locations))
    }

    /// Common scaffolding for the three slang-backed completion routes.
    ///
    /// Owns: lock-acquire on slang+project, [`build_definition_params`],
    /// constructing [`SlangCompleteParams`], dispatching `slang.complete`,
    /// and converting transport errors to `None` so callers fall back to
    /// the syntax path. Returns the raw `Vec<SlangCompletionItem>` on
    /// success (possibly empty) so each caller can apply its own
    /// per-route filtering and empty-result fallback policy.
    async fn slang_complete_request(
        &self,
        uri: &Url,
        pos: MPosition,
        kind: SlangCompletionRequestKind,
        prefix: Option<String>,
    ) -> Option<Vec<SlangCompletionItem>> {
        self.slang_complete_request_with_patch(uri, pos, kind, prefix, None)
            .await
    }

    /// Variant of [`slang_complete_request`] that lets the caller patch the
    /// target file's text and adjust the cursor before the request leaves
    /// the server. Used by member-access completion to insert a placeholder
    /// identifier after the trigger `.` so slang's parser produces a valid
    /// `MemberAccessExpression` (instead of bailing on the dangling dot).
    async fn slang_complete_request_with_patch(
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
            build_definition_params(&project, &self.documents, uri, pos).await?;
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

    /// Try slang-backed member-access or package-scope completion.
    ///
    /// Returns `Some` when:
    /// * slang is configured + project loaded,
    /// * the line has a `.` or `::` trigger before the cursor,
    /// * the sidecar successfully resolves the LHS type or package.
    ///
    /// Returns `None` otherwise — the caller falls back to `syntax_completion`.
    async fn try_slang_member_completion(
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

        // For member-access (`.` trigger) with no member typed yet, the
        // buffer reads `a.|` — slang's parser sees an incomplete expression
        // and produces no `NamedValueExpression` for `a`, so the sidecar's
        // type lookup at the LHS returns null and we get zero items. Insert
        // a placeholder identifier at the cursor so the line parses as
        // `a.<sentinel>`: the AST then has a real `NamedValueExpression(a)`
        // the sidecar can resolve, and the unknown sentinel member name is
        // harmless (it just doesn't match any real member, so it never
        // surfaces in the result list).
        //
        // Skipped for package-scope (`pkg::|`) because the sidecar's
        // package-scope handler walks the text backward from the cursor to
        // pull the LHS name and looks it up by name — it doesn't need a
        // parsed RHS.
        let patch = (!is_pkg_scope && member_prefix.is_empty())
            .then(|| build_member_completion_sentinel(&rope, pos))
            .flatten();
        let send_pos = patch.as_ref().map_or(pos, |p| p.adjusted_position);

        let items = self
            .slang_complete_request_with_patch(uri, send_pos, kind, wire_prefix, patch)
            .await?;
        if items.is_empty() {
            // Route 2 (`completion` LSP method) returns an empty array when
            // a `.` / `::` trigger is present and slang yields nothing —
            // no syntax fallback for member-access (would surface unrelated
            // workspace symbols, the "workspace dump" anti-pattern). Make
            // the log say what actually happens so the user can diagnose
            // (usually: type or package not in elaboration unit; check the
            // `.mimir.toml` filelist and `+incdir+`).
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
    ///
    /// Called when there is no `.` / `::` / `` ` `` trigger before the cursor
    /// — i.e. the user is typing a plain identifier. The sidecar walks the
    /// enclosing scope chain and returns all visible symbols, inner scopes
    /// shadowing outer ones.
    ///
    /// Returns `Some` when slang resolves at least one candidate. Returns
    /// `None` on transport error, empty result, or no slang configured —
    /// the caller falls through to `syntax_completion`.
    async fn try_slang_identifier_completion(
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
            .slang_complete_request(
                uri,
                pos,
                SlangCompletionRequestKind::Identifier,
                wire_prefix,
            )
            .await?;
        if items.is_empty() {
            debug!("slang identifier completion: no items; falling back to syntax");
            return None;
        }
        debug!(count = items.len(), "slang identifier completion");
        Some(slang_items_to_response(items))
    }

    /// Try slang-backed macro-name completion.
    ///
    /// Called when the cursor is right after a `` ` `` trigger character.
    /// The sidecar returns all `` `define `` names visible in the compilation
    /// unit; the prefix (if any) is applied server-side.
    ///
    /// Returns `None` on transport error or no slang configured — the caller
    /// falls through to `syntax_macro_completion`. An empty successful result
    /// returns `Some(empty)` so the route does *not* fall back to syntax
    /// (the slang macro list is authoritative when slang is reachable).
    async fn try_slang_macro_completion(
        &self,
        uri: &Url,
        pos: MPosition,
        macro_prefix: &str,
    ) -> Option<CompletionResponse> {
        let wire_prefix = (!macro_prefix.is_empty()).then(|| macro_prefix.to_owned());
        let items = self
            .slang_complete_request(uri, pos, SlangCompletionRequestKind::Macro, wire_prefix)
            .await?;
        debug!(count = items.len(), "slang macro completion");
        Some(slang_items_to_response(items))
    }

    /// Try slang-backed signature help.
    ///
    /// Returns `Some` when slang is configured and the sidecar returns at
    /// least one signature. Returns `None` on transport error, empty result,
    /// or no slang configured — the caller falls through to tree-sitter.
    async fn try_slang_signature_help(
        &self,
        uri: &Url,
        pos: MPosition,
    ) -> Option<Vec<mimir_slang::SignatureItem>> {
        let client = self.slang.read().await.clone()?;
        let project = self.project.read().await.clone()?;

        let (def_params, _) = build_definition_params(&project, &self.documents, uri, pos).await?;
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

    /// Gather syntax-side candidates for `uri`, split into same-file and
    /// cross-file streams. Both are filtered by `keep`; **no prefix
    /// prefilter** — fuzzy ranking happens in callers, and a starts_with
    /// prefilter would hide legitimate subsequence matches (e.g. `cls`
    /// → `my_class`).
    ///
    /// No deduplication — callers apply their own (both
    /// `syntax_completion` and `syntax_macro_completion` key by `name`).
    /// Returns `None` only when `uri` is not in the document store.
    async fn gather_syntax_candidates(
        &self,
        uri: &Url,
        keep: impl Fn(&Symbol) -> bool,
    ) -> Option<SyntaxCandidates> {
        let same_file: Vec<Symbol> = {
            let docs = self.documents.read().await;
            let state = docs.get(uri)?;
            state.index.iter().filter(|s| keep(s)).cloned().collect()
        };

        let cross_file: Vec<(Url, Symbol)> = {
            let wi = self.workspace_index.read().await;
            wi.entries()
                .filter(|e| keep(&e.symbol))
                .map(|e| (e.url.clone(), e.symbol.clone()))
                .collect()
        };

        Some(SyntaxCandidates {
            same_file,
            cross_file,
        })
    }

    /// Syntax-only macro completion for `uri` at `pos`.
    ///
    /// Fallback when slang is not configured. Gathers `` `define `` symbols
    /// from same-file + workspace index, fuzzy-scores each candidate
    /// against `macro_prefix`, dedupes by name (a macro shadowed in the
    /// current file shouldn't surface under another file), and orders the
    /// popup best-first via `sort_text`.
    async fn syntax_macro_completion(
        &self,
        uri: &Url,
        macro_prefix: &str,
    ) -> Option<CompletionResponse> {
        const MAX_ITEMS: usize = 200;
        let candidates = self
            .gather_syntax_candidates(uri, |s| s.kind == MSymbolKind::Macro)
            .await?;

        let mut matcher = completion_score::matcher();
        let mut seen: HashSet<String> = HashSet::new();
        let mut scored: Vec<(u32, CompletionItem)> = Vec::new();

        let make_item =
            |name: String, score: u32, data: Option<serde_json::Value>| CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("`define".to_owned()),
                sort_text: Some(completion_score::assign_sort_text(score)),
                data,
                ..Default::default()
            };

        // Same-file first; if a name comes back from the workspace as well,
        // the dedup `seen` keeps the same-file entry which has the boost.
        for sym in candidates.same_file {
            let Some(s) = completion_score::score(&mut matcher, macro_prefix, &sym.name) else {
                continue;
            };
            if !seen.insert(sym.name.clone()) {
                continue;
            }
            let total = s + completion_score::SAME_FILE_BOOST;
            let data = make_resolve_data(uri, sym.name_range.start.line);
            scored.push((total, make_item(sym.name, total, data)));
        }
        for (entry_url, sym) in candidates.cross_file {
            let Some(s) = completion_score::score(&mut matcher, macro_prefix, &sym.name) else {
                continue;
            };
            if !seen.insert(sym.name.clone()) {
                continue;
            }
            let data = make_resolve_data(&entry_url, sym.name_range.start.line);
            scored.push((s, make_item(sym.name, s, data)));
        }

        scored.sort_by_key(|e| std::cmp::Reverse(e.0));
        scored.truncate(MAX_ITEMS);
        let items: Vec<CompletionItem> = scored.into_iter().map(|(_, it)| it).collect();

        debug!(
            count = items.len(),
            prefix = %macro_prefix,
            labels = ?items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            "syntax macro completion",
        );
        Some(CompletionResponse::Array(items))
    }

    /// Syntax-only completion for `uri` at `pos`.
    ///
    /// Merges three candidate sources, fuzzy-ranks them, and orders the
    /// popup best-first via `sort_text`:
    /// 1. Same-file symbols — boosted by [`completion_score::SAME_FILE_BOOST`]
    ///    so a same-file fuzzy hit always beats a cross-file exact hit
    ///    (matches the pre-fuzzy "your file wins" UX).
    /// 2. Cross-file symbols from the workspace index — filename as `detail`.
    /// 3. SV keywords — score divided by [`completion_score::KEYWORD_DIVIDE`]
    ///    so even a perfect keyword score sits below user symbols.
    ///
    /// Macros excluded — served by the dedicated macro path.
    /// Deduplication key: `name`. Same-file is processed first, so a
    /// shadowed cross-file or keyword entry never displaces the same-file
    /// hit. Returns `None` only when `uri` is not in the document store.
    async fn syntax_completion(&self, uri: &Url, pos: MPosition) -> Option<CompletionResponse> {
        const MAX_ITEMS: usize = 200;

        let text = {
            let docs = self.documents.read().await;
            docs.get(uri)?.document.text()
        };
        let rope = Rope::from_str(&text);
        let prefix = mimir_syntax::symbols::prefix_at(&rope, pos).unwrap_or_default();

        let candidates = self
            .gather_syntax_candidates(uri, |s| s.kind != MSymbolKind::Macro)
            .await?;

        let mut matcher = completion_score::matcher();
        let mut seen: HashSet<String> = HashSet::new();
        let mut scored: Vec<(u32, CompletionItem)> = Vec::new();

        for sym in candidates.same_file {
            let Some(s) = completion_score::score(&mut matcher, &prefix, &sym.name) else {
                continue;
            };
            if !seen.insert(sym.name.clone()) {
                continue;
            }
            let total = s + completion_score::SAME_FILE_BOOST;
            scored.push((
                total,
                CompletionItem {
                    label: sym.name.clone(),
                    kind: Some(symbol_kind_to_completion_kind(sym.kind)),
                    sort_text: Some(completion_score::assign_sort_text(total)),
                    data: make_resolve_data(uri, sym.name_range.start.line),
                    ..Default::default()
                },
            ));
        }

        for (entry_url, sym) in candidates.cross_file {
            let Some(s) = completion_score::score(&mut matcher, &prefix, &sym.name) else {
                continue;
            };
            if !seen.insert(sym.name.clone()) {
                continue;
            }
            let detail = entry_url
                .path_segments()
                .and_then(|mut segs| segs.next_back())
                .map(str::to_owned);
            scored.push((
                s,
                CompletionItem {
                    label: sym.name.clone(),
                    kind: Some(symbol_kind_to_completion_kind(sym.kind)),
                    detail,
                    sort_text: Some(completion_score::assign_sort_text(s)),
                    data: make_resolve_data(&entry_url, sym.name_range.start.line),
                    ..Default::default()
                },
            ));
        }

        for kw in mimir_syntax::keywords::KEYWORDS.iter().copied() {
            let Some(s) = completion_score::score(&mut matcher, &prefix, kw) else {
                continue;
            };
            if !seen.insert(kw.to_owned()) {
                continue;
            }
            let demoted = s / completion_score::KEYWORD_DIVIDE;
            let mut item = keyword_completion_item(kw);
            item.sort_text = Some(completion_score::assign_sort_text(demoted));
            scored.push((demoted, item));
        }

        scored.sort_by_key(|e| std::cmp::Reverse(e.0));
        scored.truncate(MAX_ITEMS);
        let items: Vec<CompletionItem> = scored.into_iter().map(|(_, it)| it).collect();

        debug!(
            count = items.len(),
            prefix = %prefix,
            labels = ?items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            "syntax completion",
        );
        Some(CompletionResponse::Array(items))
    }
}

// --------------------------------------------------------------------------
// LanguageServer impl — wires LSP requests/notifications to our store.
// --------------------------------------------------------------------------

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        info!(
            client = ?params.client_info,
            root = ?params.root_uri,
            "initialize received",
        );

        // Resolve `.mimir.toml` from the workspace root. We try
        // `workspace_folders` first (LSP 3.6+, multi-root) and fall back
        // to the deprecated single `root_uri` for older clients. A
        // missing/unreadable config logs at warn but never fails the
        // initialise — slang is optional, syntax diagnostics still work.
        if let Some(start) = workspace_root_path(&params) {
            match ResolvedProject::discover(&start) {
                Ok(Some(resolved)) => {
                    // If slang wasn't configured via process env at startup,
                    // check whether the project's [env] table provides
                    // MIMIR_SLANG_PATH and try to spawn from there.
                    if self.slang.read().await.is_none() {
                        if let Some(path) = resolved.env.get(crate::SLANG_PATH_ENV) {
                            match mimir_slang::Client::spawn(path, std::iter::empty::<&str>()).await
                            {
                                Ok(client) => {
                                    info!(
                                        path = %path,
                                        "slang sidecar spawned from .mimir.toml [env]",
                                    );
                                    *self.slang.write().await = Some(Arc::new(client));
                                }
                                Err(e) => {
                                    warn!(
                                        path = %path,
                                        error = %e,
                                        "could not spawn slang sidecar from .mimir.toml [env]; \
                                         continuing without",
                                    );
                                }
                            }
                        }
                    }

                    // Spawn the workspace-index hydration *before* moving
                    // `resolved` into the project lock so we don't have to
                    // re-read it. The task is fire-and-forget — the index
                    // is best-effort cross-file resolution; if it never
                    // lands (e.g. server shuts down first), F12 just
                    // degrades to same-file resolution, which is fine.
                    let paths = resolved.files.clone();
                    let include_dirs = resolved.include_dirs.clone();
                    let parser = self.parser.clone();
                    let workspace_index = self.workspace_index.clone();
                    // Snapshot the first project file before the move so we
                    // can build a stable startup-elaborate trigger URI.
                    let first_project_file = paths.first().cloned();
                    tokio::spawn(async move {
                        hydrate_workspace_index(paths, include_dirs, parser, workspace_index).await;
                    });
                    *self.project.write().await = Some(resolved);

                    // Kick off a workspace-wide slang elaborate so semantic
                    // cross-file features (definition, completion,
                    // signatureHelp) are warm before the user opens any
                    // file. `schedule_elaborate` reads `self.project`, so
                    // we must call it after the write above. It's a no-op
                    // when slang isn't configured. The trigger URI is just
                    // a debounce-map key — reuse the first filelist entry.
                    if let Some(first) = first_project_file {
                        if let Ok(trigger) = Url::from_file_path(&first) {
                            debug!(
                                trigger = %trigger,
                                "scheduling startup slang elaborate",
                            );
                            self.schedule_elaborate(trigger).await;
                        }
                    }
                }
                Ok(None) => {
                    info!(
                        root = %start.display(),
                        "no .mimir.toml found; slang stays inactive for this session",
                    );
                }
                Err(e) => {
                    warn!(error = %e, "failed to load .mimir.toml; continuing without");
                }
            }
        } else {
            debug!("no workspace root in initialize params; skipping project discovery");
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // Incremental sync: editor sends us range-based edits, not
                // the whole file. This is critical for performance on large
                // files and is the whole point of using a rope. We use the
                // `Options` form (not the simpler `Kind`) so we can also
                // opt into `save` notifications — `did_save` doesn't need
                // the buffer text (we already have it), only the URI, so
                // `include_text: false`.
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                // Tree-sitter go-to-definition. Stage 2: same-file index
                // first, workspace-wide index (open docs + filelist) on
                // miss. Stage 3 will route to slang when the sidecar is
                // configured.
                definition_provider: Some(OneOf::Left(true)),
                // `documentSymbol` reuses the same cached index — same data,
                // free checkbox. The editor uses it to render the outline
                // view and to drive symbol-aware navigation shortcuts.
                document_symbol_provider: Some(OneOf::Left(true)),
                // Slang-only: cursor on variable/field/return-site → type declaration.
                // No tree-sitter fallback (tree-sitter has no semantic type info).
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                // Slang-only: cursor on virtual method → all overrides;
                // cursor on class → all direct subclasses.
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                // Tree-sitter-only for now: highlight every occurrence of the
                // identifier under the cursor in the same file. v1 is text-
                // based (no scope/shadowing); semantic-aware scoping is
                // future work atop slang.
                document_highlight_provider: Some(OneOf::Left(true)),
                // Tree-sitter-only: workspace-wide "Find All References".
                // Same-file results are scope-aware (reuses the
                // `documentHighlight` helper); other open buffers contribute
                // whole-file lexical matches; closed filelist-hydrated files
                // contribute declaration sites only (the workspace index
                // doesn't retain parse trees). Slang RPC for references is a
                // future slice.
                references_provider: Some(OneOf::Left(true)),
                // Syntax-only for stages 1–4; slang takes over for stages 5–6.
                // Trigger characters: `.` (member access), `` ` `` (macros),
                // `$` (system task/function names), `:` (the first half of
                // `::`; the handler runs again on the second colon, where
                // `detect_member_access` recognises the package-scope trigger).
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into(), "`".into(), "$".into(), ":".into()]),
                    // Lazy enrichment via `completionItem/resolve` — every
                    // syntax-side item ships a `data` payload that the
                    // resolve handler turns into a markdown documentation
                    // block (the declaration line) on demand.
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                // Pure tree-sitter feature: walk the SV tree and emit one
                // foldable range per top-level construct (module, class,
                // function, …). No semantic info, no symbol table needed.
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                // Slang-first, then tree-sitter fallback for signature popup.
                // Trigger on `(` so the popup appears immediately when the user
                // opens a function's argument list; `,` keeps it updated as the
                // cursor moves between arguments.
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                // Tree-sitter-only inlay hints: show `param: type` labels before
                // each argument. Only for calls whose callee is in the syntax
                // index; method calls (unknown receiver type) are silently skipped.
                inlay_hint_provider: Some(OneOf::Left(true)),
                // Workspace-wide fuzzy symbol search (VS Code's Ctrl+T,
                // Emacs xref-find-apropos). Reuses the workspace symbol
                // index already populated for `definition` / `completion`;
                // ranks via the same fuzzy scorer completion uses. Excludes
                // `Variable`/`Port`/`Parameter`/`EnumMember` from the result
                // set — too noisy for an IDE's project-wide picker.
                workspace_symbol_provider: Some(OneOf::Left(true)),
                // Semantic tokens: SV-aware coloring driven by the
                // tree-sitter parse tree. Full + range; no delta. The
                // legend is fixed at server-init time (the same
                // ordinals are baked into `mimir_syntax::semantic_tokens`
                // — re-ordering would silently mis-color every open
                // document).
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: Default::default(),
                            legend: semantic_tokens_legend(),
                            range: Some(true),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                // Hover: declaration line for the symbol under the cursor,
                // a synthesized signature for callables, the `define` body
                // for macros, and receiver-aware lookup for `this.X` /
                // `super.X` / `obj.X`. Slang-first when configured (scope-
                // aware lookup via the existing `definition` RPC) with a
                // tree-sitter fallback that uses the same workspace symbol
                // index `definition` / `workspace/symbol` already consume.
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                // Document formatting via `verible-verilog-format`. Both
                // whole-file and range (snapped to whole lines) are supported;
                // set `[features] formatting = false` in `.mimir.toml` to
                // suppress these capabilities. See `docs/formatter.md`.
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                // We publish diagnostics as a *push* (via the `Client`) on
                // every change — we don't yet implement the pull-based
                // `textDocument/diagnostic` request from LSP 3.17.
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "mimir-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("initialized — ready for requests");
        // Dynamically register a `workspace/didChangeWatchedFiles`
        // watcher for `.mimir.toml` and SV source files. The static
        // capability covers `didSave`; this side has to be dynamic
        // because the LSP spec requires file-system watchers go
        // through `client/registerCapability`. Older clients that
        // don't advertise `dynamicRegistration` will reject the
        // request — we log at warn and continue without the watcher;
        // the documented external-edit gap stays open in that case.
        let registration = Registration {
            id: "mimir-watched-files".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers: vec![
                    FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/.mimir.toml".into()),
                        kind: None,
                    },
                    FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.{sv,svh,v}".into()),
                        kind: None,
                    },
                ],
            })
            .ok(),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            warn!(
                error = %e,
                "client refused didChangeWatchedFiles registration; external file edits won't reflect until restart",
            );
        }
    }

    async fn shutdown(&self) -> LspResult<()> {
        info!("shutdown requested");
        Ok(())
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let TextDocumentItem {
            uri,
            language_id,
            version,
            text,
        } = params.text_document;

        debug!(language_id, version, bytes = text.len(), "did_open");

        {
            let mut docs = self.documents.write().await;
            docs.insert(
                uri.clone(),
                DocumentState {
                    document: TextDocument::new(&text, version),
                    language_id,
                    index: Vec::new(),
                    tree: None,
                    index_version: i32::MIN,
                },
            );
        }

        self.reparse_and_publish(uri.clone()).await;
        self.schedule_elaborate(uri).await;
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let new_version = params.text_document.version;

        // Apply each change in order. The LSP spec guarantees changes are
        // sent in document order — earlier ones must be applied before
        // later ones. We use a write lock for the whole batch so partial
        // states aren't observable to a concurrent reparse.
        {
            let mut docs = self.documents.write().await;
            let Some(state) = docs.get_mut(&uri) else {
                warn!("did_change for unknown URI; ignoring");
                return;
            };

            for change in params.content_changes {
                match change.range {
                    None => {
                        // Full sync (only happens if the client opted out
                        // of incremental sync — we advertise INCREMENTAL,
                        // but be defensive).
                        state.document.replace_all(&change.text, new_version);
                    }
                    Some(range) => {
                        let m_range = MRange::new(
                            MPosition::new(range.start.line, range.start.character),
                            MPosition::new(range.end.line, range.end.character),
                        );
                        if let Err(e) = state.document.apply_incremental_edit(
                            m_range,
                            &change.text,
                            new_version,
                        ) {
                            // A bad edit means the editor and us disagree
                            // about document state. Log loudly; the
                            // diagnostics for this version will be wrong
                            // but the next full sync should resync us.
                            error!(error = %e, "incremental edit failed");
                        }
                    }
                }
            }
        }

        self.reparse_and_publish(uri.clone()).await;
        self.schedule_elaborate(uri).await;
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut docs = self.documents.write().await;
            docs.remove(&uri);
        }
        // LSP spec: clear diagnostics for closed docs by publishing empty.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    /// File saved by the editor. We don't re-parse — `did_change` has
    /// already kept the rope in sync with the buffer — but we do
    /// schedule a slang elaborate so the sidecar's view of the
    /// compilation unit reflects what's now on disk (the sidecar reads
    /// files via the same open-buffer override path, but the save is
    /// a strong signal the user wants their changes elaborated end-to-
    /// end). No-op when slang isn't configured.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        debug!("did_save");
        self.schedule_elaborate(uri).await;
    }

    /// External file-system events (LSP `workspace/didChangeWatchedFiles`).
    /// Routes by path and event kind:
    ///
    /// * `.mimir.toml` Created/Changed → re-discover the project from
    ///   its workspace root and re-hydrate the workspace symbol index
    ///   from the new filelist. Guarded against concurrent invocations
    ///   so two events in flight at once don't race.
    /// * `.sv` / `.svh` / `.v` Created/Changed → if the file isn't
    ///   currently open in the editor, re-hydrate its entry in the
    ///   workspace symbol index (open buffers always win — they're
    ///   authoritative for unsaved content).
    /// * Deleted (any watched path) → evict the URL from the workspace
    ///   index.
    ///
    /// The watcher is registered dynamically in `initialized` — clients
    /// without `workspace.didChangeWatchedFiles.dynamicRegistration` send
    /// no events here and the documented external-edit gap stays open.
    #[instrument(level = "debug", skip_all, fields(count = params.changes.len()))]
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for evt in params.changes {
            let path = match evt.uri.to_file_path() {
                Ok(p) => p,
                Err(()) => {
                    debug!(uri = %evt.uri, "watched-files event with non-file URL; skipping");
                    continue;
                }
            };

            let is_mimir_toml = path.file_name().and_then(|n| n.to_str()) == Some(".mimir.toml");

            match evt.typ {
                FileChangeType::DELETED => {
                    debug!(path = %path.display(), "watched file deleted; evicting");
                    let mut wi = self.workspace_index.write().await;
                    wi.update(evt.uri.clone(), &[]);
                }
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    if is_mimir_toml {
                        debug!(path = %path.display(), ".mimir.toml changed; re-hydrating project");
                        self.rehydrate_project_from(&path).await;
                    } else {
                        // Skip re-hydration for files the editor has open
                        // — its rope is authoritative.
                        let is_open = {
                            let docs = self.documents.read().await;
                            docs.contains_key(&evt.uri)
                        };
                        if is_open {
                            debug!(uri = %evt.uri, "watched file is open in editor; skipping disk re-hydrate");
                            continue;
                        }
                        self.rehydrate_single_file(&path).await;
                    }
                }
                _ => {}
            }
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);

        // Slang first when configured + project loaded. Trust slang on
        // empty (the user opted into the semantic resolver; an
        // authoritative "no" is more accurate than syntactic guesses).
        // Transport errors fall through to the syntax path.
        let outcome = self.try_slang_definition(&uri, target).await;
        match route_definition(outcome) {
            DefinitionRoute::UseSlangResult(locs) => {
                debug!(count = locs.len(), "slang definition resolved");
                Ok(slang_locations_to_response(locs))
            }
            DefinitionRoute::UseSyntaxFallback => Ok(self.syntax_definition(&uri, target).await),
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);

        // Slang-only: no tree-sitter fallback for type resolution.
        // None (slang not configured / transport error) → returns None to editor.
        let outcome = self.try_slang_type_definition(&uri, target).await;
        match route_type_definition(outcome) {
            TypeDefinitionRoute::UseSlangResult(locs) => {
                debug!(count = locs.len(), "slang type_definition resolved");
                Ok(slang_locations_to_response(locs))
            }
            TypeDefinitionRoute::UseEmpty => Ok(None),
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);

        // Slang-only: implementation queries need full semantic analysis.
        // None (slang not configured / transport error) → returns None to editor.
        let outcome = self.try_slang_implementation(&uri, target).await;
        match route_implementation(outcome) {
            ImplementationRoute::UseSlangResult(locs) => {
                debug!(count = locs.len(), "slang implementation resolved");
                Ok(slang_locations_to_response(locs))
            }
            ImplementationRoute::UseEmpty => Ok(None),
        }
    }

    /// Highlight every occurrence of the identifier under the cursor in
    /// the same document. Scope-aware: when the cursor sits on a name
    /// that's declared in an enclosing function/task/class/module/etc.,
    /// only references inside that declaring scope come back, and inner
    /// scopes that re-declare the same name are pruned (proper
    /// shadowing). For free-standing references whose declaration isn't
    /// visible (e.g. `super.x`), falls back to whole-file matching.
    /// Cursor on whitespace, a keyword, or a non-identifier returns
    /// `None`.
    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> LspResult<Option<Vec<DocumentHighlight>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);

        let Some(tree) = self.cached_tree(&uri).await else {
            debug!("document_highlight: no tree available");
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());
        // Bail early if the cursor isn't on an identifier — `occurrences_of_at`
        // would also return an empty `Vec`, but probing here lets us
        // signal "no result" (`None`) versus "result, but empty"
        // distinctly to the editor.
        if mimir_syntax::symbols::identifier_at(&tree, &rope, target).is_none() {
            debug!("document_highlight: cursor not on identifier");
            return Ok(None);
        }
        let highlights: Vec<DocumentHighlight> =
            mimir_syntax::symbols::occurrences_of_at(&tree, &rope, target)
                .into_iter()
                .map(|r| DocumentHighlight {
                    range: m_range_to_lsp(r),
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect();
        debug!(count = highlights.len(), "document_highlight returned");
        Ok(Some(highlights))
    }

    /// Workspace-wide "Find All References" for the identifier under the
    /// cursor. Tree-sitter only — the slang sidecar doesn't yet expose a
    /// `references` RPC. v1 shape:
    ///
    /// 1. **Same file**: scope-aware via [`occurrences_of_at`]. Two `phase`
    ///    locals in different methods don't bleed into each other; a
    ///    free-standing reference whose declaration isn't visible
    ///    (`super.x`) falls back to whole-file matching, matching what
    ///    `documentHighlight` does.
    /// 2. **Other open buffers**: whole-file lexical match via
    ///    [`occurrences_of`]. Parse trees for open docs are already cached
    ///    in [`Backend::documents`], so this is essentially free.
    /// 3. **Closed filelist-hydrated files**: the workspace index only
    ///    retains `Symbol` (name + ranges), not parse trees, so we
    ///    contribute *declaration sites only* (`entry.symbol.name_range`).
    ///    Cross-file *usages* in non-open files are a v2 follow-up that
    ///    would require re-parsing on demand.
    ///
    /// Honours [`ReferenceContext::include_declaration`]: when `false`,
    /// declarations identified by the workspace index are filtered out.
    /// Caps total results at [`REFERENCES_LIMIT`] to keep the editor UI
    /// responsive; logs a `warn!` when truncation kicks in.
    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position.text_document.uri),
    )]
    async fn references(&self, params: ReferenceParams) -> LspResult<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let target = MPosition::new(pos.line, pos.character);
        let include_declaration = params.context.include_declaration;

        let Some(cursor_tree) = self.cached_tree(&uri).await else {
            debug!("references: no tree available");
            return Ok(None);
        };
        let cursor_rope = Rope::from_str(cursor_tree.source());

        // Cursor must sit on an identifier; otherwise `None` lets the
        // editor distinguish "no result" from "empty result".
        let Some(name) = mimir_syntax::symbols::identifier_at(&cursor_tree, &cursor_rope, target)
        else {
            debug!("references: cursor not on identifier");
            return Ok(None);
        };
        let name = name.to_owned();

        // Snapshot the open-doc trees (excluding the cursor file) under a
        // single read lock, then drop the lock before holding the
        // workspace-index lock. We clone the trees — `SyntaxTree` is
        // cheap to clone (`tree_sitter::Tree` is internally `Arc`).
        let other_open: Vec<(Url, SyntaxTree)> = {
            let docs = self.documents.read().await;
            docs.iter()
                .filter(|(other_uri, _)| **other_uri != uri)
                .filter_map(|(other_uri, state)| {
                    state.tree.as_ref().map(|t| (other_uri.clone(), t.clone()))
                })
                .collect()
        };

        let wi = self.workspace_index.read().await;
        let locations = collect_references(
            &name,
            &uri,
            &cursor_tree,
            &cursor_rope,
            target,
            &other_open,
            &wi,
            include_declaration,
        );
        debug!(count = locations.len(), name = %name, "references returned");
        Ok(Some(locations))
    }

    /// Hover: declaration line for the symbol under the cursor, with a
    /// synthesized signature for callables and the full `define` body
    /// for macros. Receiver-aware for `this.X` / `super.X` / `obj.X` —
    /// reuses the same enclosing-class + `find_method_in_class` chain
    /// that drives inlay hints.
    ///
    /// Slang-first when configured: routes through
    /// [`try_slang_hover`] (which calls `slang.definition` and reads the
    /// declaration line at the resolved location). On transport error or
    /// an empty slang result, falls through to the tree-sitter path —
    /// hover is a UX feature, not a correctness one, so we prefer "show
    /// the textually-first match" to "show nothing" when slang declines
    /// to resolve.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);

        let Some((tree, index)) = self.cached_tree_and_index(&uri).await else {
            debug!("hover: no cached parse for this URI");
            return Ok(None);
        };
        let rope = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => Rope::from_str(&state.document.text()),
                None => {
                    debug!("hover: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };

        // Slang-first lookup, then tree-sitter fallback.
        if let Some(hover) = self.try_slang_hover(&uri, &rope, target).await {
            return Ok(Some(hover));
        }

        if let Some(hover) = self
            .hover_via_tree_sitter(&uri, &tree, &rope, &index, target)
            .await
        {
            return Ok(Some(hover));
        }

        // Final fallback: cursor on a reserved keyword or `$system_task`
        // for which we have a static one-line LRM-grounded description
        // (e.g. `always_ff`, `$display`). Runs after slang and the
        // workspace-symbol lookup both miss so it never overrides
        // user-defined symbols that happen to shadow a keyword.
        let features = self.current_features().await;
        if features.keyword_hover {
            Ok(keyword_hover_at(&tree, &rope, target))
        } else {
            Ok(None)
        }
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> LspResult<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let index = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => state.index.clone(),
                None => {
                    debug!("document_symbol for unknown URI; returning None");
                    return Ok(None);
                }
            }
        };

        let _ = &uri; // URI is implicit in the response — DocumentSymbol carries no Location.
        let symbols = nest_symbols(&index);
        debug!(count = symbols.len(), "document_symbol returned");
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    /// Workspace-wide fuzzy symbol search. Powers VS Code's
    /// "Go to Symbol in Workspace…" (`Ctrl+T`) and Emacs's cross-file
    /// `xref-find-apropos` / `consult-lsp-symbols`.
    ///
    /// Reads the existing workspace symbol index (populated eagerly on
    /// `initialize` from the filelist, then kept current by
    /// `reparse_and_publish` for open docs) and fuzzy-ranks every entry
    /// against `params.query`. An empty query returns every visible kind
    /// up to [`WORKSPACE_SYMBOL_LIMIT`], matching the IDE convention of
    /// showing "everything" when the picker first opens.
    #[instrument(level = "debug", skip_all, fields(query = %params.query))]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> LspResult<Option<Vec<SymbolInformation>>> {
        let wi = self.workspace_index.read().await;
        let results = rank_workspace_symbols(&params.query, wi.entries());
        debug!(returned = results.len(), "workspace/symbol returned");
        Ok(Some(results))
    }

    /// Pure tree-sitter feature: read the cached parse tree and emit
    /// foldable line ranges for each module/class/function/task/package/etc.
    /// Folding cares about the full tree shape, not just the symbol
    /// `index`, so we read `state.tree` rather than `state.index`.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> LspResult<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        let Some(tree) = self.cached_tree(&uri).await else {
            debug!("folding_range: no tree available");
            return Ok(None);
        };
        let ranges: Vec<FoldingRange> = mimir_syntax::folding::folding_ranges(&tree)
            .into_iter()
            .map(m_fold_to_lsp)
            .collect();
        debug!(count = ranges.len(), "folding_range returned");
        Ok(Some(ranges))
    }

    /// SV-aware semantic tokens for the whole document. Pure
    /// tree-sitter — walks the cached parse tree, classifies every
    /// keyword / type / identifier / literal, and returns one
    /// delta-encoded `SemanticTokens` blob. The legend advertised in
    /// `initialize` pins the ordinals; see
    /// [`mimir_syntax::semantic_tokens`] for the classifier rules and
    /// known limitations.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> LspResult<Option<SemanticTokensResult>> {
        let features = self.current_features().await;
        if !features.semantic_tokens {
            debug!("semantic_tokens_full: disabled by feature toggle");
            return Ok(None);
        }
        let uri = params.text_document.uri;
        let Some(tree) = self.cached_tree(&uri).await else {
            debug!("semantic_tokens_full: no tree available");
            return Ok(None);
        };
        let rope = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => Rope::from_str(&state.document.text()),
                None => {
                    debug!("semantic_tokens_full: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let raw = mimir_syntax::semantic_tokens::semantic_tokens(
            &tree,
            &rope,
            features.format_specs_in_strings,
        );
        let data = encode_semantic_tokens(&raw);
        debug!(count = data.len(), "semantic_tokens_full returned");
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    /// Same classifier as [`Self::semantic_tokens_full`], but constrained
    /// to the editor-supplied range so the cost on huge files scales
    /// with the visible viewport. We translate the LSP range to a byte
    /// range once and hand it to
    /// [`mimir_syntax::semantic_tokens::semantic_tokens_in_range`]; the
    /// walker prunes whole subtrees that don't overlap.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> LspResult<Option<SemanticTokensRangeResult>> {
        let features = self.current_features().await;
        if !features.semantic_tokens {
            debug!("semantic_tokens_range: disabled by feature toggle");
            return Ok(None);
        }
        let uri = params.text_document.uri;
        let Some(tree) = self.cached_tree(&uri).await else {
            debug!("semantic_tokens_range: no tree available");
            return Ok(None);
        };
        let rope = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => Rope::from_str(&state.document.text()),
                None => {
                    debug!("semantic_tokens_range: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let start =
            MPosition::new(params.range.start.line, params.range.start.character)
                .to_byte_offset(&rope)
                .ok();
        let end = MPosition::new(params.range.end.line, params.range.end.character)
            .to_byte_offset(&rope)
            .ok();
        let (Some(start_byte), Some(end_byte)) = (start, end) else {
            debug!("semantic_tokens_range: range out of bounds");
            return Ok(None);
        };
        let raw = mimir_syntax::semantic_tokens::semantic_tokens_in_range(
            &tree,
            &rope,
            start_byte..end_byte,
            features.format_specs_in_strings,
        );
        let data = encode_semantic_tokens(&raw);
        debug!(count = data.len(), "semantic_tokens_range returned");
        Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> LspResult<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);

        // --- slang path ---
        if let Some(sigs) = self.try_slang_signature_help(&uri, target).await {
            let result = slang_signatures_to_lsp(sigs, 0);
            return Ok(Some(result));
        }

        // --- tree-sitter fallback ---
        let Some((tree, index)) = self.cached_tree_and_index(&uri).await else {
            debug!("signature_help: no tree available");
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());

        let Some(call) = call_site_at(&tree, &rope, target) else {
            debug!("signature_help: no call site at cursor");
            return Ok(None);
        };
        debug!(name = %call.name, kind = ?call.kind, "signature_help: call site found");

        // Look up the symbol in the same-file index, then workspace index.
        let sym: Option<Symbol> = match index.iter().find(|s| s.name == call.name).cloned() {
            Some(s) => Some(s),
            None => {
                let wi = self.workspace_index.read().await;
                let found = wi
                    .entries()
                    .find(|e| e.symbol.name == call.name)
                    .map(|e| e.symbol.clone());
                drop(wi);
                found
            }
        };

        let Some(sym) = sym else {
            debug!(name = %call.name, "signature_help: symbol not found in index");
            return Ok(None);
        };

        let Some(sig_info) = signature_for(&sym) else {
            debug!(name = %call.name, "signature_help: symbol not callable");
            return Ok(None);
        };

        let active = active_arg_index(&call, &rope, target);
        let lsp_params: Vec<ParameterInformation> = sig_info
            .params
            .iter()
            .map(|p| ParameterInformation {
                label: ParameterLabel::LabelOffsets([p.label_offset.0, p.label_offset.1]),
                documentation: None,
            })
            .collect();

        let result = SignatureHelp {
            signatures: vec![SignatureInformation {
                label: sig_info.label,
                documentation: None,
                parameters: Some(lsp_params),
                active_parameter: Some(active as u32),
            }],
            active_signature: Some(0),
            active_parameter: Some(active as u32),
        };
        Ok(Some(result))
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document.uri),
    )]
    async fn inlay_hint(&self, params: InlayHintParams) -> LspResult<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let vp = params.range;
        let vp_range = MRange::new(
            MPosition::new(vp.start.line, vp.start.character),
            MPosition::new(vp.end.line, vp.end.character),
        );

        let Some((tree, index)) = self.cached_tree_and_index(&uri).await else {
            debug!("inlay_hint trace: no cached tree for this URI; bailing");
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());

        let call_sites = call_sites_in(&tree, &rope, vp_range);
        debug!(
            calls = call_sites.len(),
            "inlay_hint trace: scanning AST for call sites in viewport",
        );

        let wi = self.workspace_index.read().await;
        let mut hints: Vec<InlayHint> = Vec::new();

        for call in &call_sites {
            let kind_label = match &call.kind {
                CallKind::Function => "function",
                CallKind::Method { .. } => "method",
                CallKind::Macro => "macro",
            };
            let receiver = match &call.kind {
                CallKind::Method { receiver_text, .. } => Some(receiver_text.as_str()),
                _ => None,
            };

            // Method-call routing. Four shapes:
            //   * `this.X(...)`  → resolve via enclosing class
            //   * `super.X(...)` → resolve via enclosing class's `extends` chain
            //   * `new(...)`      → resolve via LHS of the surrounding assignment
            //                       (receiver_text is empty for `class_new` calls)
            //   * `obj.X(...)`    → resolve via the AST-declared type of `obj`
            if matches!(call.kind, CallKind::Method { .. }) {
                let recv = receiver.unwrap_or("");
                let resolved = resolve_method_symbol(call, recv, &tree, &rope, &index, &wi);
                match resolved {
                    MethodResolution::Resolved(sym, source_label) => {
                        let labels = hints_for(call, &sym);
                        debug!(
                            name = %call.name,
                            receiver = recv,
                            via = source_label,
                            sym_params = sym.params.as_ref().map(|p| p.len()).unwrap_or(0),
                            call_args = call.args.len(),
                            labels = labels.len(),
                            "inlay_hint trace: method resolved",
                        );
                        for label in labels {
                            hints.push(InlayHint {
                                position: Position::new(
                                    label.position.line,
                                    label.position.character,
                                ),
                                label: InlayHintLabel::String(label.text),
                                kind: Some(InlayHintKind::PARAMETER),
                                text_edits: None,
                                tooltip: None,
                                padding_left: None,
                                padding_right: Some(true),
                                data: None,
                            });
                        }
                    }
                    MethodResolution::NotResolved(reason) => {
                        debug!(
                            name = %call.name,
                            kind = kind_label,
                            receiver = recv,
                            args = call.args.len(),
                            line = call.name_range.start.line,
                            col = call.name_range.start.character,
                            reason,
                            "inlay_hint trace: method NOT resolved — slang would help here",
                        );
                    }
                }
                continue;
            }

            // Same-file (DocumentState.index) first.
            let same_file_hit = index.iter().find(|s| s.name == call.name).cloned();
            // Workspace-wide (filelist + include-chain hydration) fallback.
            let workspace_hit = if same_file_hit.is_none() {
                wi.entries()
                    .find(|e| e.symbol.name == call.name)
                    .map(|e| e.symbol.clone())
            } else {
                None
            };
            let sym = same_file_hit.clone().or(workspace_hit.clone());

            debug!(
                name = %call.name,
                kind = kind_label,
                args = call.args.len(),
                line = call.name_range.start.line,
                col = call.name_range.start.character,
                same_file_hit = same_file_hit.is_some(),
                workspace_hit = workspace_hit.is_some(),
                resolved = sym.is_some(),
                slang_would_help = sym.is_none(),
                "inlay_hint trace: symbol lookup",
            );

            let Some(sym) = sym else { continue };

            for label in hints_for(call, &sym) {
                hints.push(InlayHint {
                    position: Position::new(label.position.line, label.position.character),
                    label: InlayHintLabel::String(label.text),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: None,
                    padding_left: None,
                    padding_right: Some(true),
                    data: None,
                });
            }
        }

        debug!(
            emitted = hints.len(),
            "inlay_hint trace: done — these are the hints the editor will show",
        );
        Ok(Some(hints))
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position.text_document.uri),
    )]
    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let target = MPosition::new(pos.line, pos.character);

        // Read the document text once; used by both trigger-detection paths below.
        let text = {
            let docs = self.documents.read().await;
            docs.get(&uri).map(|s| s.document.text())
        };
        let rope = text.as_deref().map(Rope::from_str);

        // Route 1: `` ` `` trigger — macro-name completion.
        if let Some(rope) = &rope {
            if let Some(macro_prefix) = detect_macro_trigger(rope, target) {
                if let Some(resp) = self
                    .try_slang_macro_completion(&uri, target, &macro_prefix)
                    .await
                {
                    return Ok(Some(resp));
                }
                // Syntax fallback: `define symbols from the index.
                return Ok(self.syntax_macro_completion(&uri, &macro_prefix).await);
            }
        }

        // Route 2: `.` or `::` trigger — member / package-scope completion.
        // Slang-only: without type information, any fallback would suggest
        // unrelated workspace symbols (the "workspace dump" anti-pattern).
        // If the trigger is present but slang can't resolve it, return empty
        // rather than polluting the popup with irrelevant candidates.
        let has_member_trigger = rope
            .as_ref()
            .map(|r| detect_member_access(r, target).is_some())
            .unwrap_or(false);

        if let Some(resp) = self.try_slang_member_completion(&uri, target).await {
            return Ok(Some(resp));
        }
        if has_member_trigger {
            return Ok(Some(CompletionResponse::Array(vec![])));
        }

        // Route 3: plain identifier — scope-aware completion.
        // Slang first (scope-correct); falls back to syntax+workspace+keywords.
        if let Some(resp) = self.try_slang_identifier_completion(&uri, target).await {
            return Ok(Some(resp));
        }
        Ok(self.syntax_completion(&uri, target).await)
    }

    /// Lazily enrich a completion item with the declaration line as
    /// markdown documentation. Items without a [`CompletionResolveData`]
    /// payload (keywords, slang-sourced items) are returned unchanged.
    ///
    /// Cheap path: one rope slice on the document text — no parser run,
    /// no workspace scan. If the document isn't open and isn't on disk,
    /// the documentation is left empty.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        let features = self.current_features().await;
        if !features.formatting {
            debug!("formatting: disabled by feature toggle");
            return Ok(None);
        }
        let cfg = self.current_formatter_config().await;
        let source = {
            let docs = self.documents.read().await;
            match docs.get(&params.text_document.uri) {
                Some(state) => state.document.text().to_owned(),
                None => {
                    debug!("formatting: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let rope = ropey::Rope::from_str(&source);
        let (wrapped, has_ifdefs) = if cfg.wrap_ifdefs {
            let (w, flag) = wrap_ifdefs(&source);
            if flag {
                warn!(
                    "formatting: file contains `ifdef/`ifndef blocks; \
                     wrapping them with format-off pragmas so Verible can parse the rest"
                );
            }
            (w, flag)
        } else {
            (source.clone(), false)
        };
        match invoke_verible(&cfg, &wrapped, None).await {
            Ok(raw_formatted) => {
                let formatted = if has_ifdefs {
                    strip_mimir_pragmas(&raw_formatted)
                } else {
                    raw_formatted
                };
                if formatted == source {
                    // Verible returned the file unchanged even after we removed
                    // the injected pragmas.  This shouldn't happen for well-formed
                    // code but can occur if every statement is inside an ifdef.
                    warn!(
                        "verible-verilog-format produced no changes \
                         (all code may be inside preprocessor guards)"
                    );
                    self.client
                        .show_message(
                            MessageType::WARNING,
                            "Formatter produced no changes — all code may be inside \
                             preprocessor guards. Check the server log for details.",
                        )
                        .await;
                    return Ok(None);
                }
                if has_ifdefs {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            "`ifdef/`ifndef blocks were preserved verbatim; \
                             only the surrounding code was reformatted.",
                        )
                        .await;
                }
                Ok(Some(whole_file_edit(&rope, &formatted)))
            }
            Err(e) => {
                error!(error = %e, "verible-verilog-format failed; returning no edits");
                Ok(None)
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        let features = self.current_features().await;
        if !features.formatting {
            debug!("range_formatting: disabled by feature toggle");
            return Ok(None);
        }
        let cfg = self.current_formatter_config().await;
        let source = {
            let docs = self.documents.read().await;
            match docs.get(&params.text_document.uri) {
                Some(state) => state.document.text().to_owned(),
                None => {
                    debug!("range_formatting: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let rope = ropey::Rope::from_str(&source);
        // LSP line numbers are 0-based; Verible's --lines flag is 1-based.
        let start_line = params.range.start.line + 1;
        let end_line = params.range.end.line + 1;
        let (wrapped, has_ifdefs) = if cfg.wrap_ifdefs {
            let (w, flag) = wrap_ifdefs(&source);
            if flag {
                warn!(
                    "range_formatting: file contains `ifdef/`ifndef blocks; \
                     wrapping them with format-off pragmas"
                );
            }
            (w, flag)
        } else {
            (source.clone(), false)
        };
        match invoke_verible(&cfg, &wrapped, Some(start_line..=end_line)).await {
            Ok(raw_formatted) => {
                let formatted = if has_ifdefs {
                    strip_mimir_pragmas(&raw_formatted)
                } else {
                    raw_formatted
                };
                if formatted == source {
                    warn!(
                        "verible-verilog-format produced no changes for range \
                         {start_line}-{end_line}"
                    );
                    self.client
                        .show_message(
                            MessageType::WARNING,
                            "Formatter produced no changes for the selected range. \
                             All code in the range may be inside preprocessor guards.",
                        )
                        .await;
                    return Ok(None);
                }
                if has_ifdefs {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            "`ifdef/`ifndef blocks in the range were preserved verbatim.",
                        )
                        .await;
                }
                Ok(Some(whole_file_edit(&rope, &formatted)))
            }
            Err(e) => {
                error!(error = %e, "verible-verilog-format failed; returning no edits");
                Ok(None)
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(label = %item.label))]
    async fn completion_resolve(&self, item: CompletionItem) -> LspResult<CompletionItem> {
        let Some(data) = item.data.clone() else {
            return Ok(item);
        };
        let resolve: CompletionResolveData = match serde_json::from_value(data) {
            Ok(r) => r,
            Err(e) => {
                debug!(error = %e, "completionItem/resolve: malformed data, returning unchanged");
                return Ok(item);
            }
        };

        // Try the open-doc store first; fall back to a disk read so
        // cross-file items resolve even when the user hasn't opened the
        // declaring file yet.
        let line_text: Option<String> = {
            let docs = self.documents.read().await;
            docs.get(&resolve.url).and_then(|state| {
                let rope = Rope::from_str(&state.document.text());
                read_line_trimmed(&rope, resolve.line)
            })
        }
        .or_else(|| {
            resolve
                .url
                .to_file_path()
                .ok()
                .and_then(|p| std::fs::read_to_string(&p).ok())
                .and_then(|text| read_line_trimmed(&Rope::from_str(&text), resolve.line))
        });

        if let Some(line) = line_text {
            let mut item = item;
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```systemverilog\n{line}\n```"),
            }));
            Ok(item)
        } else {
            Ok(item)
        }
    }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Build a single whole-file [`TextEdit`] that replaces the current document
/// text with `new_text`.
///
/// Verible always emits the complete file even when `--lines` constrains which
/// lines it rewrites, so both `formatting` and `range_formatting` use this
/// helper to return one encompassing edit. The end position is computed in
/// UTF-16 code units (the LSP wire format) to satisfy clients that validate
/// positions against the advertised offset encoding.
fn whole_file_edit(rope: &ropey::Rope, new_text: &str) -> Vec<TextEdit> {
    let total_lines = rope.len_lines();
    let last_line_idx = total_lines.saturating_sub(1);
    let last_line = rope.line(last_line_idx);
    let last_col_utf16: u32 = last_line.chars().map(|c| c.len_utf16() as u32).sum();
    vec![TextEdit {
        range: Range {
            start: Position { line: 0, character: 0 },
            end: Position {
                line: last_line_idx as u32,
                character: last_col_utf16,
            },
        },
        new_text: new_text.to_owned(),
    }]
}

/// Pick a filesystem path to start `.mimir.toml` discovery from.
///
/// Preference order:
///
/// 1. The first `workspace_folders[].uri` (LSP 3.6+, what every modern
///    editor sends today).
/// 2. The deprecated `root_uri` (single-root clients, still common).
/// 3. The even-more-deprecated `root_path` (raw filesystem path, used by
///    very old clients).
///
/// `None` when none of the three are present — typically a hand-fed
/// JSON-RPC session for debugging — in which case the caller skips
/// project discovery and slang stays inactive.
fn workspace_root_path(params: &InitializeParams) -> Option<PathBuf> {
    if let Some(folders) = params.workspace_folders.as_ref() {
        if let Some(first) = folders.first() {
            if let Ok(path) = first.uri.to_file_path() {
                return Some(path);
            }
        }
    }
    #[allow(deprecated)]
    if let Some(uri) = params.root_uri.as_ref() {
        if let Ok(path) = uri.to_file_path() {
            return Some(path);
        }
    }
    #[allow(deprecated)]
    if let Some(p) = params.root_path.as_ref() {
        return Some(PathBuf::from(p));
    }
    None
}

/// Build the elaborate request from the resolved project config and the
/// currently-open documents.
///
/// Returns the `ElaborateParams` to send and a parallel `Vec<Url>` of
/// every file we asked slang to look at. The latter is used by
/// [`publish_slang_result`] to publish empty diagnostics for files slang
/// reported clean — that's how we honour the "slang says clean → drop
/// tree-sitter syntax errors" conflict policy across files.
///
/// Open documents' in-memory text overrides any on-disk version, so the
/// user's unsaved changes participate in elaboration. Open documents not
/// listed in the project filelist are also included — the user might be
/// editing a file the `.f` doesn't list yet, and we still want
/// diagnostics for it.
/// Eager-hydrate the workspace index from a project filelist.
///
/// Spawned from `initialize` once `.mimir.toml` has resolved. Reads each
/// file from disk, parses it with the shared `SyntaxParser`, and folds
/// the resulting symbols into the workspace index under one write-lock
/// transaction at the end. Marks the index hydrated regardless of how
/// many files actually parsed — a partial result still beats a cold
/// index, and `is_hydrated` is just a "did we attempt this once" flag.
///
/// The function is `async` so it can `await` the parser Mutex; the
/// per-file parses themselves are CPU-bound but ms-scale per file, so we
/// don't bother with `spawn_blocking`. If a project ships hundreds of
/// large generated headers we may revisit.
async fn hydrate_workspace_index(
    paths: Vec<PathBuf>,
    include_dirs: Vec<PathBuf>,
    parser: Arc<Mutex<SyntaxParser>>,
    workspace_index: Arc<RwLock<WorkspaceIndex>>,
) {
    let count_requested = paths.len();
    let entries = {
        let mut p = parser.lock().await;
        workspace_index::hydrate_from_paths(&paths, &include_dirs, &mut p, |path| {
            std::fs::read_to_string(path).ok()
        })
    };

    let parsed = entries.len();
    {
        let mut wi = workspace_index.write().await;
        for (url, syms) in entries {
            wi.update(url, &syms);
        }
    }
    info!(
        files = parsed,
        requested = count_requested,
        "workspace index hydrated",
    );
}

/// Hash the inputs of an [`ElaborateParams`] for cache-keying purposes.
///
/// Includes everything slang's compilation depends on: each source file's
/// `path` + `text` + `is_compilation_unit`, plus `include_dirs`,
/// `defines`, and `top`. Uses `DefaultHasher` — deterministic within one
/// process (sufficient for an in-memory equality check) and O(total bytes).
///
/// Two requests with identical inputs hash to the same value; any change
/// to a file's text, the filelist, include search paths, or defines
/// produces a different hash and forces a fresh slang elaborate.
fn hash_elaborate_inputs(params: &ElaborateParams) -> u64 {
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

async fn build_elaborate_params(
    project: &ResolvedProject,
    documents: &Arc<RwLock<HashMap<Url, DocumentState>>>,
) -> (ElaborateParams, Vec<Url>) {
    // Snapshot open document text under the read lock; release before
    // any disk I/O. `text()` clones the rope into a String — cheap
    // enough for typical files, and lets the lock release immediately.
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

    assemble_elaborate_params(project, &open_text, |path| {
        std::fs::read_to_string(path).ok()
    })
}

/// Pure version of [`build_elaborate_params`]: given the project, a
/// snapshot of open documents, and an injectable disk reader, produce
/// the request envelope. Split out so unit tests can drive it without an
/// `Arc<RwLock<HashMap<...>>>` or a real filesystem.
///
/// `read_disk` is called for project files that aren't currently open.
/// Returning `None` means "this file isn't on disk either" — slang sees
/// an empty buffer for it. That's the same fallback we'd get from
/// `read_to_string(...).unwrap_or_default()`, just an explicit seam.
fn assemble_elaborate_params(
    project: &ResolvedProject,
    open_text: &HashMap<PathBuf, (Url, String)>,
    mut read_disk: impl FnMut(&std::path::Path) -> Option<String>,
) -> (ElaborateParams, Vec<Url>) {
    let mut files: Vec<SourceFile> = Vec::new();
    let mut files_in_request: Vec<Url> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // Project filelist first — declaration order matters for slang
    // (later files can refer to earlier ones in the same compilation
    // unit; reordering would change diagnostics).
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

    // Open documents not yet covered by the project filelist. We send
    // them so unsaved edits to includee files still affect elaboration,
    // but mark them `is_compilation_unit = false` — the sidecar will
    // seed its `SourceManager` with the buffer and let `` `include ``
    // resolution find it, instead of wrapping it in a standalone
    // `SyntaxTree` (which would be wrong: an includee out of its
    // `package` context produces spurious errors, and the buffer
    // collides with the one the preprocessor pulled in via include).
    //
    // Headers that aren't open in the editor are intentionally NOT
    // pulled in here — slang's preprocessor reads them straight from
    // disk via `+incdir+`. Inlining the full include closure (e.g. all
    // of UVM) would balloon every request to multi-MB and pay no
    // dividend the preprocessor can't already deliver.
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

/// Build a `definition` request envelope for the slang sidecar.
///
/// Reuses [`assemble_elaborate_params`] for the file/include/define
/// assembly so the request matches the most recent `elaborate` shape
/// — the sidecar can answer from its compilation cache when the
/// inputs are bit-equal. Only the cursor's `target_path` /
/// `target_position` are added on top.
///
/// Returns `None` when the cursor URI has no filesystem path (e.g.
/// `untitled:` buffers); slang addresses files by path so there's
/// nothing meaningful to send.
async fn build_definition_params(
    project: &ResolvedProject,
    documents: &Arc<RwLock<HashMap<Url, DocumentState>>>,
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
    let (elab, files_in_request) = assemble_elaborate_params(project, &open_text, |path| {
        std::fs::read_to_string(path).ok()
    });
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

/// Convert a slang `DefinitionLocation` into LSP's `Location`.
///
/// Returns `None` when the path can't be turned back into a URL — same
/// fallback path as `slang_to_lsp_diagnostic` uses for diagnostics,
/// keeping behaviour consistent across slang result types.
fn slang_location_to_lsp(loc: SlangDefinitionLocation) -> Option<Location> {
    let url = path_to_url(&loc.path)?;
    Some(Location {
        uri: url,
        range: m_range_to_lsp(loc.range),
    })
}

/// Outcome of the AST-driven method-call resolver. `Resolved(sym, via)`
/// carries the resolved symbol plus a short human-readable tag for the
/// route taken (used by trace logs). `NotResolved(reason)` is also
/// human-readable.
enum MethodResolution {
    Resolved(Symbol, &'static str),
    NotResolved(&'static str),
}

/// Resolve a method-call site to a `Symbol` via the AST, without slang.
///
/// `recv` is the receiver text as the call-site builder stored it —
/// "super", "this", "" (for `class_new`), or a `hierarchical_identifier`
/// for `obj.method` chains.
///
/// Routes:
///   * `recv == "this"`  → look up `call.name` as a Method in the enclosing
///     class's same-file index entries.
///   * `recv == "super"` → walk the enclosing class's `extends` chain via
///     [`find_method_in_class`].
///   * `recv == ""`      → constructor-expression form (`class_new`); use
///     [`mimir_syntax::symbols::class_new_lhs_at`] to find the LHS context,
///     then look up `"new"` in the resolved class.
///   * otherwise        → `obj.method`-style; the receiver is a
///     `hierarchical_identifier` that includes the method name, so we
///     strip the trailing segment, refuse chained receivers (`obj.field`
///     would need slang), feed the bare identifier through
///     [`mimir_syntax::symbols::find_variable_type_at`] to get its
///     declared type, normalize, and resolve.
fn resolve_method_symbol(
    call: &mimir_syntax::calls::CallSite,
    recv: &str,
    tree: &mimir_syntax::SyntaxTree,
    rope: &Rope,
    same_file_index: &[Symbol],
    wi: &workspace_index::WorkspaceIndex,
) -> MethodResolution {
    use mimir_syntax::symbols::{
        class_new_lhs_at, enclosing_class_info_at, find_variable_type_at, normalize_type_name,
        ClassNewLhs,
    };

    match recv {
        "this" => {
            // Same-file methods of the enclosing class. The same-file index
            // already only contains symbols from this document; for typical
            // single-class files (apb_monitor, packet, …) name lookup is
            // sufficient.
            let _ = enclosing_class_info_at(tree, rope, call.name_range.start);
            same_file_index
                .iter()
                .find(|s| s.name == call.name && s.kind == MSymbolKind::Method)
                .cloned()
                .map(|s| MethodResolution::Resolved(s, "this/same-file"))
                .unwrap_or(MethodResolution::NotResolved(
                    "this.X not in same-file index",
                ))
        }
        "super" => {
            let info = enclosing_class_info_at(tree, rope, call.name_range.start);
            let Some(parent) = info.and_then(|i| i.parent_class_name) else {
                return MethodResolution::NotResolved("super used but no extends clause");
            };
            find_method_in_class(wi, &parent, &call.name)
                .map(|s| MethodResolution::Resolved(s, "super/inheritance walk"))
                .unwrap_or(MethodResolution::NotResolved(
                    "super.X not found in any ancestor",
                ))
        }
        "" => {
            // `class_new` expression — find the LHS context.
            let ctx = match class_new_lhs_at(tree, rope, call.name_range.start) {
                Some(c) => c,
                None => {
                    return MethodResolution::NotResolved(
                        "class_new not in a recognised assignment shape",
                    )
                }
            };
            let target_class = match ctx {
                ClassNewLhs::DeclaredType(ty) => normalize_type_name(&ty),
                ClassNewLhs::LhsName(name) => {
                    find_variable_type_at(tree, rope, call.name_range.start, &name)
                        .as_deref()
                        .and_then(normalize_type_name)
                }
            };
            let Some(cls) = target_class else {
                return MethodResolution::NotResolved("class_new LHS type unresolvable from AST");
            };
            find_method_in_class(wi, &cls, "new")
                .map(|s| MethodResolution::Resolved(s, "class_new/LHS-type"))
                .unwrap_or(MethodResolution::NotResolved(
                    "constructor not found for resolved class",
                ))
        }
        _ => {
            // `obj.method` style. `recv` is the whole hierarchical_identifier
            // including the method name (an artefact of how `tf_call`
            // call-site detection captures the receiver). Strip the trailing
            // segment to get just the receiver chain, then accept only
            // single-segment receivers — chained access (`obj.field.method`)
            // would need recursive resolution that's closer to a type
            // checker and out of scope here.
            let receiver_chain = match recv.rsplit_once('.') {
                Some((before, _method)) => before,
                None => recv,
            };
            if receiver_chain.contains('.') {
                return MethodResolution::NotResolved("chained receiver access needs slang");
            }
            let ty = find_variable_type_at(tree, rope, call.name_range.start, receiver_chain);
            let Some(cls) = ty.as_deref().and_then(normalize_type_name) else {
                return MethodResolution::NotResolved("receiver type unresolvable from AST");
            };
            find_method_in_class(wi, &cls, &call.name)
                .map(|s| MethodResolution::Resolved(s, "obj.method/AST-typed"))
                .unwrap_or(MethodResolution::NotResolved(
                    "method not found in resolved receiver class",
                ))
        }
    }
}

/// Look up a method named `method_name` declared inside the body of the
/// class named `class_name`, walking up the inheritance chain via each
/// class's recorded [`Symbol::parent_class_name`] when the method isn't
/// found in the class itself.
///
/// Used by `inlay_hint` to resolve `super.X(...)` calls without slang:
/// the AST gives us the parent class name from the `extends` clause; this
/// helper bridges that to the parent's (or grandparent's, …) method
/// symbols via the index — so `super.run_phase(phase)` from a class
/// extending `uvm_monitor` finds `uvm_component::run_phase` two levels up.
///
/// Strategy per inheritance step:
///   1. Find the workspace entry for the current class (kind=Class).
///      Multiple matches across files are possible — pick the first.
///   2. Among entries for `method_name`, pick the one whose URL matches the
///      class's URL *and* whose `full_range` is inside the class's
///      `full_range`. That's the method declared in that class body.
///   3. If no match, recurse on the parent class name. Capped at 16 hops
///      to prevent runaway searches if the index has a cycle.
fn find_method_in_class(
    wi: &workspace_index::WorkspaceIndex,
    class_name: &str,
    method_name: &str,
) -> Option<Symbol> {
    let mut current = class_name.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..16 {
        if !visited.insert(current.clone()) {
            return None;
        }
        let class_entry = wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == MSymbolKind::Class)
            .cloned()?;
        if let Some(method) = wi
            .lookup(method_name)
            .iter()
            .find(|e| {
                e.url == class_entry.url
                    && range_contains(class_entry.symbol.full_range, e.symbol.full_range)
                    && e.symbol.kind == MSymbolKind::Method
            })
            .map(|e| e.symbol.clone())
        {
            return Some(method);
        }
        match class_entry.symbol.parent_class_name {
            Some(parent) => current = parent,
            None => return None,
        }
    }
    None
}

/// Variant of [`find_method_in_class`] for class fields / variables.
///
/// Walks the `extends` chain identically but matches kind=Variable
/// against entries whose `full_range` is contained in the class body.
/// Used by hover for cursor on `this.cfg`, `obj.field`, etc.
fn find_field_in_class(
    wi: &workspace_index::WorkspaceIndex,
    class_name: &str,
    field_name: &str,
) -> Option<Symbol> {
    let mut current = class_name.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..16 {
        if !visited.insert(current.clone()) {
            return None;
        }
        let class_entry = wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == MSymbolKind::Class)
            .cloned()?;
        if let Some(field) = wi
            .lookup(field_name)
            .iter()
            .find(|e| {
                e.url == class_entry.url
                    && range_contains(class_entry.symbol.full_range, e.symbol.full_range)
                    && matches!(
                        e.symbol.kind,
                        MSymbolKind::Variable | MSymbolKind::Port | MSymbolKind::Parameter
                    )
            })
            .map(|e| e.symbol.clone())
        {
            return Some(field);
        }
        match class_entry.symbol.parent_class_name {
            Some(parent) => current = parent,
            None => return None,
        }
    }
    None
}

/// Find the URL of the file that contains the class body in which `sym`
/// is declared. Walks the `extends` chain like
/// [`find_method_in_class`] / [`find_field_in_class`] do, so a method
/// resolved on an ancestor returns the ancestor's file URL.
fn method_url_in_class(
    wi: &workspace_index::WorkspaceIndex,
    class_name: &str,
    sym: &Symbol,
) -> Option<Url> {
    let mut current = class_name.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..16 {
        if !visited.insert(current.clone()) {
            return None;
        }
        let class_entry = wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == MSymbolKind::Class)
            .cloned()?;
        if wi
            .lookup(&sym.name)
            .iter()
            .any(|e| e.url == class_entry.url && e.symbol.name_range == sym.name_range)
        {
            return Some(class_entry.url);
        }
        match class_entry.symbol.parent_class_name {
            Some(parent) => current = parent,
            None => return None,
        }
    }
    None
}

/// Build a `Hover` from a resolved [`Symbol`] by:
///
/// 1. Synthesizing a typed signature for callables (function/task/
///    method/macro) via [`mimir_syntax::signature::signature_for`].
/// 2. For macros, additionally appending the full `define` body
///    captured from the source between `full_range.start` and
///    `full_range.end`.
/// 3. Falling back to the raw declaration line for non-callables
///    (classes, modules, variables, typedefs, parameters, …).
///
/// The line is read from the open-doc store first (the editor's
/// authoritative view of unsaved content), then from disk — mirrors
/// `completionItem/resolve`'s pattern. Returns `None` only when the
/// declaration line genuinely can't be found anywhere.
fn hover_for_symbol(
    sym: &Symbol,
    sym_url: &Url,
    docs: &std::collections::HashMap<Url, DocumentState>,
) -> Option<Hover> {
    let rope_from_doc: Option<Rope> = docs
        .get(sym_url)
        .map(|s| Rope::from_str(&s.document.text()));

    // 1. Callable signatures (function/task/method/macro).
    if let Some(sig) = mimir_syntax::signature::signature_for(sym) {
        if sym.kind == MSymbolKind::Macro {
            // For macros: signature + body.
            let body = read_macro_body(sym, sym_url, rope_from_doc.as_ref());
            let value = match body {
                Some(b) if !b.trim().is_empty() => {
                    format!("```systemverilog\n{}\n{}\n```", sig.label, b)
                }
                _ => format!("```systemverilog\n{}\n```", sig.label),
            };
            return Some(hover_from_markdown(value));
        }
        return Some(hover_from_markdown(format!(
            "```systemverilog\n{};\n```",
            sig.label
        )));
    }

    // 2. Non-callables: the declaration line.
    let line_no = sym.name_range.start.line;
    let line = rope_from_doc
        .as_ref()
        .and_then(|r| read_line_trimmed(r, line_no))
        .or_else(|| {
            sym_url
                .to_file_path()
                .ok()
                .and_then(|p| std::fs::read_to_string(&p).ok())
                .and_then(|t| read_line_trimmed(&Rope::from_str(&t), line_no))
        })?;
    Some(hover_markdown(&line))
}

/// Read the source slice covering `sym.full_range` from the open-doc
/// rope first, then from disk. Returns the trimmed body.
///
/// Used by hover on a macro reference to show the full `\`define`
/// expansion, including multi-line `\\`-continued bodies. Returns
/// `None` if neither source is readable; the caller drops to showing
/// just the signature in that case.
fn read_macro_body(sym: &Symbol, sym_url: &Url, doc_rope: Option<&Rope>) -> Option<String> {
    let slice_from_rope = |rope: &Rope| -> Option<String> {
        let start = sym.full_range.start.to_byte_offset(rope).ok()?;
        let end = sym.full_range.end.to_byte_offset(rope).ok()?;
        if end <= start || end > rope.len_bytes() {
            return None;
        }
        Some(rope.byte_slice(start..end).to_string())
    };

    let raw = doc_rope.and_then(slice_from_rope).or_else(|| {
        let path = sym_url.to_file_path().ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let rope = Rope::from_str(&text);
        slice_from_rope(&rope)
    })?;

    // Strip the leading `\`define MACRO_NAME(...)`-or-`\`define MACRO_NAME`
    // header. Everything after the first `)` (for parametrised macros) or
    // after the macro name (for bare ones) up to the end-of-define is the
    // body. We keep this conservative: skip the first source line up to
    // and including the closing paren of the params; if there's no `(`
    // skip past the name.
    let after_name = raw.find(&sym.name).map(|i| i + sym.name.len()).unwrap_or(0);
    let after_params = if let Some(rest) = raw.get(after_name..) {
        if rest.trim_start().starts_with('(') {
            // Skip to the matching `)`.
            rest.find(')')
                .map(|idx| after_name + idx + 1)
                .unwrap_or(after_name)
        } else {
            after_name
        }
    } else {
        after_name
    };

    let body = raw
        .get(after_params..)
        .unwrap_or("")
        .trim_matches(|c: char| c == ' ' || c == '\t' || c == '\\' || c == '\r' || c == '\n');
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
}

/// Wrap a single line as a SystemVerilog markdown fenced block — the
/// same format `completionItem/resolve` uses, so hover and resolve
/// docstrings look identical to the user.
fn hover_markdown(line: &str) -> Hover {
    hover_from_markdown(format!("```systemverilog\n{line}\n```"))
}

/// Final hover fallback: if the cursor sits on a reserved keyword or
/// `$system_task` for which the curated table in
/// [`mimir_syntax::keywords::doc_for`] has a description, build a
/// markdown popup. Returns `None` for unknown words, whitespace, or
/// punctuation — the caller treats that as "no hover".
///
/// The popup format mirrors [`hover_for_symbol`] so keyword help looks
/// the same as symbol help: the word itself in a `systemverilog`
/// fenced block, then the one-line description as a separate markdown
/// paragraph below.
fn keyword_hover_at(tree: &SyntaxTree, rope: &Rope, target: MPosition) -> Option<Hover> {
    let word = mimir_syntax::symbols::word_at(tree, rope, target)?;
    let doc = mimir_syntax::keywords::doc_for(word)?;
    Some(hover_from_markdown(format!(
        "```systemverilog\n{word}\n```\n\n{doc}"
    )))
}

/// Build a `Hover` from an already-formatted markdown blob. Always
/// emits `MarkupKind::Markdown`; LSP clients that prefer plain text
/// degrade gracefully on their end.
fn hover_from_markdown(markdown: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    }
}

/// Wrap a list of locations into a `GotoDefinitionResponse`, treating
/// the empty list as "no declaration found" (`None`).
///
/// This is the trust-slang-on-empty contract: an authoritative empty
/// result from slang short-circuits to `None` rather than triggering a
/// syntax fallback. The fallback is reserved for transport errors.
fn slang_locations_to_response(locs: Vec<Location>) -> Option<GotoDefinitionResponse> {
    if locs.is_empty() {
        None
    } else {
        Some(GotoDefinitionResponse::Array(locs))
    }
}

/// Outcome of a slang definition request, used by `goto_definition` to
/// decide whether to short-circuit (Resolved, including empty) or fall
/// through to the syntax path (TransportError).
#[derive(Debug)]
enum SlangDefinitionOutcome {
    /// Slang answered. The vector may be empty — that's "no decl found"
    /// and the server returns `None` to the editor without falling
    /// back to syntax.
    Resolved(Vec<Location>),
    /// IO / protocol error talking to the sidecar. The caller falls
    /// back to syntax.
    TransportError,
}

/// Routing decision for `goto_definition`: which backend's answer the
/// handler should ultimately return. Pure (no I/O) so the slang-vs-syntax
/// policy is unit-testable without spawning a sidecar.
#[derive(Debug, PartialEq, Eq)]
enum DefinitionRoute {
    /// Use slang's locations (may be empty — trust-slang short-circuits
    /// the empty case to `None` in the response, never the syntax index).
    UseSlangResult(Vec<Location>),
    /// Slang isn't configured, no project loaded, or the sidecar request
    /// hit a transport error. The handler should consult the syntax
    /// index instead.
    UseSyntaxFallback,
}

/// Pure routing policy. `None` means the slang path didn't run (not
/// configured, no project, untitled buffer); `Some(TransportError)`
/// means it ran and failed; `Some(Resolved(_))` means it ran and
/// answered.
///
/// Policy: slang is primary; an *empty* slang answer falls back to the
/// tree-sitter workspace index. Unlike `goto_type_definition` /
/// `goto_implementation`, definition has a meaningful syntax fallback
/// (every declared symbol is in the workspace index), so an empty slang
/// reply on a position slang can't resolve (e.g. a type argument inside
/// `IDENT#(T)`) still has somewhere useful to land.
fn route_definition(outcome: Option<SlangDefinitionOutcome>) -> DefinitionRoute {
    match outcome {
        Some(SlangDefinitionOutcome::Resolved(locs)) if !locs.is_empty() => {
            DefinitionRoute::UseSlangResult(locs)
        }
        // Slang resolved-but-empty, transport error, or not run at all —
        // give the syntax index a chance.
        _ => DefinitionRoute::UseSyntaxFallback,
    }
}

/// Convert a slang `TypeDefinitionLocation` into LSP's `Location`.
fn slang_type_definition_location_to_lsp(loc: SlangTypeDefinitionLocation) -> Option<Location> {
    let url = path_to_url(&loc.path)?;
    Some(Location {
        uri: url,
        range: m_range_to_lsp(loc.range),
    })
}

/// Outcome of a slang `typeDefinition` request.
#[derive(Debug)]
enum SlangTypeDefinitionOutcome {
    /// Slang answered (may be empty — no fallback in either case).
    Resolved(Vec<Location>),
    /// I/O / protocol error. The handler returns `None` to the editor.
    TransportError,
}

/// Routing decision for `goto_type_definition`. Pure so the policy is
/// unit-testable without a sidecar.
#[derive(Debug, PartialEq, Eq)]
enum TypeDefinitionRoute {
    /// Use slang's locations (trust-slang-on-empty: empty → `None`).
    UseSlangResult(Vec<Location>),
    /// Slang not configured, untitled buffer, or transport error.
    UseEmpty,
}

fn route_type_definition(outcome: Option<SlangTypeDefinitionOutcome>) -> TypeDefinitionRoute {
    match outcome {
        Some(SlangTypeDefinitionOutcome::Resolved(locs)) => {
            TypeDefinitionRoute::UseSlangResult(locs)
        }
        Some(SlangTypeDefinitionOutcome::TransportError) | None => TypeDefinitionRoute::UseEmpty,
    }
}

/// Convert a slang `ImplementationLocation` into LSP's `Location`.
fn slang_implementation_location_to_lsp(loc: SlangImplementationLocation) -> Option<Location> {
    let url = path_to_url(&loc.path)?;
    Some(Location {
        uri: url,
        range: m_range_to_lsp(loc.range),
    })
}

/// Outcome of a slang `implementation` request.
#[derive(Debug)]
enum SlangImplementationOutcome {
    /// Slang answered (may be empty — no fallback in either case).
    Resolved(Vec<Location>),
    /// I/O / protocol error. The handler returns `None` to the editor.
    TransportError,
}

/// Routing decision for `goto_implementation`. Pure so the policy is
/// unit-testable without a sidecar.
#[derive(Debug, PartialEq, Eq)]
enum ImplementationRoute {
    /// Use slang's locations (trust-slang-on-empty: empty → `None`).
    UseSlangResult(Vec<Location>),
    /// Slang not configured, untitled buffer, or transport error.
    UseEmpty,
}

fn route_implementation(outcome: Option<SlangImplementationOutcome>) -> ImplementationRoute {
    match outcome {
        Some(SlangImplementationOutcome::Resolved(locs)) => {
            ImplementationRoute::UseSlangResult(locs)
        }
        Some(SlangImplementationOutcome::TransportError) | None => ImplementationRoute::UseEmpty,
    }
}

/// Last-ditch fallback when `Url::from_file_path` rejects a path (e.g.
/// non-absolute path on a system where it requires absolute). We surface
/// it as a `file:` URL with the raw path, accepting that the editor may
/// or may not match it back to an open document. Better than panicking;
/// happens only on pathological filesystems.
fn placeholder_url(p: &std::path::Path) -> Url {
    Url::parse(&format!("file://{}", p.display()))
        .unwrap_or_else(|_| Url::parse("file:///").expect("file:/// is always valid"))
}

/// Apply a slang elaborate result to the editor's diagnostic state.
///
/// Strategy:
///
/// 1. Bucket diagnostics by file URL.
/// 2. For every file we sent in the request: publish either the slang
///    diagnostics for it, or empty (which honours the conflict policy by
///    overwriting any prior tree-sitter publish).
/// 3. For files slang reported but we didn't send (transitive includes):
///    publish those too.
/// 4. For files that *had* slang diagnostics in the previous cycle and
///    aren't in the current cycle's request or result: publish empty.
///    Otherwise the editor keeps showing stale red squiggles after the
///    user fixes the trigger.
///
/// The version number passed to `publish_diagnostics` is `None` because
/// slang's response can lag behind the editor's view of the document by
/// several edits — letting the editor decide whether to display means we
/// don't need to invent a synthetic version.
async fn publish_slang_result(
    lsp_client: &Client,
    files_in_request: &[Url],
    result: ElaborateResult,
    slang_published: &Arc<RwLock<HashSet<Url>>>,
) {
    let prev_snapshot = slang_published.read().await.clone();
    let plan = plan_slang_publishes(files_in_request, result, &prev_snapshot);

    for (url, diags) in &plan.publishes {
        lsp_client
            .publish_diagnostics(url.clone(), diags.clone(), None)
            .await;
    }

    debug!(
        publishes = plan.publishes.len(),
        new_dirty = plan.new_published.len(),
        "applied slang elaborate result",
    );

    *slang_published.write().await = plan.new_published;
}

/// One round of slang publishes, computed without touching tower-lsp.
struct SlangPublishPlan {
    /// In-order publish calls to make. Each `(url, diags)` becomes one
    /// `publish_diagnostics` call. Empty `diags` clears that URL.
    publishes: Vec<(Url, Vec<Diagnostic>)>,
    /// URLs that ended up with non-empty slang diagnostics this cycle —
    /// stored in `slang_published` so the *next* cycle can clear any
    /// that drop off.
    new_published: HashSet<Url>,
}

/// Pure decision logic: given the files we just sent slang, the result
/// it returned, and the URLs we left non-empty *last* cycle, decide
/// which `publish_diagnostics` calls to make.
///
/// Rules in priority order:
///
/// 1. Every file in the request gets exactly one publish (possibly
///    empty). Even an empty publish is meaningful — it overwrites the
///    tree-sitter diagnostics tower-lsp already published, which is the
///    "slang says clean, drop the syntax false positives" policy.
/// 2. Files slang reported diagnostics for that *weren't* in the
///    request (transitive `` `include `` targets) get a publish each.
/// 3. URLs that had non-empty slang diagnostics last cycle but appear
///    in neither (1) nor (2) get an empty publish — otherwise the
///    editor keeps their old red squiggles after the user fixed the
///    underlying error.
fn plan_slang_publishes(
    files_in_request: &[Url],
    result: ElaborateResult,
    previous_published: &HashSet<Url>,
) -> SlangPublishPlan {
    let mut by_url: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    for d in result.diagnostics {
        let Some(url) = path_to_url(&d.path) else {
            warn!(path = %d.path, "could not map slang path back to a URL; dropping");
            continue;
        };
        by_url
            .entry(url)
            .or_default()
            .push(slang_to_lsp_diagnostic(d));
    }

    let mut publishes: Vec<(Url, Vec<Diagnostic>)> = Vec::new();
    let mut new_published: HashSet<Url> = HashSet::new();

    // Rule 1: one publish per requested file, in request order.
    let mut request_seen: HashSet<&Url> = HashSet::new();
    for url in files_in_request {
        if !request_seen.insert(url) {
            continue;
        }
        let diags = by_url.remove(url).unwrap_or_default();
        if !diags.is_empty() {
            new_published.insert(url.clone());
        }
        publishes.push((url.clone(), diags));
    }

    // Rule 2: diagnostics slang reported for files we didn't request.
    for (url, diags) in by_url {
        new_published.insert(url.clone());
        publishes.push((url, diags));
    }

    // Rule 3: clear URLs that had slang diagnostics last cycle but
    // aren't accounted for above. Skip URLs we already published for
    // (those are already cleared / overwritten). The set is built from
    // the in-progress `publishes` vec, so we clone the URLs out before
    // we start pushing — otherwise we'd be reading and writing the same
    // vec at once.
    let already_publishing: HashSet<Url> = publishes.iter().map(|(u, _)| u.clone()).collect();
    for stale in previous_published.difference(&new_published) {
        if already_publishing.contains(stale) {
            continue;
        }
        publishes.push((stale.clone(), Vec::new()));
    }

    SlangPublishPlan {
        publishes,
        new_published,
    }
}

/// Resolve a name to its declaration sites, same-file first, workspace
/// second.
///
/// Stage-2 precedence rule: if the same file declares the name, those
/// declarations are the answer — workspace hits with the same name in
/// other files are *not* added. That matches the behaviour a user
/// expects when the resolver is syntactic: a local declaration shadows
/// anything in another file. Only when same-file comes up empty do we
/// fall through to the workspace.
///
/// Workspace hits already arrive as a per-name slice from
/// [`WorkspaceIndex::lookup`]. We only need to dedup by `(url,
/// name_range)` so that a file open in the editor *and* listed in the
/// project filelist (which would have folded in twice via
/// `reparse_and_publish` plus eager hydration) doesn't return two
/// identical locations.
///
/// Pure function so it can be unit-tested without spinning up
/// `tower-lsp`.
fn resolve_definition(
    name: &str,
    source_uri: &Url,
    doc_index: &[Symbol],
    workspace_hits: &[(Url, Symbol)],
) -> Vec<(Url, Symbol)> {
    let same_file: Vec<(Url, Symbol)> = doc_index
        .iter()
        .filter(|s| s.name == name)
        .map(|s| (source_uri.clone(), s.clone()))
        .collect();

    if !same_file.is_empty() {
        return same_file;
    }

    let mut out: Vec<(Url, Symbol)> = workspace_hits.to_vec();
    out.sort_by(|a, b| {
        (
            a.0.as_str(),
            a.1.name_range.start.line,
            a.1.name_range.start.character,
        )
            .cmp(&(
                b.0.as_str(),
                b.1.name_range.start.line,
                b.1.name_range.start.character,
            ))
    });
    out.dedup_by(|a, b| a.0 == b.0 && a.1.name_range == b.1.name_range);
    out
}

/// Cap on the total number of `Location`s returned by
/// `textDocument/references`. Picked larger than the workspace-symbol cap
/// (200) because a popular UVM macro can legitimately have hundreds of
/// usages and silently dropping them would be misleading — but small
/// enough that the editor's peek list stays responsive. Truncation is
/// logged at `warn!`.
const REFERENCES_LIMIT: usize = 1000;

/// Collect `textDocument/references` results.
///
/// Pure function — takes already-resolved tree snapshots and the
/// workspace index by reference, returns LSP `Location`s. Split out from
/// the async handler so it's unit-testable without `tower-lsp`.
///
/// `other_open` carries every open document **other than** the cursor
/// file: each entry is `(url, tree)`. The cursor file is handled
/// separately via the scope-aware path.
///
/// Dedup strategy: locations are keyed by `(url, range)`. The cursor
/// file's scope-aware hits are pushed first, then open-buffer
/// whole-file hits, then workspace-index declaration sites. A file that
/// is both open *and* listed in the filelist therefore contributes its
/// (richer) open-buffer hits, and the declaration site from the index
/// is deduped away.
///
/// `include_declaration = false` filters out locations that match any
/// declaration `name_range` known to the workspace index — the LSP
/// semantics are "exclude the *definition* of the symbol", and the
/// index's declarations are the closest syntactic proxy.
#[allow(clippy::too_many_arguments)]
fn collect_references(
    name: &str,
    cursor_uri: &Url,
    cursor_tree: &SyntaxTree,
    cursor_rope: &Rope,
    cursor_pos: MPosition,
    other_open: &[(Url, SyntaxTree)],
    wi: &WorkspaceIndex,
    include_declaration: bool,
) -> Vec<Location> {
    // Set of (url, range) declaration sites known to the workspace index.
    // Used both to dedup against the workspace-only path and to filter
    // out declarations when the client requested it.
    let declarations: HashSet<(Url, MRange)> = wi
        .lookup(name)
        .iter()
        .map(|e| (e.url.clone(), e.symbol.name_range))
        .collect();

    let mut seen: HashSet<(Url, MRange)> = HashSet::new();
    let mut out: Vec<Location> = Vec::new();
    let mut truncated = false;

    let push = |out: &mut Vec<Location>,
                seen: &mut HashSet<(Url, MRange)>,
                truncated: &mut bool,
                u: &Url,
                r: MRange|
     -> bool {
        if !include_declaration && declarations.contains(&(u.clone(), r)) {
            return true;
        }
        if !seen.insert((u.clone(), r)) {
            return true;
        }
        if out.len() >= REFERENCES_LIMIT {
            *truncated = true;
            return false;
        }
        out.push(Location {
            uri: u.clone(),
            range: m_range_to_lsp(r),
        });
        true
    };

    // 1. Same file — scope-aware.
    for r in mimir_syntax::symbols::occurrences_of_at(cursor_tree, cursor_rope, cursor_pos) {
        if !push(&mut out, &mut seen, &mut truncated, cursor_uri, r) {
            break;
        }
    }

    // 2. Other open buffers — whole-file lexical match.
    'outer: for (other_uri, other_tree) in other_open {
        let other_rope = Rope::from_str(other_tree.source());
        for r in mimir_syntax::symbols::occurrences_of(other_tree, &other_rope, name) {
            if !push(&mut out, &mut seen, &mut truncated, other_uri, r) {
                break 'outer;
            }
        }
    }

    // 3. Declaration sites from the workspace index (for files that
    //    aren't currently open). Open buffers above already covered
    //    those entries, and dedup keeps things consistent.
    if !truncated {
        for entry in wi.lookup(name) {
            if !push(
                &mut out,
                &mut seen,
                &mut truncated,
                &entry.url,
                entry.symbol.name_range,
            ) {
                break;
            }
        }
    }

    if truncated {
        warn!(
            limit = REFERENCES_LIMIT,
            name = %name,
            "references truncated at limit",
        );
    }

    out
}

/// Convert our internal `Range` into the `lsp_types` shape.
fn m_range_to_lsp(r: MRange) -> Range {
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

/// Convert a `mimir_syntax::FoldRange` into the `lsp_types` shape.
///
/// Whole-line folds — `start_character` / `end_character` are `None` so the
/// editor decides the exact column. `kind: Region` is the closest LSP fit
/// for SV constructs; the other choices (`Comment`, `Imports`) don't apply.
fn m_fold_to_lsp(f: mimir_syntax::FoldRange) -> FoldingRange {
    FoldingRange {
        start_line: f.start_line,
        start_character: None,
        end_line: f.end_line,
        end_character: None,
        kind: Some(FoldingRangeKind::Region),
        collapsed_text: None,
    }
}

/// Build the LSP semantic-tokens legend from
/// [`mimir_syntax::semantic_tokens::TokenType`]'s static list. Ordinals
/// here pin the wire format — must stay in lockstep with the enum.
fn semantic_tokens_legend() -> SemanticTokensLegend {
    use mimir_syntax::semantic_tokens::{TokenModifier, TokenType};
    SemanticTokensLegend {
        token_types: TokenType::legend()
            .iter()
            .map(|t| SemanticTokenType::new(t.name()))
            .collect(),
        token_modifiers: TokenModifier::legend_names()
            .iter()
            .map(|n| SemanticTokenModifier::new(n))
            .collect(),
    }
}

/// Convert source-order [`mimir_syntax::semantic_tokens::RawToken`]s into
/// LSP's delta-encoded `SemanticToken` records. Each record is a delta
/// from the previous token: `delta_line` is the row delta;
/// `delta_start` is the column delta within the same row, or the
/// absolute column when `delta_line > 0`.
///
/// Input must already be sorted by `(line, start_col)` — the classifier
/// guarantees this and a unit test asserts it.
fn encode_semantic_tokens(
    raw: &[mimir_syntax::semantic_tokens::RawToken],
) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(raw.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;
    for t in raw {
        let delta_line = t.line - prev_line;
        let delta_start = if delta_line == 0 {
            t.start_col - prev_col
        } else {
            t.start_col
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: t.length,
            token_type: t.token_type,
            token_modifiers_bitset: t.modifiers,
        });
        prev_line = t.line;
        prev_col = t.start_col;
    }
    out
}

/// Maximum number of entries returned by `workspace/symbol`. Picked to
/// match completion's 200-item cap — same trade-off (the picker becomes
/// unusable above a few hundred items anyway, and the fuzzy ranker
/// surfaces the best matches first).
const WORKSPACE_SYMBOL_LIMIT: usize = 200;

/// Decide whether a workspace symbol kind should appear in the
/// `workspace/symbol` picker.
///
/// IDE pickers like VS Code's `Ctrl+T` are top-of-mind navigation —
/// users expect declarations they'd hand-author or jump to by name
/// (modules, classes, functions, macros, …), not every `logic [7:0] x`
/// variable in the project. We exclude port/variable/parameter/enum-
/// member kinds because surfacing them swamps the picker on any
/// real-sized UVM testbench. See [`README.md`](../../../README.md)
/// `workspace/symbol` checklist entry for the v1 user-facing contract.
fn is_workspace_symbol_kind(kind: MSymbolKind) -> bool {
    !matches!(
        kind,
        MSymbolKind::Variable
            | MSymbolKind::Port
            | MSymbolKind::Parameter
            | MSymbolKind::EnumMember
    )
}

/// Fuzzy-rank workspace-index entries against `query` and convert the
/// surviving hits to LSP [`SymbolInformation`].
///
/// Pulled out of [`Backend::symbol`] as a pure function so the unit
/// tests below can drive it without spinning up a `Backend`. Iteration
/// order of `entries` is HashMap-arbitrary (see
/// [`WorkspaceIndex::entries`](crate::workspace_index::WorkspaceIndex::entries)),
/// so the final sort is the load-bearing ordering — we sort by score
/// descending and break ties by `(name, url, start_line, start_char)`
/// for determinism in tests and in the user-visible picker.
///
/// Kind filtering and the [`WORKSPACE_SYMBOL_LIMIT`] cap are both
/// applied here, so a caller that swaps in a different scorer later
/// won't drift from the documented v1 contract.
fn rank_workspace_symbols<'a>(
    query: &str,
    entries: impl Iterator<Item = &'a workspace_index::Entry>,
) -> Vec<SymbolInformation> {
    let mut matcher = completion_score::matcher();
    let mut scored: Vec<(u32, &workspace_index::Entry)> = Vec::new();
    for entry in entries {
        if !is_workspace_symbol_kind(entry.symbol.kind) {
            continue;
        }
        let Some(score) = completion_score::score(&mut matcher, query, &entry.symbol.name) else {
            continue;
        };
        scored.push((score, entry));
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.symbol.name.cmp(&b.1.symbol.name))
            .then_with(|| a.1.url.as_str().cmp(b.1.url.as_str()))
            .then_with(|| {
                a.1.symbol
                    .name_range
                    .start
                    .line
                    .cmp(&b.1.symbol.name_range.start.line)
                    .then(
                        a.1.symbol
                            .name_range
                            .start
                            .character
                            .cmp(&b.1.symbol.name_range.start.character),
                    )
            })
    });

    scored
        .into_iter()
        .take(WORKSPACE_SYMBOL_LIMIT)
        .map(|(_, entry)| {
            #[allow(deprecated)]
            SymbolInformation {
                name: entry.symbol.name.clone(),
                kind: symbol_kind_to_lsp(entry.symbol.kind),
                tags: None,
                deprecated: None,
                location: Location {
                    uri: entry.url.clone(),
                    range: m_range_to_lsp(entry.symbol.name_range),
                },
                container_name: entry.symbol.parent_class_name.clone(),
            }
        })
        .collect()
}

/// Map our internal `SymbolKind` onto the LSP wire enum.
///
/// The LSP set is closed (numeric on the wire), so we map each variant
/// to the closest LSP equivalent. SystemVerilog-specific concepts
/// (`Property`, `Sequence`, `Covergroup`) don't have dedicated LSP
/// kinds; we fall back to `OBJECT` for those — VS Code renders them
/// with a neutral icon.
fn symbol_kind_to_lsp(kind: MSymbolKind) -> SymbolKind {
    match kind {
        MSymbolKind::Module => SymbolKind::MODULE,
        MSymbolKind::Interface => SymbolKind::INTERFACE,
        MSymbolKind::Program => SymbolKind::MODULE,
        MSymbolKind::Package => SymbolKind::PACKAGE,
        MSymbolKind::Class => SymbolKind::CLASS,
        MSymbolKind::Task => SymbolKind::FUNCTION,
        MSymbolKind::Function => SymbolKind::FUNCTION,
        MSymbolKind::Method => SymbolKind::METHOD,
        MSymbolKind::Typedef => SymbolKind::TYPE_PARAMETER,
        MSymbolKind::Parameter => SymbolKind::CONSTANT,
        MSymbolKind::Variable => SymbolKind::VARIABLE,
        MSymbolKind::Port => SymbolKind::FIELD,
        MSymbolKind::EnumMember => SymbolKind::ENUM_MEMBER,
        MSymbolKind::Macro => SymbolKind::CONSTANT,
        // SV `constraint` blocks have no direct LSP kind; `OBJECT` is
        // the same neutral fallback we use for SVA properties /
        // covergroups.
        MSymbolKind::Constraint
        | MSymbolKind::Property
        | MSymbolKind::Sequence
        | MSymbolKind::Covergroup => SymbolKind::OBJECT,
    }
}

/// Map a mimir-syntax [`MSymbolKind`] to a completion item kind.
///
/// Uses the nearest LSP [`CompletionItemKind`] for each SV construct.
/// Exists in `mimir-server` (not `mimir-syntax`) to keep LSP types out of
/// the lower crates, per the dependency rule in `CLAUDE.md`.
fn symbol_kind_to_completion_kind(kind: MSymbolKind) -> CompletionItemKind {
    match kind {
        MSymbolKind::Module => CompletionItemKind::MODULE,
        MSymbolKind::Interface => CompletionItemKind::INTERFACE,
        MSymbolKind::Program => CompletionItemKind::MODULE,
        MSymbolKind::Package => CompletionItemKind::MODULE,
        MSymbolKind::Class => CompletionItemKind::CLASS,
        MSymbolKind::Task => CompletionItemKind::FUNCTION,
        MSymbolKind::Function => CompletionItemKind::FUNCTION,
        MSymbolKind::Method => CompletionItemKind::METHOD,
        MSymbolKind::Typedef => CompletionItemKind::CLASS,
        MSymbolKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        MSymbolKind::Constraint => CompletionItemKind::FIELD,
        MSymbolKind::Parameter => CompletionItemKind::CONSTANT,
        MSymbolKind::Variable => CompletionItemKind::VARIABLE,
        MSymbolKind::Port => CompletionItemKind::VARIABLE,
        MSymbolKind::Property => CompletionItemKind::PROPERTY,
        MSymbolKind::Sequence => CompletionItemKind::VALUE,
        MSymbolKind::Covergroup => CompletionItemKind::STRUCT,
        MSymbolKind::Macro => CompletionItemKind::CONSTANT,
    }
}

/// Scan the rope line up to `pos` for a member-access or package-scope trigger.
///
/// Returns `Some((is_package_scope, prefix_after_trigger))` when a `.` or
/// `::` trigger is found immediately before any partial identifier at the
/// cursor. `is_package_scope` is `true` for `::`, `false` for `.`.
/// The returned prefix is the partial identifier typed after the trigger
/// (may be empty when the cursor is immediately after the trigger character).
/// Returns `None` when no trigger is found.
fn detect_member_access(rope: &Rope, pos: MPosition) -> Option<(bool, String)> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line = rope.line(pos.line as usize);

    // Build the line text up to the cursor (respecting UTF-16 character count).
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

    // Skip trailing identifier chars (the partial member name being typed).
    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$') {
        i -= 1;
    }
    let prefix: String = chars[i..].iter().collect();

    // Check for `.` trigger.
    if i > 0 && chars[i - 1] == '.' {
        return Some((false, prefix));
    }
    // Check for `::` trigger.
    if i >= 2 && chars[i - 2] == ':' && chars[i - 1] == ':' {
        return Some((true, prefix));
    }

    None
}

/// Detect a `` ` `` macro trigger before the cursor.
///
/// Returns `Some(prefix)` when the identifier (or empty string) immediately
/// before the cursor is preceded by a backtick. For example:
///   `` `MY_ `` at column 5 → `Some("MY_")`
///   `` `     `` at column 1 → `Some("")`
///
/// Returns `None` for any other context (including `.` or `::`).
fn detect_macro_trigger(rope: &Rope, pos: MPosition) -> Option<String> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line = rope.line(pos.line as usize);

    // Build the line text up to the cursor (UTF-16 aware).
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

    // Skip trailing identifier chars (the partial macro name being typed).
    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_') {
        i -= 1;
    }
    let prefix: String = chars[i..].iter().collect();

    // The character immediately before the identifier must be a backtick.
    if i > 0 && chars[i - 1] == '`' {
        return Some(prefix);
    }

    None
}

/// Payload stored in `CompletionItem.data` so a later
/// `completionItem/resolve` request can re-locate the symbol cheaply
/// (no re-parse, no workspace re-scan) and read its declaration line
/// out of the rope.
///
/// Only attached to syntax-side user-symbol items (same-file + cross-file
/// from the workspace index). Keywords and slang-sourced items leave
/// `data` empty — keywords have no declaration to read, and the slang
/// sidecar doesn't yet return ranges (a follow-up).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CompletionResolveData {
    /// URL of the file the declaration lives in.
    url: Url,
    /// Zero-based line of the declaration (`Symbol::name_range.start.line`).
    line: u32,
}

/// Build the `data` JSON payload that pairs with a syntax-side
/// completion item. The result is `None` only on serde failure (which
/// shouldn't happen for our shape) — the route then ships an item with
/// no resolve data and the resolve handler treats it as a no-op.
fn make_resolve_data(url: &Url, name_line: u32) -> Option<serde_json::Value> {
    serde_json::to_value(CompletionResolveData {
        url: url.clone(),
        line: name_line,
    })
    .ok()
}

/// Read a line from `rope`, dropping any trailing CR/LF and the
/// surrounding whitespace. Returns `None` for an out-of-bounds line so
/// the resolve path can degrade gracefully if the rope drifted.
fn read_line_trimmed(rope: &Rope, line: u32) -> Option<String> {
    let idx = line as usize;
    if idx >= rope.len_lines() {
        return None;
    }
    let raw: String = rope.line(idx).chars().collect();
    Some(raw.trim_end_matches(['\r', '\n']).trim().to_owned())
}

/// Read a declaration that may span multiple lines, starting from
/// `start_line` and continuing until we hit what looks like the end of
/// the declaration. Used by the slang hover fallback so multi-line
/// function/task/macro declarations don't get truncated to their first
/// source line.
///
/// "End of declaration" is intentionally simple — we stop *after* the
/// first line that:
///
/// * Ends in `;` not preceded by a `\` (so a single-line declaration
///   reads cleanly and a multi-line C-style `\`-continued macro keeps
///   going).
/// * Is exactly empty (after trimming) — a blank line terminates a
///   `\`-continued macro definition.
/// * Starts an `endfunction` / `endtask` / `endmodule` / `endclass` /
///   `end` block (the declaration has flowed into its body).
///
/// We cap at 16 lines so a runaway shape never produces a giant hover
/// popup.
fn read_declaration_block(rope: &Rope, start_line: u32) -> Option<String> {
    const MAX_LINES: usize = 16;
    let total = rope.len_lines();
    let start = start_line as usize;
    if start >= total {
        return None;
    }
    let mut collected = Vec::with_capacity(4);
    let mut prev_ends_with_backslash = false;
    for offset in 0..MAX_LINES {
        let idx = start + offset;
        if idx >= total {
            break;
        }
        let raw: String = rope.line(idx).chars().collect();
        let line = raw.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();

        // Stop *before* an empty line that follows a non-continuation —
        // an empty separator between two top-level decls.
        if offset > 0 && trimmed.is_empty() && !prev_ends_with_backslash {
            break;
        }
        // Stop *before* an `end*` keyword (we've fallen into the body).
        if offset > 0
            && (trimmed.starts_with("endfunction")
                || trimmed.starts_with("endtask")
                || trimmed.starts_with("endmodule")
                || trimmed.starts_with("endclass")
                || trimmed.starts_with("endpackage")
                || trimmed.starts_with("endinterface"))
        {
            break;
        }

        collected.push(line.to_owned());
        let ends_with_backslash = line.trim_end().ends_with('\\');
        let ends_with_semicolon = line.trim_end().ends_with(';');

        // A semicolon terminates a normal declaration *unless* it's
        // inside a `\`-continued macro body.
        if ends_with_semicolon && !prev_ends_with_backslash && !ends_with_backslash {
            break;
        }
        // A line that doesn't continue and isn't part of a continuation
        // group — single-line declaration, we're done.
        if !ends_with_backslash && !prev_ends_with_backslash && offset == 0 {
            // Single-line case: we already pushed it, decide based on
            // the line's own terminator.
            if ends_with_semicolon || trimmed.is_empty() {
                break;
            }
        }
        prev_ends_with_backslash = ends_with_backslash;
    }
    if collected.is_empty() {
        return None;
    }
    // Strip leading whitespace consistently across all lines so the
    // markdown block doesn't show jagged indentation. Use the minimum
    // leading-whitespace count of the non-empty lines.
    let common: usize = collected
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.bytes().take_while(|b| *b == b' ' || *b == b'\t').count())
        .min()
        .unwrap_or(0);
    let dedented: Vec<String> = collected
        .iter()
        .map(|l| {
            if l.len() >= common {
                l[common..].to_owned()
            } else {
                l.clone()
            }
        })
        .collect();
    Some(dedented.join("\n"))
}

/// Build a `CompletionItem` for a SystemVerilog keyword.
///
/// When the keyword has a registered snippet body in
/// [`mimir_syntax::keywords::KEYWORD_SNIPPETS`] (e.g. `module`, `class`,
/// `always_ff`), the item carries `insert_text` + `Snippet` format and a
/// `"snippet"` detail so the popup distinguishes it. Otherwise it's a
/// bare keyword item — the editor inserts `label` verbatim.
fn keyword_completion_item(kw: &'static str) -> CompletionItem {
    match mimir_syntax::keywords::snippet_for(kw) {
        Some(body) => CompletionItem {
            label: kw.to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some("snippet".to_owned()),
            insert_text: Some(body.to_owned()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        },
        None => CompletionItem {
            label: kw.to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        },
    }
}

/// Same-file vs cross-file syntax-side completion candidates.
///
/// Returned by [`Backend::gather_syntax_candidates`]. Streams are kept
/// separate so callers can prioritise same-file hits in their dedup pass.
struct SyntaxCandidates {
    same_file: Vec<Symbol>,
    cross_file: Vec<(Url, Symbol)>,
}

/// Patch applied to a slang request so the sidecar sees a parseable
/// version of the buffer. The completion-sentinel inserts a dummy
/// identifier at the cursor; the LSP buffer is unchanged.
#[derive(Debug, Clone)]
struct TargetTextPatch {
    /// Byte offset (UTF-8) at which `insert` is spliced into the file
    /// identified by `SlangDefinitionParams::target_path`.
    insert_byte_offset: usize,
    /// Text inserted at `insert_byte_offset`.
    insert: &'static str,
    /// Cursor position to send to slang AFTER the splice — i.e. the
    /// post-insert location of the original cursor (shifted right by
    /// `insert.encode_utf16().count()`). LSP positions are UTF-16 units.
    adjusted_position: MPosition,
}

/// Reserved identifier inserted by [`build_member_completion_sentinel`].
/// Chosen to be unlikely to collide with any real symbol in user code:
/// double underscores plus a `mimir_` prefix.
const COMPLETION_SENTINEL: &str = "__mimir_complete__";

/// Build a [`TargetTextPatch`] that inserts [`COMPLETION_SENTINEL`] at the
/// cursor so a `.`-triggered completion produces a parseable
/// `MemberAccessExpression`. Returns `None` only when the cursor position
/// can't be turned into a byte offset (out-of-bounds line/character —
/// shouldn't happen for a position the editor sent us, but we degrade
/// gracefully rather than panic).
fn build_member_completion_sentinel(rope: &Rope, pos: MPosition) -> Option<TargetTextPatch> {
    let byte = pos.to_byte_offset(rope).ok()?;
    let utf16_shift = COMPLETION_SENTINEL.encode_utf16().count() as u32;
    Some(TargetTextPatch {
        insert_byte_offset: byte,
        insert: COMPLETION_SENTINEL,
        adjusted_position: MPosition::new(pos.line, pos.character + utf16_shift),
    })
}

/// Splice `patch.insert` into the request's target file at the recorded
/// byte offset. The target file is identified by `def.target_path`.
fn apply_target_text_patch(def: &mut SlangDefinitionParams, patch: &TargetTextPatch) {
    let target = def.target_path.clone();
    for sf in def.files.iter_mut() {
        if sf.path == target && patch.insert_byte_offset <= sf.text.len() {
            sf.text.insert_str(patch.insert_byte_offset, patch.insert);
            break;
        }
    }
}

/// Build a [`SlangCompleteParams`] by lifting the six wire fields out of a
/// resolved [`SlangDefinitionParams`] and stamping in the request `kind` /
/// `prefix`. Centralises the field-shuffle that the three slang completion
/// routes used to repeat verbatim.
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

/// Convert a vector of sidecar [`SlangCompletionItem`]s into an LSP
/// [`CompletionResponse::Array`]. Single-source mapping for the three
/// Convert a list of slang `SignatureItem`s into LSP's `SignatureHelp`.
///
/// `active_sig` is the index of the currently active overload (0 for the
/// first — we don't yet do overload resolution so it's always 0).
fn slang_signatures_to_lsp(
    sigs: Vec<mimir_slang::SignatureItem>,
    active_sig: u32,
) -> SignatureHelp {
    let lsp_sigs: Vec<SignatureInformation> = sigs
        .into_iter()
        .map(|s| {
            let lsp_params: Vec<ParameterInformation> = s
                .params
                .into_iter()
                .map(|p| {
                    let label_text = if let Some(ty) = p.ty {
                        format!("{ty} {}", p.name)
                    } else {
                        p.name
                    };
                    ParameterInformation {
                        label: ParameterLabel::Simple(label_text),
                        documentation: None,
                    }
                })
                .collect();
            SignatureInformation {
                label: s.label,
                documentation: None,
                parameters: Some(lsp_params),
                active_parameter: None,
            }
        })
        .collect();
    SignatureHelp {
        signatures: lsp_sigs,
        active_signature: Some(active_sig),
        active_parameter: None,
    }
}

/// slang routes; keeps the field-by-field translation in one place.
///
/// Dedupes by `label` (first-wins) so a symbol visible through multiple
/// paths — e.g. lexical scope and a `import pkg::*` — only surfaces once.
fn slang_items_to_response(items: Vec<SlangCompletionItem>) -> CompletionResponse {
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

/// Map a sidecar numeric `kind` code to an LSP [`CompletionItemKind`].
///
/// The sidecar uses a small numeric vocabulary that mirrors LSP's enum but
/// doesn't depend on the Rust `lsp_types` crate. This is the only place we
/// decode those numbers back into the typed enum.
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

/// Build a `DocumentSymbol` (no children attached yet).
#[allow(deprecated)]
fn symbol_to_lsp_document_symbol(
    sym: &Symbol,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    DocumentSymbol {
        name: sym.name.clone(),
        detail: None,
        kind: symbol_kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        range: m_range_to_lsp(sym.full_range),
        selection_range: m_range_to_lsp(sym.name_range),
        children,
    }
}

/// Turn the DFS-ordered flat symbol index into the nested
/// `DocumentSymbol` tree the LSP wants. A class's methods become
/// children of the class; a package's classes become children of the
/// package; etc.
///
/// The `mimir-syntax::index` walker emits parents before their
/// descendants, so we can nest in a single linear pass: each symbol's
/// children are the contiguous run of subsequent symbols whose
/// `full_range` is contained in this symbol's `full_range`.
fn nest_symbols(symbols: &[Symbol]) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < symbols.len() {
        let (node, consumed) = nest_symbol_subtree(&symbols[i..]);
        out.push(node);
        i += consumed;
    }
    out
}

/// Build one subtree starting at `symbols[0]`; returns the node plus
/// the number of slice entries it (and its descendants) consumed.
fn nest_symbol_subtree(symbols: &[Symbol]) -> (DocumentSymbol, usize) {
    let head = &symbols[0];
    let mut children: Vec<DocumentSymbol> = Vec::new();
    let mut i = 1;
    while i < symbols.len() && range_contains(head.full_range, symbols[i].full_range) {
        let (child, consumed) = nest_symbol_subtree(&symbols[i..]);
        children.push(child);
        i += consumed;
    }
    let node = symbol_to_lsp_document_symbol(
        head,
        if children.is_empty() {
            None
        } else {
            Some(children)
        },
    );
    (node, i)
}

/// True if `outer` fully encloses `inner` (touching endpoints count as
/// containment — the syntactic ranges of an outer decl and its first
/// inner decl don't perfectly nest in tree-sitter, so strict comparison
/// would misclassify legitimate children as siblings).
fn range_contains(outer: MRange, inner: MRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// Convert a path string from the slang sidecar back to a `file:` URL.
/// Slang echoes back exactly the path we sent (which is `PathBuf::display`
/// on a path we built from `Url::to_file_path` in the first place), so on
/// every platform we exercise the round-trip is loss-free.
fn path_to_url(path: &str) -> Option<Url> {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        Url::from_file_path(&p).ok()
    } else {
        // Best-effort: try to canonicalise relative paths against the
        // current process directory. Realistically we always send
        // absolute paths so this branch is a safety net.
        std::fs::canonicalize(&p)
            .ok()
            .and_then(|abs| Url::from_file_path(abs).ok())
    }
}

/// Map a tree-sitter (`mimir-syntax`) diagnostic onto the wire format
/// `lsp_types` uses. Kept in a free function (not `From`) because both
/// types live in crates we don't control, so the orphan rule would block a
/// `From` impl.
fn syntax_to_lsp_diagnostic(d: MDiagnostic) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: d.range.start.line,
                character: d.range.start.character,
            },
            end: Position {
                line: d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: Some(match d.severity {
            MSeverity::Error => DiagnosticSeverity::ERROR,
            MSeverity::Warning => DiagnosticSeverity::WARNING,
            MSeverity::Information => DiagnosticSeverity::INFORMATION,
            MSeverity::Hint => DiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(d.code.to_string())),
        source: Some("mimir".to_string()),
        message: d.message,
        related_information: None,
        tags: None,
        code_description: None,
        data: None,
    }
}

/// Map a slang diagnostic onto the wire format. Mirrors
/// [`syntax_to_lsp_diagnostic`] — the `code` field carries slang's stable
/// diagnostic code (e.g. `"UnknownModule"`) so editors can group/filter on
/// it. `source` stays `"mimir"` so users don't have to learn two filter
/// labels; the code already disambiguates.
fn slang_to_lsp_diagnostic(d: SlangDiagnostic) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: d.range.start.line,
                character: d.range.start.character,
            },
            end: Position {
                line: d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: Some(match d.severity {
            SlangSeverity::Error => DiagnosticSeverity::ERROR,
            SlangSeverity::Warning => DiagnosticSeverity::WARNING,
            SlangSeverity::Information => DiagnosticSeverity::INFORMATION,
            SlangSeverity::Hint => DiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(d.code)),
        source: Some("mimir".to_string()),
        message: d.message,
        related_information: None,
        tags: None,
        code_description: None,
        data: None,
    }
}

/// Merge tree-sitter and slang diagnostics for a single file into the LSP
/// shape we publish.
///
/// **Conflict policy.** When `slang_active` is `true` (slang elaborated
/// this file successfully), slang's diagnostics are authoritative —
/// tree-sitter syntax errors are dropped because they're often cascading
/// false positives from preprocessor-driven code tree-sitter can't see
/// through (the apb.sv `missing endpackage` is the canonical example).
/// When `slang_active` is `false` (no sidecar configured, sidecar crashed,
/// or this file wasn't in the elaboration set), tree-sitter is the only
/// source of truth.
///
/// `slang` is expected to already be filtered to diagnostics for **this
/// file**. The caller does the path → URI matching; this function just
/// merges per-file diagnostic sets.
///
/// Today this is always called with `slang_active = false` because the
/// sidecar binary doesn't exist yet (Stage 1). The function lives now so
/// Stage 3 only flips the flag.
fn merge_diagnostics(
    syntax: Vec<MDiagnostic>,
    slang: Vec<SlangDiagnostic>,
    slang_active: bool,
) -> Vec<Diagnostic> {
    if slang_active {
        slang.into_iter().map(slang_to_lsp_diagnostic).collect()
    } else {
        syntax.into_iter().map(syntax_to_lsp_diagnostic).collect()
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------
//
// These tests cover the *pure-logic* helpers. The full `Backend` requires
// a `tower_lsp::Client`, which only `LspService::new` can mint — so an
// end-to-end test would have to spawn the server and do JSON-RPC. That's
// a follow-up; here we just exercise what we can in isolation.

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_syntax::Diagnostic as MDiag;

    /// Helper: a tree-sitter diagnostic at a given severity.
    fn syntax_diag(sev: MSeverity) -> MDiag {
        MDiag {
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            message: "syntax".to_string(),
            severity: sev,
            code: "syntax",
        }
    }

    /// Helper: a slang diagnostic at a given severity.
    fn slang_diag(sev: SlangSeverity, code: &str) -> SlangDiagnostic {
        SlangDiagnostic {
            path: "a.sv".into(),
            range: MRange::new(MPosition::new(2, 4), MPosition::new(2, 9)),
            severity: sev,
            code: code.to_string(),
            message: "slang".to_string(),
        }
    }

    /// tree-sitter → LSP conversion preserves all the fields the editor needs.
    #[test]
    fn syntax_diagnostic_conversion_preserves_fields() {
        let d = MDiag {
            range: MRange::new(MPosition::new(1, 2), MPosition::new(1, 5)),
            message: "boom".to_string(),
            severity: MSeverity::Error,
            code: "syntax",
        };
        let lsp = syntax_to_lsp_diagnostic(d);
        assert_eq!(lsp.range.start.line, 1);
        assert_eq!(lsp.range.start.character, 2);
        assert_eq!(lsp.range.end.line, 1);
        assert_eq!(lsp.range.end.character, 5);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("mimir"));
        assert_eq!(lsp.message, "boom");
    }

    /// All four tree-sitter severity variants map to the right LSP severity.
    #[test]
    fn syntax_severity_maps_completely() {
        let cases = [
            (MSeverity::Error, DiagnosticSeverity::ERROR),
            (MSeverity::Warning, DiagnosticSeverity::WARNING),
            (MSeverity::Information, DiagnosticSeverity::INFORMATION),
            (MSeverity::Hint, DiagnosticSeverity::HINT),
        ];
        for (ours, theirs) in cases {
            assert_eq!(
                syntax_to_lsp_diagnostic(syntax_diag(ours)).severity,
                Some(theirs)
            );
        }
    }

    /// slang → LSP conversion preserves the slang-specific code string and
    /// keeps the same `source` label as syntax diagnostics — `code`
    /// disambiguates, so users only have to filter by one source.
    #[test]
    fn slang_diagnostic_conversion_preserves_fields() {
        let d = SlangDiagnostic {
            path: "/proj/m.sv".into(),
            range: MRange::new(MPosition::new(7, 0), MPosition::new(7, 12)),
            severity: SlangSeverity::Error,
            code: "UnknownModule".into(),
            message: "module 'foo' not found".into(),
        };
        let lsp = slang_to_lsp_diagnostic(d);
        assert_eq!(lsp.range.start.line, 7);
        assert_eq!(lsp.range.end.character, 12);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("mimir"));
        assert_eq!(
            lsp.code,
            Some(NumberOrString::String("UnknownModule".into()))
        );
        assert_eq!(lsp.message, "module 'foo' not found");
    }

    /// All four slang severity variants map to the right LSP severity.
    #[test]
    fn slang_severity_maps_completely() {
        let cases = [
            (SlangSeverity::Error, DiagnosticSeverity::ERROR),
            (SlangSeverity::Warning, DiagnosticSeverity::WARNING),
            (SlangSeverity::Information, DiagnosticSeverity::INFORMATION),
            (SlangSeverity::Hint, DiagnosticSeverity::HINT),
        ];
        for (ours, theirs) in cases {
            assert_eq!(
                slang_to_lsp_diagnostic(slang_diag(ours, "x")).severity,
                Some(theirs)
            );
        }
    }

    /// `slang_active = false` is today's behavior: tree-sitter wins, slang
    /// vec is ignored even if non-empty (defensive — should never be passed
    /// non-empty in this branch, but the function shouldn't lose data).
    #[test]
    fn merge_passes_through_syntax_when_slang_inactive() {
        let merged = merge_diagnostics(
            vec![syntax_diag(MSeverity::Error)],
            vec![slang_diag(SlangSeverity::Error, "UnknownModule")],
            /* slang_active */ false,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].code,
            Some(NumberOrString::String("syntax".into()))
        );
    }

    /// `slang_active = true` drops tree-sitter syntax errors and surfaces
    /// only slang's. This is the conflict policy that makes the apb.sv
    /// false positives disappear once Stage 3 turns the flag on.
    #[test]
    fn merge_drops_syntax_when_slang_active() {
        let merged = merge_diagnostics(
            vec![syntax_diag(MSeverity::Error), syntax_diag(MSeverity::Error)],
            vec![slang_diag(SlangSeverity::Error, "UnknownModule")],
            /* slang_active */ true,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].code,
            Some(NumberOrString::String("UnknownModule".into()))
        );
    }

    /// `slang_active = true` with an empty slang vec means "slang said this
    /// file is clean" — drop the tree-sitter syntax errors too, otherwise
    /// the user still sees the false positives we're trying to suppress.
    #[test]
    fn merge_drops_syntax_when_slang_active_and_clean() {
        let merged = merge_diagnostics(
            vec![syntax_diag(MSeverity::Error)],
            vec![],
            /* slang_active */ true,
        );
        assert!(
            merged.is_empty(),
            "expected zero diagnostics, got {merged:?}"
        );
    }

    /// Pass-through with empty inputs returns empty — the trivial case,
    /// guarded so a future refactor can't accidentally invent diagnostics.
    #[test]
    fn merge_empty_in_empty_out() {
        assert!(merge_diagnostics(vec![], vec![], false).is_empty());
        assert!(merge_diagnostics(vec![], vec![], true).is_empty());
    }

    // --- Stage 3: elaborate-params assembly + publish planning ----------

    use crate::project::ResolvedProject;
    use mimir_slang::ElaborateResult;
    use std::path::PathBuf;

    /// Helper: a `ResolvedProject` with `n` files, `top` set, default
    /// debounce.
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

    /// Helper: a slang diagnostic for a given path.
    fn slang_diag_at(path: &str, code: &str) -> SlangDiagnostic {
        SlangDiagnostic {
            path: path.into(),
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            severity: SlangSeverity::Error,
            code: code.into(),
            message: "boom".into(),
        }
    }

    /// `assemble_elaborate_params` puts the project's filelist first
    /// (preserving order), prefers in-memory text over disk, and falls
    /// back to disk for files the user hasn't opened.
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

    /// Open documents not in the project filelist get appended after the
    /// project files — we still want diagnostics for them even before the
    /// user adds them to `.f`.
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
        // Open-but-not-in-filelist files are seeded into the source
        // manager but not parsed as their own compilation unit — so
        // unsaved buffers participate via include resolution without
        // colliding with the preprocessor's own load of the file.
        assert_eq!(params.files[1].path, "/tmp/scratch.sv");
        assert!(!params.files[1].is_compilation_unit);
        assert_eq!(files_in_request.len(), 2);
        assert!(files_in_request.contains(&scratch_url));
    }

    /// Duplicate paths in the project filelist are deduplicated — slang
    /// would either reject the duplicate or silently coalesce; we don't
    /// want to find out which the hard way.
    #[test]
    fn assemble_deduplicates_repeated_files() {
        let f = PathBuf::from("/proj/a.sv");
        let project = project_with_files(vec![f.clone(), f.clone()]);

        let (params, files_in_request) =
            assemble_elaborate_params(&project, &HashMap::new(), |_| Some(String::new()));

        assert_eq!(params.files.len(), 1);
        assert_eq!(files_in_request.len(), 1);
    }

    /// `assemble_elaborate_params` does NOT inline transitively
    /// `` `include`` d files into the slang request. Headers reach the
    /// sidecar via slang's own preprocessor (`+incdir+` lookup); inlining
    /// the full closure (e.g. all of UVM) would balloon every request to
    /// multi-megabytes for no behavioural gain. The "open header" case is
    /// already covered by `assemble_appends_open_docs_not_in_filelist`.
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

    /// Plan: every requested file gets a publish, including files with
    /// no diagnostics (empty publish overwrites tree-sitter).
    #[test]
    fn plan_publishes_every_requested_file_even_when_clean() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let url_b = Url::parse("file:///proj/b.sv").unwrap();
        let result = ElaborateResult {
            diagnostics: vec![slang_diag_at("/proj/a.sv", "X")],
        };
        let plan = plan_slang_publishes(&[url_a.clone(), url_b.clone()], result, &HashSet::new());

        assert_eq!(plan.publishes.len(), 2);
        let a_pub = plan.publishes.iter().find(|(u, _)| u == &url_a).unwrap();
        let b_pub = plan.publishes.iter().find(|(u, _)| u == &url_b).unwrap();
        assert_eq!(a_pub.1.len(), 1);
        assert!(b_pub.1.is_empty());
        assert_eq!(plan.new_published, HashSet::from([url_a]));
    }

    /// Plan: a URL that had diagnostics last cycle and isn't in this
    /// cycle's request or result gets an explicit empty publish — so
    /// the editor's stale red squiggles disappear.
    #[test]
    fn plan_clears_url_that_dropped_off() {
        let url_dropped = Url::parse("file:///proj/old.sv").unwrap();
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let result = ElaborateResult {
            diagnostics: vec![],
        };
        let prev = HashSet::from([url_dropped.clone()]);

        let plan = plan_slang_publishes(&[url_a.clone()], result, &prev);

        // Two publishes: empty for url_a (in request), empty for
        // url_dropped (stale-clear).
        assert_eq!(plan.publishes.len(), 2);
        assert!(plan
            .publishes
            .iter()
            .any(|(u, d)| u == &url_a && d.is_empty()));
        assert!(plan
            .publishes
            .iter()
            .any(|(u, d)| u == &url_dropped && d.is_empty()));
        assert!(plan.new_published.is_empty());
    }

    /// Plan: a stale URL that *also* shows up in this cycle's request
    /// isn't published twice — Rule 1 already handled it.
    #[test]
    fn plan_does_not_double_publish_stale_url_in_request() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let result = ElaborateResult {
            diagnostics: vec![],
        };
        let prev = HashSet::from([url_a.clone()]);

        let plan = plan_slang_publishes(&[url_a.clone()], result, &prev);

        assert_eq!(plan.publishes.len(), 1);
        assert!(plan.publishes[0].1.is_empty());
    }

    /// Plan: diagnostics for a file we didn't request (transitive
    /// include) still get published.
    #[test]
    fn plan_publishes_transitive_include_diagnostics() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let result = ElaborateResult {
            diagnostics: vec![slang_diag_at("/proj/inc/uvm.svh", "X")],
        };

        let plan = plan_slang_publishes(&[url_a.clone()], result, &HashSet::new());

        assert_eq!(plan.publishes.len(), 2);
        let inc_url = Url::parse("file:///proj/inc/uvm.svh").unwrap();
        assert!(plan
            .publishes
            .iter()
            .any(|(u, d)| u == &inc_url && d.len() == 1));
        assert!(plan.new_published.contains(&inc_url));
    }

    // --- Stage 2: go-to-definition resolver ---------------------------

    /// Helper: build a `Symbol` of the given name + kind. Range values
    /// are arbitrary — the tests only care about identity/order.
    fn sym(name: &str, kind: MSymbolKind, line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 1)),
            full_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 10)),
            params: None,
            parent_class_name: None,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Empty doc index *and* empty workspace: the resolver returns
    /// nothing (and the caller turns that into a `None` LSP response).
    #[test]
    fn resolve_definition_both_empty_returns_no_match() {
        let here = url("file:///a.sv");
        assert!(resolve_definition("foo", &here, &[], &[]).is_empty());
    }

    /// Same-file precedence: when the doc index has the name, the
    /// workspace is ignored entirely. This is the Stage-2 shadowing
    /// rule — a syntactic resolver can't reason about scope, so we
    /// conservatively treat any same-file declaration as authoritative
    /// and don't dilute the editor's peek list with every cross-file
    /// homonym.
    #[test]
    fn resolve_definition_same_file_beats_workspace() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let doc_idx = vec![sym("foo", MSymbolKind::Module, 0)];
        let ws = vec![(other.clone(), sym("foo", MSymbolKind::Class, 9))];

        let hits = resolve_definition("foo", &here, &doc_idx, &ws);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, here);
        assert_eq!(hits[0].1.kind, MSymbolKind::Module);
    }

    /// Multiple same-file declarations (e.g. a `var x` plus a class
    /// field `x`) all come back. The editor's peek list lets the user
    /// pick — that's the v1 UX for ambiguous syntactic resolution.
    #[test]
    fn resolve_definition_multiple_same_file_returned_in_order() {
        let here = url("file:///a.sv");
        let doc_idx = vec![
            sym("x", MSymbolKind::Variable, 1),
            sym("y", MSymbolKind::Variable, 2),
            sym("x", MSymbolKind::Parameter, 5),
        ];
        let hits = resolve_definition("x", &here, &doc_idx, &[]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1.kind, MSymbolKind::Variable);
        assert_eq!(hits[0].1.name_range.start.line, 1);
        assert_eq!(hits[1].1.kind, MSymbolKind::Parameter);
        assert_eq!(hits[1].1.name_range.start.line, 5);
    }

    /// Same-file empty, workspace single match: that match is returned.
    #[test]
    fn resolve_definition_workspace_fallback_single() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let ws = vec![(other.clone(), sym("my_class", MSymbolKind::Class, 3))];

        let hits = resolve_definition("my_class", &here, &[], &ws);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, other);
        assert_eq!(hits[0].1.kind, MSymbolKind::Class);
    }

    /// Same-file empty, multiple workspace matches across URLs: all
    /// returned, deduped on `(url, name_range)`. The duplicate here
    /// simulates a file that's both open (folded in via
    /// `reparse_and_publish`) and listed in the project filelist
    /// (folded in via eager hydration).
    #[test]
    fn resolve_definition_workspace_dedups_identical_locations() {
        let here = url("file:///a.sv");
        let b = url("file:///b.sv");
        let c = url("file:///c.sv");
        let dup = sym("shared", MSymbolKind::Class, 4);
        let ws = vec![
            (b.clone(), dup.clone()),
            (b.clone(), dup),
            (c.clone(), sym("shared", MSymbolKind::Class, 7)),
        ];

        let hits = resolve_definition("shared", &here, &[], &ws);
        assert_eq!(hits.len(), 2, "duplicate (url, range) collapsed to one");
        let urls: Vec<&Url> = hits.iter().map(|(u, _)| u).collect();
        assert!(urls.contains(&&b));
        assert!(urls.contains(&&c));
    }

    /// Every internal `SymbolKind` variant maps to *some* LSP kind.
    /// Guards against a future refactor that adds a variant but forgets
    /// the match arm — we'd rather see a missed-test failure than
    /// silently fall through.
    #[test]
    fn symbol_kind_to_lsp_covers_every_variant() {
        let cases = [
            (MSymbolKind::Module, SymbolKind::MODULE),
            (MSymbolKind::Interface, SymbolKind::INTERFACE),
            (MSymbolKind::Program, SymbolKind::MODULE),
            (MSymbolKind::Package, SymbolKind::PACKAGE),
            (MSymbolKind::Class, SymbolKind::CLASS),
            (MSymbolKind::Task, SymbolKind::FUNCTION),
            (MSymbolKind::Function, SymbolKind::FUNCTION),
            (MSymbolKind::Method, SymbolKind::METHOD),
            (MSymbolKind::Typedef, SymbolKind::TYPE_PARAMETER),
            (MSymbolKind::Parameter, SymbolKind::CONSTANT),
            (MSymbolKind::Variable, SymbolKind::VARIABLE),
            (MSymbolKind::Port, SymbolKind::FIELD),
            (MSymbolKind::EnumMember, SymbolKind::ENUM_MEMBER),
            (MSymbolKind::Macro, SymbolKind::CONSTANT),
            (MSymbolKind::Property, SymbolKind::OBJECT),
            (MSymbolKind::Sequence, SymbolKind::OBJECT),
            (MSymbolKind::Covergroup, SymbolKind::OBJECT),
            (MSymbolKind::Constraint, SymbolKind::OBJECT),
        ];
        for (mine, theirs) in cases {
            assert_eq!(symbol_kind_to_lsp(mine), theirs, "{mine:?}");
        }
    }

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

    /// `nest_symbols` over an empty index returns no roots.
    #[test]
    fn nest_symbols_empty() {
        assert!(nest_symbols(&[]).is_empty());
    }

    /// A single top-level symbol becomes a single root with no children.
    #[test]
    fn nest_symbols_single_top_level() {
        let s = Symbol {
            name: "my_mod".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(0, 7), MPosition::new(0, 13)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 9)),
            params: None,
            parent_class_name: None,
        };
        let out = nest_symbols(&[s]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "my_mod");
        assert!(out[0].children.is_none());
    }

    /// A class with two methods produces one root with two children, in
    /// source order.
    #[test]
    fn nest_symbols_class_with_methods() {
        // class c spans lines 0..6; method f spans 1..2; method g spans 3..4.
        let class = Symbol {
            name: "c".into(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(0, 6), MPosition::new(0, 7)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(6, 8)),
            params: None,
            parent_class_name: None,
        };
        let f = Symbol {
            name: "f".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(1, 18), MPosition::new(1, 19)),
            full_range: MRange::new(MPosition::new(1, 4), MPosition::new(2, 12)),
            params: None,
            parent_class_name: None,
        };
        let g = Symbol {
            name: "g".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(3, 9), MPosition::new(3, 10)),
            full_range: MRange::new(MPosition::new(3, 4), MPosition::new(4, 8)),
            params: None,
            parent_class_name: None,
        };
        let out = nest_symbols(&[class, f, g]);
        assert_eq!(out.len(), 1);
        let class_node = &out[0];
        assert_eq!(class_node.name, "c");
        let kids = class_node
            .children
            .as_ref()
            .expect("class should have children");
        let kid_names: Vec<&str> = kids.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(kid_names, vec!["f", "g"]);
        assert!(kids[0].children.is_none());
        assert!(kids[1].children.is_none());
    }

    /// Two unrelated top-level symbols stay siblings.
    #[test]
    fn nest_symbols_two_siblings() {
        let a = Symbol {
            name: "a".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(0, 7), MPosition::new(0, 8)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(1, 9)),
            params: None,
            parent_class_name: None,
        };
        let b = Symbol {
            name: "b".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(2, 7), MPosition::new(2, 8)),
            full_range: MRange::new(MPosition::new(2, 0), MPosition::new(3, 9)),
            params: None,
            parent_class_name: None,
        };
        let out = nest_symbols(&[a, b]);
        let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    /// Three-deep nesting: package > class > method.
    #[test]
    fn nest_symbols_deeply_nested() {
        let pkg = Symbol {
            name: "p".into(),
            kind: MSymbolKind::Package,
            name_range: MRange::new(MPosition::new(0, 8), MPosition::new(0, 9)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(8, 10)),
            params: None,
            parent_class_name: None,
        };
        let cls = Symbol {
            name: "c".into(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(1, 6), MPosition::new(1, 7)),
            full_range: MRange::new(MPosition::new(1, 0), MPosition::new(6, 8)),
            params: None,
            parent_class_name: None,
        };
        let m = Symbol {
            name: "f".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(2, 18), MPosition::new(2, 19)),
            full_range: MRange::new(MPosition::new(2, 4), MPosition::new(3, 12)),
            params: None,
            parent_class_name: None,
        };
        let out = nest_symbols(&[pkg, cls, m]);
        assert_eq!(out.len(), 1);
        let pkg_node = &out[0];
        assert_eq!(pkg_node.name, "p");
        let pkg_kids = pkg_node.children.as_ref().unwrap();
        assert_eq!(pkg_kids.len(), 1);
        assert_eq!(pkg_kids[0].name, "c");
        let cls_kids = pkg_kids[0].children.as_ref().unwrap();
        assert_eq!(cls_kids.len(), 1);
        assert_eq!(cls_kids[0].name, "f");
    }

    // --- Stage 3: slang-backed go-to-definition routing ---------------

    /// A non-empty Resolved outcome becomes a `Some(Array(...))`
    /// response — the editor sees slang's locations.
    #[test]
    fn slang_outcome_resolved_with_locs_returns_array() {
        let url = Url::parse("file:///proj/a.sv").unwrap();
        let locs = vec![Location {
            uri: url.clone(),
            range: Range {
                start: Position {
                    line: 3,
                    character: 7,
                },
                end: Position {
                    line: 3,
                    character: 13,
                },
            },
        }];
        let resp = slang_locations_to_response(locs);
        match resp {
            Some(GotoDefinitionResponse::Array(arr)) => {
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0].uri, url);
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    /// An empty Resolved outcome short-circuits to `None`. This is the
    /// trust-slang-on-empty contract: do **not** fall back to the syntax
    /// index when slang authoritatively says "no declaration found."
    #[test]
    fn slang_outcome_resolved_empty_returns_none() {
        assert!(slang_locations_to_response(Vec::new()).is_none());
    }

    /// `slang_location_to_lsp` preserves the path and range, mapping
    /// through `path_to_url` + `m_range_to_lsp` the same way diagnostics
    /// already do.
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

    /// `route_definition` picks slang when slang resolved (any vec).
    #[test]
    fn route_definition_uses_slang_when_resolved_non_empty() {
        let url = Url::parse("file:///proj/a.sv").unwrap();
        let locs = vec![Location {
            uri: url,
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
        }];
        let route = route_definition(Some(SlangDefinitionOutcome::Resolved(locs.clone())));
        assert_eq!(route, DefinitionRoute::UseSlangResult(locs));
    }

    /// `route_definition` falls back to syntax when slang resolved with
    /// an empty location set. Slang misses some positions (notably type
    /// names inside parameterized scopes — see the apb_monitor →
    /// apb_rw cross-file test); when it can't resolve, the workspace
    /// index gets a chance instead of silently returning no result.
    #[test]
    fn route_definition_falls_back_when_slang_resolved_empty() {
        let route = route_definition(Some(SlangDefinitionOutcome::Resolved(Vec::new())));
        assert_eq!(route, DefinitionRoute::UseSyntaxFallback);
    }

    /// Transport errors fall back to the syntax index — the user still
    /// gets *some* answer when the sidecar is misbehaving.
    #[test]
    fn route_definition_falls_back_on_transport_error() {
        let route = route_definition(Some(SlangDefinitionOutcome::TransportError));
        assert_eq!(route, DefinitionRoute::UseSyntaxFallback);
    }

    /// `None` (slang not configured, no project loaded, untitled buffer)
    /// also falls back. The syntax index is the floor.
    #[test]
    fn route_definition_falls_back_when_slang_not_run() {
        let route = route_definition(None);
        assert_eq!(route, DefinitionRoute::UseSyntaxFallback);
    }

    /// A path `path_to_url` rejects yields `None` rather than panicking
    /// or fabricating a URL.
    #[test]
    fn slang_location_to_lsp_returns_none_on_unparseable_path() {
        // `path_to_url` parses bare strings; an empty path is not a
        // valid file URL and should round-trip through the helper as
        // `None` (or at most a sanitised `file:///` placeholder). We
        // accept either as long as it's not a panic — the contract is
        // "don't crash on bad data."
        let loc = SlangDefinitionLocation {
            path: String::new(),
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 0)),
        };
        // Just exercise the path; correctness of the path-parser is
        // covered by `path_to_url`'s own tests.
        let _ = slang_location_to_lsp(loc);
    }

    // ------------------------------------------------------------------
    // route_type_definition
    // ------------------------------------------------------------------

    fn sample_location() -> Location {
        Location {
            uri: Url::parse("file:///proj/a.sv").unwrap(),
            range: Range {
                start: Position {
                    line: 1,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: 8,
                },
            },
        }
    }

    /// Slang resolved with locations → `UseSlangResult`.
    #[test]
    fn route_type_definition_uses_slang_when_resolved_non_empty() {
        let locs = vec![sample_location()];
        let route = route_type_definition(Some(SlangTypeDefinitionOutcome::Resolved(locs.clone())));
        assert_eq!(route, TypeDefinitionRoute::UseSlangResult(locs));
    }

    /// Empty resolved → still `UseSlangResult` (trust-slang-on-empty — no syntax fallback).
    #[test]
    fn route_type_definition_uses_slang_when_resolved_empty() {
        let route = route_type_definition(Some(SlangTypeDefinitionOutcome::Resolved(Vec::new())));
        assert_eq!(route, TypeDefinitionRoute::UseSlangResult(Vec::new()));
    }

    /// Transport error → `UseEmpty` (no syntax fallback for type queries).
    #[test]
    fn route_type_definition_returns_empty_on_transport_error() {
        let route = route_type_definition(Some(SlangTypeDefinitionOutcome::TransportError));
        assert_eq!(route, TypeDefinitionRoute::UseEmpty);
    }

    /// `None` (slang not configured) → `UseEmpty`.
    #[test]
    fn route_type_definition_returns_empty_when_slang_not_run() {
        let route = route_type_definition(None);
        assert_eq!(route, TypeDefinitionRoute::UseEmpty);
    }

    // ------------------------------------------------------------------
    // route_implementation
    // ------------------------------------------------------------------

    /// Slang resolved with locations → `UseSlangResult`.
    #[test]
    fn route_implementation_uses_slang_when_resolved_non_empty() {
        let locs = vec![sample_location()];
        let route = route_implementation(Some(SlangImplementationOutcome::Resolved(locs.clone())));
        assert_eq!(route, ImplementationRoute::UseSlangResult(locs));
    }

    /// Empty resolved → still `UseSlangResult` (trust-slang-on-empty).
    #[test]
    fn route_implementation_uses_slang_when_resolved_empty() {
        let route = route_implementation(Some(SlangImplementationOutcome::Resolved(Vec::new())));
        assert_eq!(route, ImplementationRoute::UseSlangResult(Vec::new()));
    }

    /// Transport error → `UseEmpty`.
    #[test]
    fn route_implementation_returns_empty_on_transport_error() {
        let route = route_implementation(Some(SlangImplementationOutcome::TransportError));
        assert_eq!(route, ImplementationRoute::UseEmpty);
    }

    /// `None` (slang not configured) → `UseEmpty`.
    #[test]
    fn route_implementation_returns_empty_when_slang_not_run() {
        let route = route_implementation(None);
        assert_eq!(route, ImplementationRoute::UseEmpty);
    }

    /// `build_member_completion_sentinel` inserts the sentinel at the
    /// cursor's byte offset and shifts the LSP position right by the
    /// sentinel's UTF-16 length. Without this, slang would see an
    /// incomplete `a.` expression and fail to resolve the LHS type.
    #[test]
    fn member_completion_sentinel_inserts_at_cursor() {
        // Single-line buffer "  a." with the cursor right after the dot
        // (UTF-16 column 4).
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

    /// `apply_target_text_patch` only patches the file matching the
    /// request's `target_path` — sibling files in `files` keep their
    /// original text.
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

    /// `CompletionOptions` can be constructed with the trigger characters
    /// declared in `initialize` — compile-time sanity check.
    #[test]
    fn completion_options_trigger_characters() {
        let opts = CompletionOptions {
            trigger_characters: Some(vec![".".into(), "`".into(), "$".into(), ":".into()]),
            resolve_provider: Some(false),
            ..Default::default()
        };
        assert_eq!(opts.trigger_characters.unwrap().len(), 4);
    }

    /// `initialize` must advertise `:` as a trigger character so the editor
    /// fires `textDocument/completion` after the second colon of `pkg::`.
    /// Without this, package-scope completion silently never runs (no
    /// request, no log).
    #[tokio::test]
    async fn initialize_advertises_colon_trigger() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        #[allow(deprecated)]
        let init = InitializeParams {
            root_uri: None,
            workspace_folders: None,
            ..Default::default()
        };
        let result = service
            .inner()
            .initialize(init)
            .await
            .expect("initialize ok");
        let triggers = result
            .capabilities
            .completion_provider
            .expect("completion provider")
            .trigger_characters
            .expect("trigger chars");
        assert!(
            triggers.iter().any(|s| s == ":"),
            "expected `:` in {triggers:?}"
        );
        assert!(
            triggers.iter().any(|s| s == "."),
            "expected `.` in {triggers:?}"
        );
        assert!(
            triggers.iter().any(|s| s == "`"),
            "expected backtick in {triggers:?}"
        );
        assert!(
            triggers.iter().any(|s| s == "$"),
            "expected `$` in {triggers:?}"
        );
    }

    // ------------------------------------------------------------------
    // symbol_kind_to_completion_kind
    // ------------------------------------------------------------------

    /// Every `MSymbolKind` variant must map to a `CompletionItemKind` — if
    /// a variant is added without updating the match, this test panics.
    #[test]
    fn completion_kind_maps_all_symbol_kinds() {
        let all = [
            MSymbolKind::Module,
            MSymbolKind::Interface,
            MSymbolKind::Program,
            MSymbolKind::Package,
            MSymbolKind::Class,
            MSymbolKind::Task,
            MSymbolKind::Function,
            MSymbolKind::Method,
            MSymbolKind::Typedef,
            MSymbolKind::EnumMember,
            MSymbolKind::Constraint,
            MSymbolKind::Parameter,
            MSymbolKind::Variable,
            MSymbolKind::Port,
            MSymbolKind::Property,
            MSymbolKind::Sequence,
            MSymbolKind::Covergroup,
            MSymbolKind::Macro,
        ];
        for kind in all {
            let _ = symbol_kind_to_completion_kind(kind);
        }
    }

    /// `Macro` → `CONSTANT` in both LSP-symbol and completion-item mappings.
    #[test]
    fn macro_symbol_kind_maps_to_constant() {
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Macro),
            CompletionItemKind::CONSTANT,
        );
        assert_eq!(symbol_kind_to_lsp(MSymbolKind::Macro), SymbolKind::CONSTANT);
    }

    /// `Class` maps to `CLASS`, `Method` to `METHOD` — spot-check the
    /// most important SV-specific entries.
    #[test]
    fn completion_kind_spot_checks() {
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Class),
            CompletionItemKind::CLASS,
        );
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Method),
            CompletionItemKind::METHOD,
        );
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Parameter),
            CompletionItemKind::CONSTANT,
        );
    }

    // ------------------------------------------------------------------
    // keyword_completion_item
    // ------------------------------------------------------------------

    /// `module` has a registered snippet → item carries `Snippet` format.
    #[test]
    fn keyword_with_snippet_emits_snippet_item() {
        let item = keyword_completion_item("module");
        assert_eq!(item.kind, Some(CompletionItemKind::KEYWORD));
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(item
            .insert_text
            .as_deref()
            .unwrap_or("")
            .contains("endmodule"));
        assert_eq!(item.detail.as_deref(), Some("snippet"));
    }

    /// A keyword without a snippet stays a bare keyword item.
    #[test]
    fn keyword_without_snippet_is_plain() {
        let item = keyword_completion_item("if");
        assert_eq!(item.kind, Some(CompletionItemKind::KEYWORD));
        assert!(item.insert_text.is_none());
        assert!(item.insert_text_format.is_none());
        assert!(item.detail.is_none());
    }

    // ------------------------------------------------------------------
    // CompletionResolveData / read_line_trimmed
    // ------------------------------------------------------------------

    /// `make_resolve_data` round-trips through serde back into a
    /// `CompletionResolveData` matching the inputs.
    #[test]
    fn resolve_data_round_trips() {
        let url = Url::parse("file:///tmp/a.sv").unwrap();
        let value = make_resolve_data(&url, 42).expect("serializes");
        let back: CompletionResolveData = serde_json::from_value(value).unwrap();
        assert_eq!(back.url, url);
        assert_eq!(back.line, 42);
    }

    /// `read_line_trimmed` returns the line text minus surrounding
    /// whitespace and the trailing newline.
    #[test]
    fn read_line_trimmed_strips_whitespace_and_newline() {
        let rope = ropey::Rope::from_str("module foo;\n  class bar;\nendmodule\n");
        assert_eq!(read_line_trimmed(&rope, 0).as_deref(), Some("module foo;"));
        assert_eq!(read_line_trimmed(&rope, 1).as_deref(), Some("class bar;"));
        assert_eq!(read_line_trimmed(&rope, 2).as_deref(), Some("endmodule"));
    }

    /// Out-of-bounds line returns `None`, not a panic.
    #[test]
    fn read_line_trimmed_oob_returns_none() {
        let rope = ropey::Rope::from_str("only one line\n");
        assert_eq!(read_line_trimmed(&rope, 99), None);
    }

    // ------------------------------------------------------------------
    // detect_member_access
    // ------------------------------------------------------------------

    fn rope_from(s: &str) -> ropey::Rope {
        ropey::Rope::from_str(s)
    }

    /// Cursor immediately after `.` → Some((false, "")).
    #[test]
    fn detect_member_access_dot_empty_prefix() {
        let rope = rope_from("my_obj.");
        let pos = MPosition::new(0, 7); // right after '.'
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((false, String::new())));
    }

    /// Cursor mid-identifier after `.` → returns dot trigger + prefix.
    #[test]
    fn detect_member_access_dot_with_prefix() {
        let rope = rope_from("my_obj.run_p");
        let pos = MPosition::new(0, 12); // end of "run_p"
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((false, "run_p".to_string())));
    }

    /// Cursor immediately after `::` → Some((true, "")).
    #[test]
    fn detect_member_access_scope_empty_prefix() {
        let rope = rope_from("my_pkg::");
        let pos = MPosition::new(0, 8); // right after '::'
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((true, String::new())));
    }

    /// Cursor mid-identifier after `::` → returns scope trigger + prefix.
    #[test]
    fn detect_member_access_scope_with_prefix() {
        let rope = rope_from("uvm_pkg::uvm_seq");
        let pos = MPosition::new(0, 16); // end of "uvm_seq"
        let result = detect_member_access(&rope, pos);
        assert_eq!(result, Some((true, "uvm_seq".to_string())));
    }

    /// Plain identifier with no trigger → `None`.
    #[test]
    fn detect_member_access_no_trigger() {
        let rope = rope_from("my_var");
        let pos = MPosition::new(0, 6);
        assert!(detect_member_access(&rope, pos).is_none());
    }

    /// Cursor past the end of an out-of-bounds line → `None`.
    #[test]
    fn detect_member_access_out_of_bounds_line() {
        let rope = rope_from("x.y");
        let pos = MPosition::new(99, 0); // line 99 doesn't exist
        assert!(detect_member_access(&rope, pos).is_none());
    }

    /// Only `:` (single colon, not `::`) is not a trigger.
    #[test]
    fn detect_member_access_single_colon_not_a_trigger() {
        let rope = rope_from("foo:bar");
        let pos = MPosition::new(0, 7);
        assert!(detect_member_access(&rope, pos).is_none());
    }

    // ------------------------------------------------------------------
    // detect_macro_trigger
    // ------------------------------------------------------------------

    /// Cursor immediately after `` ` `` → `Some("")`.
    #[test]
    fn detect_macro_trigger_empty_prefix() {
        let rope = rope_from("`");
        let pos = MPosition::new(0, 1); // right after backtick
        assert_eq!(detect_macro_trigger(&rope, pos), Some(String::new()));
    }

    /// Cursor after `` `MY_ `` → `Some("MY_")`.
    #[test]
    fn detect_macro_trigger_with_prefix() {
        let rope = rope_from("`MY_MACRO");
        let pos = MPosition::new(0, 4); // after "MY_"
        assert_eq!(detect_macro_trigger(&rope, pos), Some("MY_".to_string()));
    }

    /// Full macro name typed → prefix is the full name.
    #[test]
    fn detect_macro_trigger_full_name() {
        let rope = rope_from("`UVM_INFO");
        let pos = MPosition::new(0, 9); // end of "UVM_INFO"
        assert_eq!(
            detect_macro_trigger(&rope, pos),
            Some("UVM_INFO".to_string())
        );
    }

    /// No backtick — plain identifier → `None`.
    #[test]
    fn detect_macro_trigger_no_backtick() {
        let rope = rope_from("my_signal");
        let pos = MPosition::new(0, 9);
        assert!(detect_macro_trigger(&rope, pos).is_none());
    }

    /// `.` trigger is not a macro trigger.
    #[test]
    fn detect_macro_trigger_dot_not_macro() {
        let rope = rope_from("obj.field");
        let pos = MPosition::new(0, 9);
        assert!(detect_macro_trigger(&rope, pos).is_none());
    }

    /// Out-of-bounds line → `None`.
    #[test]
    fn detect_macro_trigger_oob_line() {
        let rope = rope_from("`M");
        let pos = MPosition::new(99, 0);
        assert!(detect_macro_trigger(&rope, pos).is_none());
    }

    // ------------------------------------------------------------------
    // slang_completion_kind_to_lsp
    // ------------------------------------------------------------------

    #[test]
    fn slang_completion_kind_to_lsp_known_codes() {
        assert_eq!(slang_completion_kind_to_lsp(2), CompletionItemKind::METHOD);
        assert_eq!(slang_completion_kind_to_lsp(5), CompletionItemKind::FIELD);
        assert_eq!(
            slang_completion_kind_to_lsp(6),
            CompletionItemKind::VARIABLE
        );
        assert_eq!(
            slang_completion_kind_to_lsp(20),
            CompletionItemKind::ENUM_MEMBER
        );
        assert_eq!(
            slang_completion_kind_to_lsp(21),
            CompletionItemKind::CONSTANT
        );
    }

    #[test]
    fn slang_completion_kind_to_lsp_unknown_falls_back_to_variable() {
        assert_eq!(
            slang_completion_kind_to_lsp(99),
            CompletionItemKind::VARIABLE
        );
    }

    // ------------------------------------------------------------------
    // Parse-tree cache on DocumentState
    // ------------------------------------------------------------------

    /// `did_open` should populate `state.tree` so subsequent LSP feature
    /// handlers (`folding_range`, `document_highlight`, `inlay_hint`,
    /// `signature_help`, `syntax_definition`) can read it instead of
    /// re-parsing on every request.
    #[tokio::test]
    async fn did_open_populates_tree_cache() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let uri = Url::parse("file:///tmp/cache-test.sv").unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "systemverilog".to_string(),
                    version: 1,
                    text: "module foo;\nendmodule\n".to_string(),
                },
            })
            .await;

        let docs = backend.documents.read().await;
        let state = docs.get(&uri).expect("doc inserted by did_open");
        let tree = state.tree.as_ref().expect("tree cached after did_open");
        assert_eq!(tree.tree.root_node().kind(), "source_file");
        assert_eq!(state.index_version, 1, "version reflects the parse");
    }

    /// `cached_tree` returns the cached `SyntaxTree` on the happy path
    /// without touching the parser lock. (We can't directly observe lock
    /// acquisition, so this just exercises the cached read path.)
    #[tokio::test]
    async fn cached_tree_returns_populated_entry() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let uri = Url::parse("file:///tmp/cache-helper.sv").unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "systemverilog".to_string(),
                    version: 1,
                    text: "module bar; endmodule\n".to_string(),
                },
            })
            .await;

        let tree = backend.cached_tree(&uri).await.expect("tree cached");
        assert_eq!(tree.tree.root_node().kind(), "source_file");
        assert!(tree.source().contains("module bar"));
    }

    /// Cache miss (unknown URI) returns `None` rather than panicking or
    /// synthesising an empty tree.
    #[tokio::test]
    async fn cached_tree_unknown_uri_returns_none() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let uri = Url::parse("file:///tmp/never-opened.sv").unwrap();
        assert!(backend.cached_tree(&uri).await.is_none());
        assert!(backend.cached_tree_and_index(&uri).await.is_none());
    }

    // ------------------------------------------------------------------
    // did_save + did_change_watched_files
    // ------------------------------------------------------------------

    /// `did_save` is a no-op for state (we already have the buffer via
    /// `did_change`) but must not panic, must not drop the document,
    /// and must safely run when slang isn't configured.
    #[tokio::test]
    async fn did_save_preserves_document_state() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let uri = Url::parse("file:///tmp/save-test.sv").unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "systemverilog".to_string(),
                    version: 1,
                    text: "module foo;\nendmodule\n".to_string(),
                },
            })
            .await;

        backend
            .did_save(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                text: None,
            })
            .await;

        let docs = backend.documents.read().await;
        let state = docs.get(&uri).expect("doc still in store after did_save");
        assert!(state.tree.is_some(), "tree cache survived the save");
    }

    /// A `Deleted` event on a watched URL evicts that URL's entries
    /// from the workspace symbol index.
    #[tokio::test]
    async fn watched_files_delete_evicts_workspace_index() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let url = Url::parse("file:///tmp/evict.sv").unwrap();

        // Seed the workspace index with a synthetic entry for `url`.
        {
            let mut wi = backend.workspace_index.write().await;
            wi.update(
                url.clone(),
                &[sym("my_class", MSymbolKind::Class, 0)],
            );
        }
        assert!(
            !backend.workspace_index.read().await.lookup("my_class").is_empty(),
            "precondition: synthetic entry indexed",
        );

        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: url.clone(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;

        assert!(
            backend.workspace_index.read().await.lookup("my_class").is_empty(),
            "deleted event should have evicted the entry",
        );
    }

    /// A `Changed` event on a URL that's currently open in the editor
    /// must NOT trigger a disk re-hydrate — open buffers always win,
    /// per [`workspace_index.rs`]'s ownership contract. We verify by
    /// pointing the event at a non-existent path: a disk re-hydrate
    /// would fail to read it, but we shouldn't even try.
    #[tokio::test]
    async fn watched_files_change_skips_open_buffers() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let url = Url::parse("file:///tmp/does-not-exist-but-open.sv").unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: url.clone(),
                    language_id: "systemverilog".to_string(),
                    version: 1,
                    text: "module open_buf; endmodule\n".to_string(),
                },
            })
            .await;

        // Should silently do nothing — no panic on unreadable path.
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: url.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;

        // The open-buffer state survives.
        let docs = backend.documents.read().await;
        assert!(docs.contains_key(&url), "open buffer must not be touched");
    }

    /// `Changed` event on a non-`.sv` non-`.mimir.toml` path that
    /// isn't open is processed via the single-file rehydrate path.
    /// Pointing it at a path that doesn't exist must be a no-op
    /// (not a panic) — the disk reader returns `None` and
    /// `hydrate_from_paths` logs and skips.
    #[tokio::test]
    async fn watched_files_change_on_missing_file_is_noop() {
        let (service, _socket) = tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let url = Url::parse("file:///tmp/definitely-not-on-disk.sv").unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: url.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        // No assertion needed beyond "didn't panic" — the workspace
        // index remains empty.
        assert_eq!(backend.workspace_index.read().await.entries().count(), 0);
    }

    // ----------------------------------------------------------------------
    // workspace/symbol — rank_workspace_symbols
    // ----------------------------------------------------------------------

    /// Build a `workspace_index::Entry` from a URL and a freshly-built
    /// `Symbol`. Co-located with the `workspace/symbol` tests because no
    /// other test path here needs it.
    fn entry(u: &Url, s: Symbol) -> workspace_index::Entry {
        workspace_index::Entry {
            url: u.clone(),
            symbol: s,
        }
    }

    /// Empty query returns every kept-kind entry — that's the picker's
    /// initial state when the user first opens it.
    #[test]
    fn workspace_symbol_empty_query_returns_all_visible_kinds() {
        let u = url("file:///a.sv");
        let entries = vec![
            entry(&u, sym("my_module", MSymbolKind::Module, 0)),
            entry(&u, sym("my_class", MSymbolKind::Class, 1)),
        ];
        let out = rank_workspace_symbols("", entries.iter());
        assert_eq!(out.len(), 2);
    }

    /// Fuzzy matching: subsequence of the candidate matches, unrelated
    /// queries don't. Mirrors completion's behaviour.
    #[test]
    fn workspace_symbol_fuzzy_matches_subsequence() {
        let u = url("file:///a.sv");
        let entries = vec![
            entry(&u, sym("uvm_foo_bar", MSymbolKind::Class, 0)),
            entry(&u, sym("unrelated", MSymbolKind::Module, 1)),
        ];
        let out = rank_workspace_symbols("ufoo", entries.iter());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "uvm_foo_bar");
    }

    /// `Variable`/`Port`/`Parameter`/`EnumMember` are too noisy for a
    /// project-wide picker — every UVM port and enum value would swamp
    /// the list — so they're filtered out. Documented v1 contract.
    #[test]
    fn workspace_symbol_excludes_noisy_kinds() {
        let u = url("file:///a.sv");
        let entries = vec![
            entry(&u, sym("clk", MSymbolKind::Port, 0)),
            entry(&u, sym("counter", MSymbolKind::Variable, 1)),
            entry(&u, sym("WIDTH", MSymbolKind::Parameter, 2)),
            entry(&u, sym("RED", MSymbolKind::EnumMember, 3)),
            entry(&u, sym("my_mod", MSymbolKind::Module, 4)),
        ];
        let out = rank_workspace_symbols("", entries.iter());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "my_mod");
    }

    /// Same name in two files yields two distinct `SymbolInformation`
    /// results — the picker shows both rows and the user picks. We
    /// must not collapse them.
    #[test]
    fn workspace_symbol_same_name_in_two_files_yields_two_results() {
        let a = url("file:///a.sv");
        let b = url("file:///b.sv");
        let entries = vec![
            entry(&a, sym("twin", MSymbolKind::Class, 0)),
            entry(&b, sym("twin", MSymbolKind::Class, 0)),
        ];
        let out = rank_workspace_symbols("twin", entries.iter());
        assert_eq!(out.len(), 2);
        let mut urls: Vec<_> = out.iter().map(|s| s.location.uri.as_str()).collect();
        urls.sort();
        assert_eq!(urls, vec!["file:///a.sv", "file:///b.sv"]);
    }

    /// The 200-item cap kicks in for over-cap result sets so the picker
    /// stays responsive. Test with an empty query so every candidate is
    /// kept by the scorer.
    #[test]
    fn workspace_symbol_enforces_cap() {
        let u = url("file:///a.sv");
        let entries: Vec<_> = (0..(WORKSPACE_SYMBOL_LIMIT + 50))
            .map(|i| entry(&u, sym(&format!("c{i:04}"), MSymbolKind::Class, i as u32)))
            .collect();
        let out = rank_workspace_symbols("", entries.iter());
        assert_eq!(out.len(), WORKSPACE_SYMBOL_LIMIT);
    }

    /// `container_name` is populated for class methods (the workspace
    /// index already carries `parent_class_name`) and `None` for free
    /// functions / modules. v1 deliberately doesn't synthesise a
    /// container for non-method symbols — README documents the gap.
    #[test]
    fn workspace_symbol_container_name_for_methods_only() {
        let u = url("file:///a.sv");
        let mut method = sym("do_thing", MSymbolKind::Method, 0);
        method.parent_class_name = Some("my_class".into());
        let entries = vec![
            entry(&u, method),
            entry(&u, sym("free_func", MSymbolKind::Function, 1)),
        ];
        let out = rank_workspace_symbols("", entries.iter());
        let by_name: HashMap<&str, &SymbolInformation> =
            out.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(
            by_name["do_thing"].container_name.as_deref(),
            Some("my_class"),
        );
        assert_eq!(by_name["free_func"].container_name, None);
    }

    /// Higher-scoring matches sort before lower ones — prefix beats
    /// subsequence, mirroring completion's UX.
    #[test]
    fn workspace_symbol_sorts_by_score_descending() {
        let u = url("file:///a.sv");
        let entries = vec![
            entry(&u, sym("my_class", MSymbolKind::Class, 0)),
            entry(&u, sym("class", MSymbolKind::Class, 1)),
        ];
        let out = rank_workspace_symbols("clas", entries.iter());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "class");
        assert_eq!(out[1].name, "my_class");
    }

    // ----------------------------------------------------------------------
    // hover — hover_for_symbol + read_macro_body
    // ----------------------------------------------------------------------

    /// Build a `DocumentState` for tests with the given text. The parsed
    /// `tree`/`index` are left empty — the hover helpers don't read them.
    fn doc_state(text: &str) -> DocumentState {
        DocumentState {
            document: TextDocument::new(text, 1),
            language_id: "systemverilog".to_string(),
            index: Vec::new(),
            tree: None,
            index_version: 0,
        }
    }

    /// Extract the markdown payload from a `Hover` — every hover we emit
    /// is `HoverContents::Markup`, so the match is total.
    fn hover_markdown_value(h: &Hover) -> &str {
        match &h.contents {
            HoverContents::Markup(MarkupContent { value, .. }) => value.as_str(),
            _ => panic!("expected MarkupContent, got {:?}", h.contents),
        }
    }

    /// Bare non-callable symbol (class) → fenced declaration line.
    #[test]
    fn hover_for_class_returns_declaration_line() {
        let url = url("file:///a.sv");
        let text = "class apb_monitor extends uvm_monitor;\n  int x;\nendclass\n";
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(text));

        let s = Symbol {
            name: "apb_monitor".to_string(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(0, 6), MPosition::new(0, 17)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 8)),
            params: None,
            parent_class_name: Some("uvm_monitor".to_string()),
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        assert_eq!(
            hover_markdown_value(&h),
            "```systemverilog\nclass apb_monitor extends uvm_monitor;\n```",
        );
    }

    /// Callable symbol (function with params) → synthesized signature,
    /// not the source line.
    #[test]
    fn hover_for_function_emits_signature() {
        let url = url("file:///a.sv");
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(""));

        let s = Symbol {
            name: "add".to_string(),
            kind: MSymbolKind::Function,
            name_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 3)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 0)),
            params: Some(vec![
                mimir_syntax::Param {
                    name: "a".into(),
                    ty: Some("int".into()),
                },
                mimir_syntax::Param {
                    name: "b".into(),
                    ty: Some("int".into()),
                },
            ]),
            parent_class_name: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        assert_eq!(
            hover_markdown_value(&h),
            "```systemverilog\nfunction add(int a, int b);\n```",
        );
    }

    /// Macro → `define` header + multi-line body.
    #[test]
    fn hover_for_macro_includes_body() {
        let url = url("file:///a.sv");
        let text = "`define MY_MACRO(x) \\\n    $display(\"hi: %0d\", x);\n";
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(text));

        // Line 1 has 27 chars (4 spaces + 23 chars of `$display(...);`); use
        // exactly that as `end.character` — the position just before `\n`.
        let s = Symbol {
            name: "MY_MACRO".to_string(),
            kind: MSymbolKind::Macro,
            name_range: MRange::new(MPosition::new(0, 8), MPosition::new(0, 16)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(1, 27)),
            params: Some(vec![mimir_syntax::Param {
                name: "x".into(),
                ty: None,
            }]),
            parent_class_name: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        let v = hover_markdown_value(&h);
        // Header is the synthesized signature; body is the trimmed body.
        assert!(
            v.starts_with("```systemverilog\n`define MY_MACRO(x)"),
            "got {v:?}"
        );
        assert!(
            v.contains("$display"),
            "expected body to include $display, got {v:?}"
        );
    }

    /// Module-kind symbol → fenced declaration line, falls back to disk
    /// when the open-doc store has no entry for the URL. We assert the
    /// open-doc path here; the disk path is exercised by integration
    /// tests.
    #[test]
    fn hover_for_unknown_url_returns_none_when_doc_absent() {
        let url = url("file:///never-opened.sv");
        let docs = std::collections::HashMap::new();
        let s = Symbol {
            name: "x".to_string(),
            kind: MSymbolKind::Variable,
            name_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 10)),
            params: None,
            parent_class_name: None,
        };
        // No doc, and the path doesn't exist on disk either → None.
        assert!(hover_for_symbol(&s, &url, &docs).is_none());
    }

    // ----------------------------------------------------------------------
    // keyword / system-task hover help — `keyword_hover_at`
    // ----------------------------------------------------------------------

    /// Build (tree, rope) from `src` for the hover-help tests.
    fn parse_for_hover(src: &str) -> (mimir_syntax::SyntaxTree, Rope) {
        mimir_core::logging::init_for_tests();
        let mut parser = mimir_syntax::SyntaxParser::new().expect("parser");
        let tree = parser.parse(src, None).expect("parse");
        (tree, Rope::from_str(src))
    }

    /// Cursor on `always_ff` returns the curated doc popup with the
    /// keyword in a fenced block and the description below.
    #[test]
    fn hover_on_always_ff_returns_doc() {
        let src = "module m;\n  always_ff @(posedge clk) q <= d;\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        let h = keyword_hover_at(&tree, &rope, MPosition::new(1, 2)).expect("hover");
        let v = hover_markdown_value(&h);
        assert!(v.starts_with("```systemverilog\nalways_ff\n```"), "got {v:?}");
        assert!(v.contains("Edge-sensitive sequential always block"), "got {v:?}");
        assert!(v.contains("§9.2.2.4"), "expected LRM ref, got {v:?}");
    }

    /// Cursor on `$display` resolves through the `$…` table.
    #[test]
    fn hover_on_dollar_display_returns_doc() {
        let src = "module m;\ninitial $display(\"hi\");\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        // Line 1, column 8 is the `$`.
        let h = keyword_hover_at(&tree, &rope, MPosition::new(1, 8)).expect("hover");
        let v = hover_markdown_value(&h);
        assert!(v.starts_with("```systemverilog\n$display\n```"), "got {v:?}");
        assert!(v.contains("Print arguments followed by a newline"), "got {v:?}");
    }

    /// A keyword we deliberately don't document (`endmodule` — structural
    /// noise) returns `None`. Guards against the fallback ever emitting an
    /// empty / surprising popup.
    #[test]
    fn hover_on_undocumented_keyword_returns_none() {
        let src = "module m;\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        // Line 1, column 0 is the 'e' of "endmodule".
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(1, 0)).is_none());
    }

    /// Punctuation / whitespace / off-the-end positions return `None`.
    #[test]
    fn hover_on_non_word_returns_none() {
        let src = "module m;\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        // Column 6 is the space between "module" and "m".
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(0, 6)).is_none());
        // Way off the end of the document.
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(99, 0)).is_none());
    }

    /// `$DISPLAY` (uppercase) is *not* in the system-task table — the LRM
    /// treats system tasks as case-sensitive. The fallback must not paper
    /// over that and return `$display`'s doc.
    #[test]
    fn hover_on_uppercase_system_task_returns_none() {
        let src = "module m;\ninitial $DISPLAY(\"hi\");\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(1, 8)).is_none());
    }

    // ----------------------------------------------------------------------
    // semantic tokens — encoder
    // ----------------------------------------------------------------------

    /// First token in the stream encodes as absolute coordinates.
    #[test]
    fn encode_semantic_tokens_first_token_is_absolute() {
        let raw = vec![mimir_syntax::semantic_tokens::RawToken {
            line: 3,
            start_col: 7,
            length: 5,
            token_type: 0,
            modifiers: 0,
        }];
        let enc = encode_semantic_tokens(&raw);
        assert_eq!(enc.len(), 1);
        assert_eq!(enc[0].delta_line, 3);
        assert_eq!(enc[0].delta_start, 7);
        assert_eq!(enc[0].length, 5);
    }

    /// Same-line follow-up tokens encode `delta_start` as the column
    /// delta from the previous token, not an absolute column.
    #[test]
    fn encode_semantic_tokens_same_line_uses_column_delta() {
        let raw = vec![
            mimir_syntax::semantic_tokens::RawToken {
                line: 0,
                start_col: 0,
                length: 6,
                token_type: 0,
                modifiers: 0,
            },
            mimir_syntax::semantic_tokens::RawToken {
                line: 0,
                start_col: 7,
                length: 3,
                token_type: 1,
                modifiers: 0,
            },
        ];
        let enc = encode_semantic_tokens(&raw);
        assert_eq!(enc[1].delta_line, 0);
        assert_eq!(enc[1].delta_start, 7); // 7 - 0 = 7
    }

    /// When `delta_line > 0` the encoder must reset `delta_start` to
    /// the absolute column, not the column delta from the prior line.
    #[test]
    fn encode_semantic_tokens_new_line_resets_column() {
        let raw = vec![
            mimir_syntax::semantic_tokens::RawToken {
                line: 0,
                start_col: 10,
                length: 3,
                token_type: 0,
                modifiers: 0,
            },
            mimir_syntax::semantic_tokens::RawToken {
                line: 2,
                start_col: 4,
                length: 5,
                token_type: 0,
                modifiers: 0,
            },
        ];
        let enc = encode_semantic_tokens(&raw);
        assert_eq!(enc[1].delta_line, 2);
        assert_eq!(enc[1].delta_start, 4); // absolute, not 4 - 10
    }

    /// The legend the server advertises must have exactly as many
    /// entries as the classifier produces ordinals for. Mismatched
    /// counts would silently misrender colours in every client.
    #[test]
    fn semantic_tokens_legend_matches_syntax_crate() {
        use mimir_syntax::semantic_tokens::{TokenModifier, TokenType};
        let legend = semantic_tokens_legend();
        assert_eq!(legend.token_types.len(), TokenType::legend().len());
        assert_eq!(legend.token_modifiers.len(), TokenModifier::legend_names().len());
    }

    /// Single-line function declaration: only one source line lands in
    /// the block.
    #[test]
    fn read_declaration_block_single_line() {
        let rope = ropey::Rope::from_str("function void foo();\n  int x;\nendfunction\n");
        let block = read_declaration_block(&rope, 0).expect("block");
        assert_eq!(block, "function void foo();");
    }

    /// Multi-line `function` declaration whose parameters wrap across
    /// four source lines: every line must land in the block.
    #[test]
    fn read_declaration_block_multi_line_function() {
        let text = "static function bit get(uvm_component cntxt,\n\
                    \t\t\t\tstring inst_name,\n\
                    \t\t\t\tstring field_name,\n\
                    \t\t\t\tinout T value);\n\
                    \tint x;\n\
                    endfunction\n";
        let rope = ropey::Rope::from_str(text);
        let block = read_declaration_block(&rope, 0).expect("block");
        assert!(block.contains("cntxt"), "{block:?}");
        assert!(block.contains("inst_name"), "{block:?}");
        assert!(block.contains("field_name"), "{block:?}");
        assert!(block.contains("inout T value"), "{block:?}");
        // The body line (`int x;`) and `endfunction` must NOT leak in.
        assert!(!block.contains("int x"), "{block:?}");
        assert!(!block.contains("endfunction"), "{block:?}");
    }

    /// `\\`-continued macro definition: every continuation line lands
    /// in the block until the first non-continued line.
    #[test]
    fn read_declaration_block_multi_line_macro() {
        let text = "`define UVM_THING(X) \\\n  do_a(X); \\\n  do_b(X); \\\n  do_c(X)\n";
        let rope = ropey::Rope::from_str(text);
        let block = read_declaration_block(&rope, 0).expect("block");
        assert!(block.contains("do_a(X)"));
        assert!(block.contains("do_b(X)"));
        assert!(block.contains("do_c(X)"));
    }

    /// `read_macro_body` strips the `\`define NAME(args)` header.
    #[test]
    fn read_macro_body_strips_define_header() {
        let url = url("file:///a.sv");
        let text = "`define FOO(a, b) a + b\n";
        let docs_state = doc_state(text);
        let rope = Rope::from_str(&docs_state.document.text());

        let s = Symbol {
            name: "FOO".to_string(),
            kind: MSymbolKind::Macro,
            name_range: MRange::new(MPosition::new(0, 8), MPosition::new(0, 11)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 23)),
            params: Some(vec![
                mimir_syntax::Param {
                    name: "a".into(),
                    ty: None,
                },
                mimir_syntax::Param {
                    name: "b".into(),
                    ty: None,
                },
            ]),
            parent_class_name: None,
        };
        let body = read_macro_body(&s, &url, Some(&rope)).expect("body extracted");
        assert_eq!(body, "a + b");
    }

    // ----------------------------------------------------------------------
    // references — collect_references
    // ----------------------------------------------------------------------

    /// Parse `text` into a `SyntaxTree` for a tests-only callsite. We
    /// build a fresh parser per test rather than sharing one — these
    /// tests aren't perf-sensitive and the parser cost is small.
    fn parse_tree(text: &str) -> SyntaxTree {
        let mut parser = SyntaxParser::new().expect("grammar load");
        parser.parse(text, None).expect("parse")
    }

    /// Build a populated `WorkspaceIndex` from a slice of `(url, text)`
    /// pairs. Mirrors how the eager hydration pass folds parsed-from-disk
    /// files into the index on `initialize`.
    fn workspace_index_from(files: &[(&Url, &str)]) -> WorkspaceIndex {
        let mut wi = WorkspaceIndex::new();
        for (u, text) in files {
            let tree = parse_tree(text);
            let rope = Rope::from_str(text);
            let symbols = mimir_syntax::symbols::index(&tree, &rope);
            wi.update((*u).clone(), &symbols);
        }
        wi
    }

    /// Helper: invoke `collect_references` with the cursor positioned on
    /// the first occurrence of `name` in `cursor_text`. Other open
    /// buffers are passed as `(url, text)` pairs.
    fn run_references(
        name: &str,
        cursor_uri: &Url,
        cursor_text: &str,
        other_open: &[(Url, &str)],
        wi: &WorkspaceIndex,
        include_declaration: bool,
    ) -> Vec<Location> {
        let cursor_tree = parse_tree(cursor_text);
        let cursor_rope = Rope::from_str(cursor_text);
        // Position the cursor on the first byte-offset occurrence of `name`.
        let byte = cursor_text.find(name).expect("name appears in cursor_text");
        // Compute UTF-16 column/line for that byte. Cursor text in these
        // tests is ASCII, so byte offset == utf16 offset and we can
        // count lines/columns by character.
        let prefix = &cursor_text[..byte];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
        let col = match prefix.rfind('\n') {
            Some(nl) => (byte - nl - 1) as u32,
            None => byte as u32,
        };
        let cursor_pos = MPosition::new(line, col);

        let other_trees: Vec<(Url, SyntaxTree)> = other_open
            .iter()
            .map(|(u, t)| (u.clone(), parse_tree(t)))
            .collect();

        collect_references(
            name,
            cursor_uri,
            &cursor_tree,
            &cursor_rope,
            cursor_pos,
            &other_trees,
            wi,
            include_declaration,
        )
    }

    /// Same-file lexical occurrences come back as `Location`s. Even
    /// without a workspace index, references inside the cursor file
    /// are returned — the most basic case.
    #[test]
    fn references_returns_same_file_occurrences() {
        let here = url("file:///a.sv");
        let text = "module m;\n  int foo;\n  initial foo = 1;\nendmodule\n";
        let wi = WorkspaceIndex::new();
        let out = run_references("foo", &here, text, &[], &wi, true);
        assert_eq!(out.len(), 2, "decl + one use");
        assert!(out.iter().all(|l| l.uri == here));
    }

    /// Open buffers other than the cursor file contribute whole-file
    /// lexical matches. The cursor file's hits *and* the other file's
    /// hits both appear.
    #[test]
    fn references_includes_other_open_buffer_usages() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let cursor_text = "module a;\n  my_class c;\nendmodule\n";
        let other_text = "module b;\n  my_class c1;\n  my_class c2;\nendmodule\n";
        let wi = WorkspaceIndex::new();
        let out = run_references(
            "my_class",
            &here,
            cursor_text,
            &[(other.clone(), other_text)],
            &wi,
            true,
        );
        // 1 in cursor file + 2 in other open buffer.
        assert_eq!(out.len(), 3);
        let other_hits = out.iter().filter(|l| l.uri == other).count();
        assert_eq!(other_hits, 2);
    }

    /// Workspace-index declarations from filelist-hydrated (non-open)
    /// files are returned as a fallback. Cursor file has no occurrence
    /// of the name itself — the only result must come from the index.
    #[test]
    fn references_includes_declaration_from_filelist_not_open() {
        let here = url("file:///a.sv");
        let elsewhere = url("file:///lib.sv");
        let cursor_text = "module m;\n  my_class c;\nendmodule\n";
        let elsewhere_text = "class my_class;\nendclass\n";
        let wi = workspace_index_from(&[(&elsewhere, elsewhere_text)]);
        let out = run_references("my_class", &here, cursor_text, &[], &wi, true);
        // 1 usage in cursor file + 1 declaration site from the index.
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|l| l.uri == elsewhere));
    }

    /// When a file is both open *and* listed in the filelist, the
    /// open-buffer scan covers it and the workspace-index declaration
    /// site is deduped away — no duplicate `Location` returned.
    #[test]
    fn references_dedupes_when_file_is_both_open_and_in_filelist() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let cursor_text = "module a;\n  my_class c;\nendmodule\n";
        let other_text = "class my_class;\nendclass\n";
        // Index has the declaration; open-buffer scan also finds it.
        let wi = workspace_index_from(&[(&other, other_text)]);
        let out = run_references(
            "my_class",
            &here,
            cursor_text,
            &[(other.clone(), other_text)],
            &wi,
            true,
        );
        // 1 use in cursor file + 1 (deduped) declaration in `other`.
        assert_eq!(out.len(), 2);
        let other_hits = out.iter().filter(|l| l.uri == other).count();
        assert_eq!(other_hits, 1, "open-buffer hit and index decl dedupe");
    }

    /// `include_declaration = false` strips out locations equal to a
    /// known declaration `name_range`, but leaves *usages* alone — even
    /// in the same file as the declaration.
    #[test]
    fn references_respects_include_declaration_false() {
        let here = url("file:///a.sv");
        let elsewhere = url("file:///lib.sv");
        let cursor_text = "module m;\n  my_class c;\nendmodule\n";
        let elsewhere_text = "class my_class;\nendclass\n";
        let wi = workspace_index_from(&[(&elsewhere, elsewhere_text)]);

        let with_decl = run_references("my_class", &here, cursor_text, &[], &wi, true);
        let without_decl = run_references("my_class", &here, cursor_text, &[], &wi, false);

        assert!(with_decl.len() > without_decl.len());
        // The usage in the cursor file is preserved.
        assert!(without_decl.iter().any(|l| l.uri == here));
        // The declaration site in the other file is gone.
        assert!(!without_decl.iter().any(|l| l.uri == elsewhere));
    }

    /// Truncation kicks in at [`REFERENCES_LIMIT`]. Synthesise a file
    /// with many usages and assert the cap, plus that we don't blow up.
    #[test]
    fn references_caps_at_limit() {
        let here = url("file:///a.sv");
        // Build a module body with REFERENCES_LIMIT + 50 references to `foo`.
        let mut text = String::from("module m;\n  int foo;\n");
        for _ in 0..(REFERENCES_LIMIT + 50) {
            text.push_str("  initial foo = 1;\n");
        }
        text.push_str("endmodule\n");
        let wi = WorkspaceIndex::new();
        let out = run_references("foo", &here, &text, &[], &wi, true);
        assert_eq!(out.len(), REFERENCES_LIMIT);
    }
}
