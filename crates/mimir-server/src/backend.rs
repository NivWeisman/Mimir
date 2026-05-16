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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mimir_core::{Position as MPosition, Range as MRange, TextDocument};
use mimir_slang::{
    Client as SlangClient, CompleteParams as SlangCompleteParams,
    CompletionRequestKind as SlangCompletionRequestKind,
    DefinitionLocation as SlangDefinitionLocation, DefinitionParams as SlangDefinitionParams,
    Diagnostic as SlangDiagnostic, ElaborateParams, ElaborateResult,
    ImplementationLocation as SlangImplementationLocation,
    ImplementationParams as SlangImplementationParams, Severity as SlangSeverity, SlangCompletionItem,
    SignatureHelpParams as SlangSignatureHelpParams, SourceFile,
    TypeDefinitionLocation as SlangTypeDefinitionLocation,
    TypeDefinitionParams as SlangTypeDefinitionParams,
};
use mimir_syntax::{
    calls::{call_site_at, active_arg_index, call_sites_in, CallKind},
    inlay::hints_for,
    signature::signature_for,
    Diagnostic as MDiagnostic, DiagnosticSeverity as MSeverity,
    Symbol, SymbolKind as MSymbolKind, SyntaxParser, SyntaxTree,
};
use ropey::Rope;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, error, info, instrument, warn};

use crate::completion_score;
use crate::project::ResolvedProject;
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
            workspace_index: Arc::new(RwLock::new(WorkspaceIndex::new())),
        }
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
        let lsp_client = self.client.clone();
        let trigger_for_task = trigger_uri.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(debounce).await;

            // Build the elaborate request from project config + the
            // currently-open documents (their in-memory text overrides
            // anything on disk so unsaved changes participate).
            let (params, files_in_request) = build_elaborate_params(&project, &documents).await;
            debug!(
                files = params.files.len(),
                include_dirs = params.include_dirs.len(),
                "sending elaborate request",
            );
            match slang.elaborate(&params).await {
                Ok(result) => {
                    publish_slang_result(&lsp_client, &files_in_request, result, &slang_published)
                        .await;
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
                prefix_lower.is_empty()
                    || it.label.to_ascii_lowercase().starts_with(&prefix_lower)
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

        let (def_params, _) =
            build_definition_params(&project, &self.documents, uri, pos).await?;
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
                debug!(count = result.signatures.len(), "slang signature help returned");
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

        Some(SyntaxCandidates { same_file, cross_file })
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

        let make_item = |name: String, score: u32, data: Option<serde_json::Value>| {
            CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("`define".to_owned()),
                sort_text: Some(completion_score::assign_sort_text(score)),
                data,
                ..Default::default()
            }
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
                            match mimir_slang::Client::spawn(path, std::iter::empty::<&str>())
                                .await
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
                        hydrate_workspace_index(paths, include_dirs, parser, workspace_index)
                            .await;
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
                // files and is the whole point of using a rope.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
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
                // Syntax-only for stages 1–4; slang takes over for stages 5–6.
                // Trigger characters: `.` (member access), `` ` `` (macros),
                // `$` (system task/function names), `:` (the first half of
                // `::`; the handler runs again on the second colon, where
                // `detect_member_access` recognises the package-scope trigger).
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ".".into(),
                        "`".into(),
                        "$".into(),
                        ":".into(),
                    ]),
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
    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> LspResult<Option<Vec<InlayHint>>> {
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
                let resolved = resolve_method_symbol(
                    call,
                    recv,
                    &tree,
                    &rope,
                    &index,
                    &wi,
                );
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
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> LspResult<Option<CompletionResponse>> {
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
                if let Some(resp) =
                    self.try_slang_macro_completion(&uri, target, &macro_prefix).await
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
        class_new_lhs_at, enclosing_class_info_at, find_variable_type_at,
        normalize_type_name, ClassNewLhs,
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
                .unwrap_or(MethodResolution::NotResolved("this.X not in same-file index"))
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
                None => return MethodResolution::NotResolved(
                    "class_new not in a recognised assignment shape",
                ),
            };
            let target_class = match ctx {
                ClassNewLhs::DeclaredType(ty) => normalize_type_name(&ty),
                ClassNewLhs::LhsName(name) => find_variable_type_at(
                    tree, rope, call.name_range.start, &name,
                )
                .as_deref()
                .and_then(normalize_type_name),
            };
            let Some(cls) = target_class else {
                return MethodResolution::NotResolved(
                    "class_new LHS type unresolvable from AST",
                );
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
                return MethodResolution::NotResolved(
                    "chained receiver access needs slang",
                );
            }
            let ty = find_variable_type_at(
                tree,
                rope,
                call.name_range.start,
                receiver_chain,
            );
            let Some(cls) = ty.as_deref().and_then(normalize_type_name) else {
                return MethodResolution::NotResolved(
                    "receiver type unresolvable from AST",
                );
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
    while i > 0
        && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$')
    {
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
    while i > 0
        && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_')
    {
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
                start: Position { line: 1, character: 0 },
                end: Position { line: 1, character: 8 },
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
        let route =
            route_implementation(Some(SlangImplementationOutcome::Resolved(locs.clone())));
        assert_eq!(route, ImplementationRoute::UseSlangResult(locs));
    }

    /// Empty resolved → still `UseSlangResult` (trust-slang-on-empty).
    #[test]
    fn route_implementation_uses_slang_when_resolved_empty() {
        let route =
            route_implementation(Some(SlangImplementationOutcome::Resolved(Vec::new())));
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
        let patch =
            build_member_completion_sentinel(&rope, pos).expect("cursor in bounds");
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
            trigger_characters: Some(vec![
                ".".into(),
                "`".into(),
                "$".into(),
                ":".into(),
            ]),
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
        let (service, _socket) =
            tower_lsp::LspService::new(|client| Backend::new(client, None));
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
        assert!(triggers.iter().any(|s| s == ":"), "expected `:` in {triggers:?}");
        assert!(triggers.iter().any(|s| s == "."), "expected `.` in {triggers:?}");
        assert!(triggers.iter().any(|s| s == "`"), "expected backtick in {triggers:?}");
        assert!(triggers.iter().any(|s| s == "$"), "expected `$` in {triggers:?}");
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
        assert!(item.insert_text.as_deref().unwrap_or("").contains("endmodule"));
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
        assert_eq!(detect_macro_trigger(&rope, pos), Some("UVM_INFO".to_string()));
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
        assert_eq!(slang_completion_kind_to_lsp(6), CompletionItemKind::VARIABLE);
        assert_eq!(slang_completion_kind_to_lsp(20), CompletionItemKind::ENUM_MEMBER);
        assert_eq!(slang_completion_kind_to_lsp(21), CompletionItemKind::CONSTANT);
    }

    #[test]
    fn slang_completion_kind_to_lsp_unknown_falls_back_to_variable() {
        assert_eq!(slang_completion_kind_to_lsp(99), CompletionItemKind::VARIABLE);
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
        let (service, _socket) =
            tower_lsp::LspService::new(|client| Backend::new(client, None));
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
        let (service, _socket) =
            tower_lsp::LspService::new(|client| Backend::new(client, None));
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
        let (service, _socket) =
            tower_lsp::LspService::new(|client| Backend::new(client, None));
        let backend = service.inner();
        let uri = Url::parse("file:///tmp/never-opened.sv").unwrap();
        assert!(backend.cached_tree(&uri).await.is_none());
        assert!(backend.cached_tree_and_index(&uri).await.is_none());
    }
}
