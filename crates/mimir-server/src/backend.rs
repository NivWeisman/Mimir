//! The `tower_lsp::LanguageServer` impl — thin coordinator between LSP wire
//! protocol and three focused service structs.
//!
//! ## Responsibilities
//!
//! * Maintain `Url → DocumentState` (the document store) and apply
//!   `did_open` / `did_change` / `did_close` mutations.
//! * After every mutation, parse the document and publish tree-sitter
//!   diagnostics via [`Backend::reparse_and_publish`].
//! * Coordinate LSP feature handlers (hover, definition, completion, …) by
//!   delegating to the appropriate service and merging results.
//!
//! ## Service architecture
//!
//! Heavy logic is split into focused modules so handlers stay thin:
//!
//! | Service | Module | Owns |
//! |---------|--------|------|
//! | [`TreeSitterProvider`] | `parse_provider` | `SyntaxParser` mutex, single-file parse, bulk hydration |
//! | [`SlangService`] | `slang_service` | Sidecar IPC, project config, closed-file cache, param assembly |
//! | [`SyntaxService`] | `syntax_service` | Document store + workspace index access |
//! | [`ElaborateService`] | `elaborate_service` | Debounce, input-hash dedup, diagnostic publish lifecycle |
//!
//! All three hold `Arc` clones of the same underlying data — no extra
//! synchronisation cost beyond the locks already in place.
//!
//! ## Concurrency model
//!
//! `tower-lsp` dispatches every handler concurrently from the tokio runtime.
//! The document store is a `tokio::sync::RwLock<HashMap<Url, DocumentState>>`;
//! the lock is held only long enough to insert/look up — parsing happens
//! outside the lock on a cloned string so concurrent edits don't block each
//! other.
//!
//! tree-sitter is fast enough (~ms for a 5000-line UVM file) that we don't
//! push parsing onto a `spawn_blocking` thread. If huge generated headers ever
//! become a bottleneck we can revisit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mimir_core::{Position as MPosition, Range as MRange, TextDocument};
use mimir_syntax::{
    calls::{active_arg_index, call_site_at, call_sites_in, CallKind},
    inlay::hints_for,
    signature::signature_for,
    Symbol, SymbolKind as MSymbolKind,
    SyntaxTree,
};
use ropey::Rope;
use tokio::sync::RwLock;
use tree_sitter::InputEdit;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, error, info, instrument, warn};

use crate::ast_features;
use crate::chain_resolve;
use crate::code_lens;
use crate::completion_score;
use crate::hierarchy_features;
use crate::hover_features::{
    append_hover_footer, builtin_method_hover_at, cursor_on_macro_usage, hover_for_symbol,
    keyword_hover_at, macro_footer_markdown, read_line_trimmed, ExpandMacroResponse,
};
use crate::includes;
use crate::elaborate_service::ElaborateService;
use crate::format::{invoke_verible, strip_mimir_pragmas, wrap_ifdefs, WrappedSource};
use crate::lsp_convert::{
    encode_semantic_tokens, into_incomplete, keyword_completion_item, m_fold_to_lsp,
    make_resolve_data, merge_diagnostics, nest_symbols, param_inlay_hint, range_lines_edit,
    semantic_tokens_legend, build_selection_range, symbol_kind_to_completion_kind,
    whole_file_edit, CompletionResolveData,
};
use crate::member_features::{
    builtin_to_symbol, resolve_method_symbol, synth_method_symbol, syntax_member_completion,
    MethodResolution,
};
use crate::parse_provider::TreeSitterProvider;
use crate::project::{FeatureToggles, FormatterConfig, ResolvedProject};
use crate::references_features::{collect_references, resolve_definition};
use crate::slang_adapter::SlangAdapter;
use crate::slang_service::{SlangService, detect_member_access, detect_macro_trigger, m_range_to_lsp};
use crate::syntax_service::{SyntaxCandidates, SyntaxService};
use crate::workspace_index::WorkspaceState;
use crate::workspace_symbols::rank_workspace_symbols;

/// Per-document state held inside the store.
///
/// We keep the last parsed `tree` here so LSP feature handlers
/// (`folding_range`, `document_highlight`, `signature_help`, `inlay_hint`,
/// `syntax_definition`) can reuse it instead of re-parsing on every
/// request. `Tree::edit`-driven incremental reparse on `did_change` is
/// still a future slice — for now every parse is full, but at least we
/// only do it once per edit.
#[derive(Debug)]
pub(crate) struct DocumentState {
    /// The live text buffer for this document.
    pub(crate) document: TextDocument,
    /// Language ID the editor reported in `did_open`. Useful for routing
    /// (e.g. we treat `verilog` and `systemverilog` slightly differently
    /// — though for now both go through the same parser).
    #[allow(dead_code)]
    pub(crate) language_id: String,
    /// Symbol index from the most recent successful parse. Empty when
    /// the document hasn't been parsed yet or the last parse failed.
    /// Powers same-file `goto_definition` and `documentSymbol`.
    pub(crate) index: Vec<Symbol>,
    /// Parse tree from the most recent successful parse. `None` between
    /// `did_open` and the first parse completing. On a parse error we
    /// deliberately leave the previous tree in place — serving slightly
    /// stale results mid-keystroke is strictly better than serving none.
    /// `SyntaxTree` clones cheaply (`tree_sitter::Tree` is `Arc` inside).
    pub(crate) tree: Option<SyntaxTree>,
    /// Document version the `index` and `tree` were built from. Used to
    /// detect a stale write — `reparse_and_publish` may finish after a
    /// fresh `did_change` has bumped the version, in which case we must
    /// not overwrite the live cache with the stale parse's results.
    pub(crate) index_version: i32,
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

    /// Tree-sitter parse provider. Owns the `SyntaxParser` mutex and all
    /// parse operations — single-file incremental parse and bulk path
    /// hydration for the workspace index.
    ts: Arc<TreeSitterProvider>,

    /// Single point of contact for all slang-sidecar IPC: client connection,
    /// resolved project config, closed-file disk cache, and every param-
    /// assembly helper.
    slang: Arc<SlangService>,

    /// Drives the `compile` RPC and caches the resulting `MimirAst`.
    /// LSP feature handlers read `adapter.cached_ast()` to answer queries
    /// without waiting for the next background compile cycle.
    adapter: Arc<SlangAdapter>,

    /// Document store + parser + workspace-index Arcs, packaged as a service
    /// so feature handlers can call `cached_tree` / `cached_tree_and_index`
    /// through a named interface rather than reaching into the raw fields.
    /// The Arcs are shared with the identically-named fields below — mutations
    /// through `self.documents` etc. are immediately visible here.
    syntax: SyntaxService,

    /// Debounce + dedup + diagnostic-publish lifecycle for slang compile.
    /// All elaborate state (pending handles, last hash, published URLs) lives
    /// here; handlers call `self.elaborate.schedule(uri)` and return.
    elaborate: ElaborateService,

    /// Combined workspace symbol index and parse trees under a single lock.
    workspace: Arc<RwLock<WorkspaceState>>,
}

impl Backend {
    /// Construct the backend. `slang` is `None` when no sidecar is
    /// configured (today's default — see [`crate::SLANG_PATH_ENV`]).
    ///
    /// Panics if the parser fails to load the SV grammar — that's a build
    /// configuration bug, not a runtime condition, and it would happen on
    /// the very first message we received anyway.
    pub fn new(client: Client, slang: Option<Arc<mimir_slang::Client>>) -> Self {
        let documents: Arc<RwLock<HashMap<Url, DocumentState>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let ts = Arc::new(TreeSitterProvider::new());
        let workspace: Arc<RwLock<WorkspaceState>> =
            Arc::new(RwLock::new(WorkspaceState::default()));
        let slang = Arc::new(SlangService::new(documents.clone(), slang));
        let adapter = Arc::new(SlangAdapter::new(slang.clone()));
        Self {
            syntax: SyntaxService::new(documents.clone(), ts.clone()),
            elaborate: ElaborateService::new(adapter.clone(), client.clone(), workspace.clone()),
            slang,
            adapter,
            client,
            documents,
            ts,
            workspace,
        }
    }

    /// Return the current [`FeatureToggles`] from the resolved project config.
    ///
    /// Falls back to [`FeatureToggles::default`] (all toggles `true`) when no
    /// `.mimir.toml` has been discovered — that keeps every feature on when
    /// the server is used without a project config file.
    async fn current_features(&self) -> FeatureToggles {
        self.slang.current_features().await
    }

    /// Return the current [`FormatterConfig`] from the resolved project config.
    ///
    /// Falls back to [`FormatterConfig::default`] (binary = `"verible-verilog-format"`,
    /// all options unset) when no `.mimir.toml` is present.
    async fn current_formatter_config(&self) -> FormatterConfig {
        self.slang.current_formatter_config().await
    }

    /// Parse the document at `uri` and publish diagnostics to the client.
    ///
    /// Called after every store mutation. Errors are logged and swallowed —
    /// we never propagate a parse failure back to the editor as an LSP
    /// error, because the editor doesn't know what to do with it.
    ///
    /// `edits` are `tree_sitter::InputEdit`s that correspond to the changes
    /// already applied to the rope. When non-empty and a prior tree exists,
    /// we mutate the tree with each edit and pass it as `previous` to the
    /// parser so tree-sitter can reuse unchanged subtrees. When empty (full
    /// sync or first open), we do a full re-parse from scratch.
    #[instrument(level = "debug", skip(self), fields(uri = %uri))]
    async fn reparse_and_publish(&self, uri: Url, edits: Vec<InputEdit>) {
        mimir_core::time_scope!("syntax.reparse_and_publish");
        let (text, version, prior_tree) = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => (
                    state.document.text(),
                    state.document.version(),
                    state.tree.clone(),
                ),
                None => {
                    // Race: document was closed between the edit and our
                    // reparse. Nothing to do.
                    debug!("document gone before reparse, skipping");
                    return;
                }
            }
        };

        // Run the parse outside the doc-store lock. The provider applies
        // edits to the prior tree (incremental reuse) and returns diagnostics
        // + symbols in one shot. On parse error it returns None and has
        // already logged at error! — we deliberately leave state.tree and
        // state.index untouched so mid-keystroke failures don't erase the
        // last-known-good results.
        let parse_result = {
            mimir_core::time_scope!("syntax.parse");
            self.ts.parse(&text, &edits, prior_tree).await
        };
        let (mut diags, new_state) = match parse_result {
            Some(r) => (r.diagnostics, Some((r.symbols, r.tree))),
            None => (Vec::new(), None),
        };

        // UVM-aware lint over the freshly parsed tree (tree-sitter only, no
        // slang). Gated by `[diagnostics] uvm_phase_super_call`; the phase
        // set and severity are configurable. Published alongside the parse
        // diagnostics below.
        if let Some((_, tree)) = new_state.as_ref() {
            let cfg = self.slang.current_uvm_lint_config().await;
            if cfg.phase_super_call {
                let rope = Rope::from_str(&text);
                diags.extend(mimir_syntax::uvm::phase_super_call_diagnostics(
                    tree,
                    &rope,
                    &cfg.phases,
                    cfg.phase_super_severity,
                ));
            }
        }

        // Write the fresh index + tree back into the doc store, but only if
        // the version we parsed is still the live one — otherwise a
        // `did_change` landed mid-parse and our results are already stale.
        // When the write does happen, also fold the new symbols into the
        // workspace index and update the workspace tree cache so that
        // closing this file after editing it doesn't leave a stale tree.
        if let Some((index, tree)) = new_state {
            // Clone the tree before moving it into the doc store so the
            // workspace tree cache gets the same fresh snapshot.
            // `SyntaxTree` clones cheaply — `tree_sitter::Tree` is backed
            // by a reference-counted C struct.
            let tree_for_cache = tree.clone();
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
                let presence_names = mimir_syntax::symbols::identifier_names(tree_for_cache.source());
                let mut ws = self.workspace.write().await;
                ws.update_presence(uri.clone(), presence_names);
                ws.index.update(uri.clone(), &index);
                ws.trees.insert(uri.clone(), tree_for_cache);
            }
        }

        // Tree-sitter publishes immediately so the editor has prompt
        // feedback (~ms after a keystroke). The deeper slang elaborate
        // is scheduled separately on a debounce timer; when it lands it
        // *overwrites* this publish for files in its compilation unit.
        let lsp_diags = merge_diagnostics(diags);

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
    /// Thin wrapper that delegates to [`SyntaxService::cached_tree`]. See
    /// that method for full semantics (cache-miss fallback, error handling,
    /// etc.). Kept here so call sites in `Backend`'s `LanguageServer` impl
    /// don't need to change spelling after the service extraction.
    async fn cached_tree(&self, uri: &Url) -> Option<SyntaxTree> {
        self.syntax.cached_tree(uri).await
    }

    /// Snapshot the cached tree *and* symbol index for `uri` together.
    ///
    /// Thin wrapper that delegates to [`SyntaxService::cached_tree_and_index`].
    /// See that method for full semantics. Kept here so call sites in
    /// `Backend`'s `LanguageServer` impl don't need to change spelling.
    async fn cached_tree_and_index(&self, uri: &Url) -> Option<(SyntaxTree, Vec<Symbol>)> {
        self.syntax.cached_tree_and_index(uri).await
    }

    /// Find every occurrence of `name` across the workspace: the cursor
    /// file (scope-aware), all *other* open-buffer trees, and the
    /// workspace-cached closed-file trees pre-filtered by the
    /// identifier-presence index. The shared engine behind `references`
    /// and `rename`, which scan exactly the same tree set.
    ///
    /// Lock discipline: the documents read lock is taken and released
    /// first; one workspace read-lock hold then covers the closed-tree
    /// collection and the index lookup inside `collect_references`. The
    /// two locks are never held together.
    async fn collect_workspace_references(
        &self,
        uri: &Url,
        cursor_tree: &SyntaxTree,
        cursor_rope: &Rope,
        target: MPosition,
        name: &str,
        include_declaration: bool,
    ) -> Vec<Location> {
        // Snapshot the open-doc trees (excluding the cursor file) under a
        // single read lock. We clone the trees — `SyntaxTree` is cheap to
        // clone (`tree_sitter::Tree` is internally `Arc`).
        let other_open: Vec<(Url, SyntaxTree)> = {
            let docs = self.documents.read().await;
            docs.iter()
                .filter(|(other_uri, _)| *other_uri != uri)
                .filter_map(|(other_uri, state)| {
                    state.tree.as_ref().map(|t| (other_uri.clone(), t.clone()))
                })
                .collect()
        };

        // Closed-file trees: all URLs in the workspace tree cache that
        // aren't the cursor file and aren't already covered by the open
        // store. Open files are authoritative; closed-file trees carry the
        // last successfully parsed content from disk.
        let open_urls: HashSet<&Url> = other_open
            .iter()
            .map(|(u, _)| u)
            .chain(std::iter::once(uri))
            .collect();
        let ws = self.workspace.read().await;
        let closed_trees: Vec<(Url, SyntaxTree)> = {
            // Pre-filter by identifier presence: skip files that definitely
            // do not contain `name` as any token. O(1) check per file URL.
            let candidates = ws.files_containing(name);
            ws.trees
                .iter()
                .filter(|(url, _)| !open_urls.contains(url))
                .filter(|(url, _)| candidates.is_some_and(|s| s.contains(url)))
                .map(|(url, tree)| (url.clone(), tree.clone()))
                .collect()
        };

        // Merge: open-buffer trees first (the scope-aware cursor file is
        // handled separately inside collect_references), then closed-file
        // trees. All are scanned with occurrences_of_scoped.
        let all_other_trees: Vec<(Url, SyntaxTree)> =
            other_open.into_iter().chain(closed_trees).collect();

        collect_references(
            name,
            uri,
            cursor_tree,
            cursor_rope,
            target,
            &all_other_trees,
            &ws.index,
            include_declaration,
        )
    }


    /// Re-discover the project from `dir` and re-hydrate the workspace symbol
    /// index from the resulting filelist.
    ///
    /// Fire-and-forget: the workspace index update happens on a spawned task,
    /// mirroring the initialize-time hydration path so the caller returns
    /// promptly. A failed re-discover logs at warn and leaves the previous
    /// project config in place.
    async fn reload_project_from_dir(&self, dir: &Path) {
        match ResolvedProject::discover(dir) {
            Ok(Some(resolved)) => {
                info!(
                    dir = %dir.display(),
                    files = resolved.files.len(),
                    include_dirs = resolved.include_dirs.len(),
                    "project reloaded",
                );
                let paths = resolved.files.clone();
                let include_dirs = resolved.include_dirs.clone();
                let first_project_file = paths.first().cloned();
                self.slang.set_project(Some(resolved)).await;

                // The new config can change anything from the filelist to
                // the diagnostics policy without touching any open buffer,
                // so both elaborate caches are stale: reset the input-hash
                // dedup (or the next schedule would skip the sidecar) and
                // drop the cached AST (so features stop answering from the
                // old project while the recompile is in flight).
                self.elaborate.invalidate_hash().await;
                self.adapter.invalidate().await;

                let ts = self.ts.clone();
                let workspace = self.workspace.clone();
                tokio::spawn(async move {
                    hydrate_workspace_index(paths, include_dirs, ts, workspace).await;
                });

                // Re-elaborate now rather than on the next edit — a
                // config-only change (e.g. demoting vendor diagnostics)
                // must refresh squiggles on its own. Same trigger-URI
                // convention as the startup elaborate: the first filelist
                // entry is just a stable debounce-map key.
                if let Some(first) = first_project_file {
                    if let Ok(trigger) = Url::from_file_path(&first) {
                        debug!(trigger = %trigger, "scheduling post-reload slang elaborate");
                        self.elaborate.schedule(trigger).await;
                    }
                }
            }
            Ok(None) => {
                warn!(
                    dir = %dir.display(),
                    "project reload: no .mimir.toml found; leaving prior config in place",
                );
            }
            Err(e) => {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "project reload: discovery failed; leaving prior config in place",
                );
            }
        }
    }

    /// Re-discover and re-load the project rooted at the directory
    /// containing `mimir_toml_path`. Called when the `.mimir.toml`
    /// itself changes on disk via `workspace/didChangeWatchedFiles`.
    async fn rehydrate_project_from(&self, mimir_toml_path: &Path) {
        let Some(start) = mimir_toml_path.parent() else {
            warn!(path = %mimir_toml_path.display(), "rehydrate: .mimir.toml has no parent dir");
            return;
        };
        self.reload_project_from_dir(start).await;
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
        let include_dirs: Vec<PathBuf> = match self.slang.build_elaborate_params().await {
            Some((params, _)) => params.include_dirs.into_iter().map(PathBuf::from).collect(),
            None => Vec::new(),
        };
        let entries = self.ts.hydrate_paths(&[path.to_path_buf()], &include_dirs).await;
        let mut ws = self.workspace.write().await;
        for (url, syms, tree) in entries {
            debug!(url = %url, count = syms.len(), "re-hydrated single file");
            let names = mimir_syntax::symbols::identifier_names(tree.source());
            ws.update_presence(url.clone(), names);
            ws.index.update(url.clone(), &syms);
            ws.trees.insert(url, tree);
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

        let mut resolved: Option<(Url, Symbol)> = match &receiver {
            Some(mimir_syntax::symbols::HoverReceiver::This) => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, target)?;
                let ws = self.workspace.read().await;
                chain_resolve::find_member(&ws.index, &info.class_name, name)
            }
            Some(mimir_syntax::symbols::HoverReceiver::Super) => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, target)?;
                let parent = info.parent_class_name?;
                let ws = self.workspace.read().await;
                chain_resolve::find_member(&ws.index, &parent, name)
            }
            Some(mimir_syntax::symbols::HoverReceiver::Object(recv_name)) => {
                let ty =
                    mimir_syntax::symbols::find_variable_type_at(tree, rope, target, recv_name)?;
                let cls = mimir_syntax::symbols::normalize_type_name(&ty)?;
                let ws = self.workspace.read().await;
                chain_resolve::find_member(&ws.index, &cls, name)
            }
            None => {
                // Skip bare-identifier lookup when the cursor is on the
                // right-hand side of `::` (class/package scope resolution).
                // The workspace index can't distinguish `get` from
                // `uvm_config_db::get`, so an unrelated match would be wrong.
                if mimir_syntax::symbols::is_scope_qualified_at(tree, rope, target) {
                    return None;
                }
                // Bare identifier: same-file index first, workspace fallback.
                if let Some(sym) = same_file_index.iter().find(|s| s.name == name).cloned() {
                    Some((uri.clone(), sym))
                } else {
                    let ws = self.workspace.read().await;
                    ws.index
                        .lookup(name)
                        .first()
                        .map(|e| (e.url.clone(), e.symbol.clone()))
                }
            }
        };

        // Multi-hop chain fallback (e.g. `a.b.c`, `this.ap.write`).
        // The single-hop arms above only read the first receiver segment; for
        // deeper chains `hover_receiver_at` returns the wrong receiver and the
        // result is None.  Parse the full chain and resolve all hops.
        if resolved.is_none() {
            if let Some(chain) = mimir_syntax::symbols::parse_member_chain_at(tree, rope, target) {
                if chain.target_idx > 0 {
                    let ws = self.workspace.read().await;
                    resolved = chain_resolve::resolve_member_chain(
                        &chain, target, tree, rope, &ws.index,
                    );
                }
            }
        }

        let (sym_url, sym) = resolved?;
        let docs = self.documents.read().await;
        hover_for_symbol(&sym, &sym_url, &docs)
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

        // For member accesses (obj.method), try type-aware chain resolution
        // first. Without this, a raw workspace name-match would fire and return
        // every declaration named e.g. `configure` across the whole project.
        // Chain_resolve uses the receiver type so it can return the single
        // correct declaration.
        if let Some(chain) = mimir_syntax::symbols::parse_member_chain_at(&tree, &rope, target) {
            debug!(
                name,
                segments = chain.segments.len(),
                target_idx = chain.target_idx,
                chain = ?chain.segments,
                "member chain detected at cursor",
            );
            if chain.target_idx > 0 {
                let ws = self.workspace.read().await;
                match chain_resolve::resolve_member_chain(&chain, target, &tree, &rope, &ws.index) {
                    Some((url, sym)) => {
                        debug!(name, resolved_url = %url, "chain definition resolved");
                        return Some(GotoDefinitionResponse::Array(vec![Location {
                            uri: url,
                            range: m_range_to_lsp(sym.name_range),
                        }]));
                    }
                    None => {
                        debug!(
                            name,
                            target_idx = chain.target_idx,
                            "chain definition unresolved — falling through to workspace name lookup",
                        );
                    }
                }
            } else {
                debug!(name, "member chain target_idx == 0 (cursor on root) — skipping chain resolver");
            }
        }

        // Workspace fallback: clone the slice for `name` under the read
        // lock (lock-then-clone) so the resolver runs without holding
        // either lock. Empty slice when there's no workspace match.
        let workspace_hits: Vec<(Url, Symbol)> = {
            let ws = self.workspace.read().await;
            ws.index
                .lookup(name)
                .iter()
                .map(|e| (e.url.clone(), e.symbol.clone()))
                .collect()
        };

        let matches = resolve_definition(name, uri, &index, &workspace_hits);
        if !matches.is_empty() {
            let locations: Vec<Location> = matches
                .into_iter()
                .map(|(url, sym)| Location {
                    uri: url,
                    range: m_range_to_lsp(sym.name_range),
                })
                .collect();
            debug!(name, count = locations.len(), "syntax definition resolved");
            return Some(GotoDefinitionResponse::Array(locations));
        }

        debug!(name, "no symbol matches in same-file or workspace index");
        None
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
            let ws = self.workspace.read().await;
            ws.index
                .entries()
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

        // O(n) top-k selection instead of O(n log n) full sort.
        if scored.len() > MAX_ITEMS {
            scored.select_nth_unstable_by(MAX_ITEMS - 1, |a, b| b.0.cmp(&a.0));
            scored.truncate(MAX_ITEMS);
        }
        scored.sort_by_key(|e| std::cmp::Reverse(e.0));
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
            // For plain-identifier completion, only globally accessible
            // declarations are useful cross-file. Variables, ports, parameters,
            // and methods belong to specific objects and only appear correctly
            // in dot-triggered or AST-aware completion.
            if matches!(
                sym.kind,
                MSymbolKind::Variable
                    | MSymbolKind::Port
                    | MSymbolKind::Method
                    | MSymbolKind::Parameter
                    | MSymbolKind::Constraint
                    | MSymbolKind::EnumMember
            ) {
                continue;
            }
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

        // O(n) top-k selection instead of O(n log n) full sort.
        if scored.len() > MAX_ITEMS {
            scored.select_nth_unstable_by(MAX_ITEMS - 1, |a, b| b.0.cmp(&a.0));
            scored.truncate(MAX_ITEMS);
        }
        scored.sort_by_key(|e| std::cmp::Reverse(e.0));
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
// Incremental re-parse helpers
// --------------------------------------------------------------------------

/// Build a `tree_sitter::InputEdit` for an LSP incremental change.
///
/// Must be called on the **pre-edit** rope so that `start_byte` and
/// `old_end_byte` refer to offsets in the old text, as tree-sitter requires.
/// Returns `None` when the LSP range is out of bounds (we fall back to a
/// full re-parse in that case).
fn make_input_edit(rope: &Rope, range: MRange, new_text: &str) -> Option<InputEdit> {
    let start_byte = range.start.to_byte_offset(rope).ok()?;
    let old_end_byte = range.end.to_byte_offset(rope).ok()?;
    let new_end_byte = start_byte + new_text.len();

    let start_row = range.start.line as usize;
    let start_col = start_byte - rope.line_to_byte(start_row);

    let old_end_row = range.end.line as usize;
    let old_end_col = old_end_byte - rope.line_to_byte(old_end_row);

    // Where does the inserted text end in the new document?
    let newline_count = new_text.bytes().filter(|&b| b == b'\n').count();
    let (new_end_row, new_end_col) = if newline_count == 0 {
        (start_row, start_col + new_text.len())
    } else {
        let last_nl = new_text.rfind('\n').unwrap();
        (start_row + newline_count, new_text.len() - last_nl - 1)
    };

    Some(InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: tree_sitter::Point {
            row: start_row,
            column: start_col,
        },
        old_end_position: tree_sitter::Point {
            row: old_end_row,
            column: old_end_col,
        },
        new_end_position: tree_sitter::Point {
            row: new_end_row,
            column: new_end_col,
        },
    })
}

// --------------------------------------------------------------------------
// Inherent helpers used by the LanguageServer impl that aren't themselves
// LSP requests (a trait impl can only hold trait methods).
// --------------------------------------------------------------------------

impl Backend {
    /// Compute the base hover (symbol declaration / signature / built-in
    /// docs). The `hover` trait method wraps this and appends a macro-
    /// expansion footer when applicable. Kept as a separate method because
    /// the wrapper needs a single result value, while this body has many
    /// early returns across the slang / tree-sitter / builtin fallbacks.
    async fn hover_impl(&self, params: &HoverParams) -> LspResult<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        let Some((tree, index)) = self.cached_tree_and_index(&uri).await else {
            debug!("hover: no cached parse for this URI");
            return Ok(None);
        };
        let rope = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => state.document.rope().clone(),
                None => {
                    debug!("hover: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };

        // AST path: use cached MimirAst for type info and docs.
        if let Some(ast) = self.adapter.cached_ast().await {
            let file_path = crate::paths::uri_to_path_string(&uri);
            if let Some(path) = file_path {
                if let Some(hover) = ast_features::hover(&ast, &path, mimir_pos, &rope) {
                    return Ok(Some(hover));
                }
            }
        }

        if let Some(hover) =
            Box::pin(self.hover_via_tree_sitter(&uri, &tree, &rope, &index, target)).await
        {
            return Ok(Some(hover));
        }

        // Built-in SV method fallback: cursor on a method defined by the LRM
        // (e.g. `push_back`, `rand_mode`, `len`) that the workspace index will
        // never contain. Runs after the workspace lookup so user-defined methods
        // with the same name always win.
        if let Some(hover) = builtin_method_hover_at(&tree, &rope, target) {
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

    /// Handler for the custom `mimir/expandMacro` LSP request (registered in
    /// `main.rs` via `LspService::build(...).custom_method(...)`).
    ///
    /// Recursively expands the macro usage under the cursor and returns its
    /// expanded source text. Returns `Ok(None)` when slang isn't configured,
    /// the cursor isn't on a macro usage, or the sidecar is unavailable —
    /// the VS Code extension turns that into a friendly "not on a macro"
    /// message.
    pub(crate) async fn expand_macro(
        &self,
        params: TextDocumentPositionParams,
    ) -> LspResult<Option<ExpandMacroResponse>> {
        mimir_core::time_scope!("lsp.expand_macro");
        let uri = params.text_document.uri;
        let pos = params.position;
        let target = MPosition::new(pos.line, pos.character);

        let Some(target_path) =
            crate::paths::uri_to_path_string(&uri)
        else {
            return Ok(None);
        };

        let version = {
            let docs = self.documents.read().await;
            docs.get(&uri).map(|s| s.document.version())
        }
        .unwrap_or(i32::MIN);

        let Some(eparams) =
            self.slang.build_expand_macro_params(target_path, target).await
        else {
            return Ok(None); // slang not configured / no project
        };

        match self.adapter.expand_macro(&uri, version, target, &eparams).await {
            Some(r) if r.found => Ok(Some(ExpandMacroResponse {
                name: r.macro_name,
                expansion: r.expanded_text,
                line_count: r.line_count,
            })),
            _ => Ok(None),
        }
    }
}

// --------------------------------------------------------------------------
// LanguageServer impl — wires LSP requests/notifications to our store.
// --------------------------------------------------------------------------

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        mimir_core::time_scope!("lsp.initialize");
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
            // Store the root so `did_change_configuration` can re-discover
            // the project without a `.mimir.toml` path in hand.
            {
                let mut ws = self.workspace.write().await;
                ws.root = Some(start.clone());
            }
            match ResolvedProject::discover(&start) {
                Ok(Some(resolved)) => {
                    // If slang wasn't configured via process env at startup,
                    // check whether the project's [env] table provides
                    // MIMIR_SLANG_PATH and try to spawn from there.
                    if !self.slang.is_configured().await {
                        if let Some(raw) = resolved.env.get(crate::SLANG_PATH_ENV) {
                            // Resolve relative paths against the .mimir.toml
                            // directory so `MIMIR_SLANG_PATH =
                            // "../../slang-sidecar/build/mimir-slang-sidecar"`
                            // works regardless of the server's CWD.
                            let abs_path = {
                                let p = std::path::Path::new(raw);
                                if p.is_absolute() {
                                    p.to_path_buf()
                                } else {
                                    resolved.root.join(p)
                                }
                            };
                            match mimir_slang::Client::spawn(
                                &abs_path,
                                std::iter::empty::<&str>(),
                            )
                            .await
                            {
                                Ok(client) => {
                                    info!(
                                        path = %abs_path.display(),
                                        "slang sidecar spawned from .mimir.toml [env]",
                                    );
                                    self.slang.set_client(Some(Arc::new(client))).await;
                                }
                                Err(e) => {
                                    warn!(
                                        path = %abs_path.display(),
                                        error = %e,
                                        "[SlangError] could not spawn slang sidecar from \
                                         .mimir.toml [env]; continuing without",
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
                    let ts = self.ts.clone();
                    let workspace = self.workspace.clone();
                    // Snapshot the first project file before the move so we
                    // can build a stable startup-elaborate trigger URI.
                    let first_project_file = paths.first().cloned();
                    tokio::spawn(async move {
                        hydrate_workspace_index(paths, include_dirs, ts, workspace).await;
                    });
                    self.slang.set_project(Some(resolved)).await;

                    // Kick off a workspace-wide slang elaborate so semantic
                    // cross-file features (definition, completion,
                    // signatureHelp) are warm before the user opens any
                    // file. It's a no-op when slang isn't configured. The
                    // trigger URI is just a debounce-map key — reuse the
                    // first filelist entry.
                    if let Some(first) = first_project_file {
                        if let Ok(trigger) = Url::from_file_path(&first) {
                            debug!(
                                trigger = %trigger,
                                "scheduling startup slang elaborate",
                            );
                            self.elaborate.schedule(trigger).await;
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
                // textDocument/declaration: slang-first (reuses definition
                // RPC, which resolves to declaration sites) with tree-sitter
                // workspace-index fallback. Prototype-vs-body distinction
                // (`extern function` / `pure virtual`) is a v2 slice.
                declaration_provider: Some(DeclarationCapability::Simple(true)),
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
                // Rename: reuses the same reference engine as
                // `textDocument/references` (scope-aware + workspace-wide),
                // replacing every occurrence with the caller-supplied name.
                // `prepare_rename` validates the cursor is on an identifier
                // before the editor shows the input box.
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
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
                // Smart "expand selection": walk the parse tree from the
                // cursor's leaf node outward. Pure tree-sitter, no symbol
                // table needed.
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                // Clickable `` `include "..." `` paths. `resolve_provider:
                // false` because we fill the target eagerly (the path is
                // cheap to resolve against the project include dirs).
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                // "▷ overrides Base::method" CodeLens. Computed in one stage
                // (the title needs the base class name), so no resolve step.
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
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
                // Call hierarchy: three-step protocol (prepareCallHierarchy +
                // incomingCalls + outgoingCalls). Tree-sitter only — slang
                // doesn't emit call-edge information in MimirAst today.
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                // We publish diagnostics as a *push* (via the `Client`) on
                // every change — we don't yet implement the pull-based
                // `textDocument/diagnostic` request from LSP 3.17.
                //
                // typeHierarchyProvider: lsp-types 0.94.1 (pinned via
                // tower-lsp 0.20) has no `type_hierarchy_provider` field on
                // `ServerCapabilities` — it arrived in lsp-types 0.95. Rather
                // than smuggle it through `experimental` (which VS Code's
                // language client does not read), we advertise the capability
                // via `client/registerCapability` in `initialized`. Move it
                // back to a static field here once tower-lsp/lsp-types is
                // bumped past the gap.
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "mimir-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        mimir_core::time_scope!("lsp.initialized");
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

        // Dynamically register `textDocument/prepareTypeHierarchy`. The static
        // `type_hierarchy_provider` capability field doesn't exist in the
        // pinned lsp-types 0.94.1 (see the comment in `initialize`), and the
        // `experimental` object VS Code ignores — so dynamic registration is
        // the only way to surface the "Show Type Hierarchy" entry without a
        // tower-lsp bump. The document selector scopes it to our languages.
        let type_hierarchy_registration = Registration {
            id: "mimir-type-hierarchy".into(),
            method: "textDocument/prepareTypeHierarchy".into(),
            register_options: serde_json::to_value(TypeHierarchyRegistrationOptions {
                text_document_registration_options: TextDocumentRegistrationOptions {
                    document_selector: Some(vec![
                        DocumentFilter {
                            language: Some("systemverilog".into()),
                            scheme: Some("file".into()),
                            pattern: None,
                        },
                        DocumentFilter {
                            language: Some("verilog".into()),
                            scheme: Some("file".into()),
                            pattern: None,
                        },
                    ]),
                },
                type_hierarchy_options: TypeHierarchyOptions {
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                },
                static_registration_options: StaticRegistrationOptions::default(),
            })
            .ok(),
        };
        if let Err(e) = self.client.register_capability(vec![type_hierarchy_registration]).await {
            warn!(
                error = %e,
                "client refused typeHierarchy registration; \"Show Type Hierarchy\" won't appear until restart",
            );
        }
    }

    async fn shutdown(&self) -> LspResult<()> {
        info!("shutdown requested");
        Ok(())
    }

    async fn did_change_configuration(&self, _params: DidChangeConfigurationParams) {
        // mimir's runtime configuration lives in `.mimir.toml`, not in
        // editor settings. Re-discover the project from the stored workspace
        // root so any `.mimir.toml` changes the editor hasn't already
        // surfaced via `didChangeWatchedFiles` are picked up.
        let root = {
            let ws = self.workspace.read().await;
            ws.root.clone()
        };
        if let Some(dir) = root {
            info!(dir = %dir.display(), "did_change_configuration: re-discovering project");
            self.reload_project_from_dir(&dir).await;
        } else {
            debug!("did_change_configuration: no workspace root stored; skipping");
        }
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        mimir_core::time_scope!("lsp.did_open");
        let TextDocumentItem {
            uri,
            language_id,
            version,
            text,
        } = params.text_document;

        debug!(language_id, version, bytes = text.len(), "did_open");

        // A file moving closed→open means future did_close must re-read it
        // from disk. Drop the entire cache so that path is not served stale.
        self.slang.clear_closed_file_cache().await;

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

        self.reparse_and_publish(uri.clone(), Vec::new()).await;
        self.elaborate.schedule(uri).await;
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        mimir_core::time_scope!("lsp.did_change");
        let uri = params.text_document.uri;
        let new_version = params.text_document.version;

        // Apply each change in order. The LSP spec guarantees changes are
        // sent in document order — earlier ones must be applied before
        // later ones. We use a write lock for the whole batch so partial
        // states aren't observable to a concurrent reparse.
        //
        // We also build `tree_sitter::InputEdit`s so `reparse_and_publish`
        // can hand them to the parser for incremental reuse of unchanged
        // subtrees. A full-sync change (no `range`) resets the edits to
        // empty, signalling a full re-parse.
        let mut edits: Vec<InputEdit> = Vec::new();
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
                        // but be defensive). Reset edits; full re-parse.
                        edits.clear();
                        state.document.replace_all(&change.text, new_version);
                    }
                    Some(range) => {
                        let m_range = MRange::new(
                            MPosition::new(range.start.line, range.start.character),
                            MPosition::new(range.end.line, range.end.character),
                        );
                        // Compute the InputEdit before mutating the rope so
                        // byte offsets still refer to the pre-edit text.
                        if let Some(edit) =
                            make_input_edit(state.document.rope(), m_range, &change.text)
                        {
                            edits.push(edit);
                        } else {
                            // Could not compute edit (out-of-bounds position).
                            // Fall back to full re-parse for safety.
                            edits.clear();
                        }
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
                            edits.clear();
                        }
                    }
                }
            }
        }

        self.reparse_and_publish(uri.clone(), edits).await;
        self.elaborate.schedule(uri).await;
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        mimir_core::time_scope!("lsp.did_close");
        let uri = params.text_document.uri;
        {
            let mut docs = self.documents.write().await;
            docs.remove(&uri);
        }
        // A file moving open→closed becomes disk-authoritative again. The
        // user may have changed it via another editor since we last read it,
        // so drop the entire cache rather than risk serving stale text.
        self.slang.clear_closed_file_cache().await;
        // Closed documents can't be hovered, so their macro-expansion
        // history is dead weight — drop it or the map grows unbounded
        // across a long session.
        self.adapter.evict_expansions(&uri).await;
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
        mimir_core::time_scope!("lsp.did_save");
        let uri = params.text_document.uri;
        debug!("did_save");
        self.elaborate.schedule(uri).await;
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
        mimir_core::time_scope!("lsp.did_change_watched_files");
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
                    let mut ws = self.workspace.write().await;
                    ws.index.update(evt.uri.clone(), &[]);
                    ws.trees.remove(&evt.uri);
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
                        // A closed project file changed on disk — evict its
                        // cached text so the next request re-reads from disk.
                        self.slang.evict_closed_file(&path).await;
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
        mimir_core::time_scope!("lsp.goto_definition");
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // AST path: MimirAst from the last successful compile.
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| s.document.rope().clone())
            };
            let file_path = crate::paths::uri_to_path_string(&uri);
            if let (Some(rope), Some(path)) = (rope, file_path) {
                if let Some(resp) = ast_features::definition(&ast, &path, mimir_pos, &rope) {
                    return Ok(Some(resp));
                }
            }
        }

        // Fall through to tree-sitter workspace index.
        Ok(Box::pin(self.syntax_definition(&uri, target)).await)
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position_params.text_document.uri),
    )]
    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        mimir_core::time_scope!("lsp.goto_declaration");
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // AST path: same as definition — MimirDecl.range points to the name token.
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| s.document.rope().clone())
            };
            let file_path = crate::paths::uri_to_path_string(&uri);
            if let (Some(rope), Some(path)) = (rope, file_path) {
                if let Some(resp) = ast_features::definition(&ast, &path, mimir_pos, &rope) {
                    return Ok(Some(resp));
                }
            }
        }

        // Fall through to tree-sitter workspace index.
        Ok(Box::pin(self.syntax_definition(&uri, target)).await)
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
        mimir_core::time_scope!("lsp.goto_type_definition");
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // AST path: find the type of the symbol and jump to its declaration.
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| s.document.rope().clone())
            };
            let file_path = crate::paths::uri_to_path_string(&uri);
            if let (Some(rope), Some(path)) = (rope, file_path) {
                if let Some(resp) = ast_features::type_definition(&ast, &path, mimir_pos, &rope) {
                    return Ok(Some(resp));
                }
            }
        }

        Ok(None)
    }

    #[instrument(level = "debug", skip_all)]
    async fn goto_implementation(
        &self,
        _params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        mimir_core::time_scope!("lsp.goto_implementation");
        // Implementation lookup is not yet available in the AST-based path.
        Ok(None)
    }

    // ------------------------------------------------------------------
    // callHierarchy/*
    // ------------------------------------------------------------------

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> LspResult<Option<Vec<CallHierarchyItem>>> {
        mimir_core::time_scope!("lsp.prepare_call_hierarchy");
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let mpos = MPosition::new(pos.line, pos.character);

        let Some(tree) = self.syntax.cached_tree(&uri).await else {
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());
        let Some(name) = mimir_syntax::symbols::identifier_at(&tree, &rope, mpos) else {
            return Ok(None);
        };
        let name = name.to_owned();

        // Look up the identifier in the per-file index, then the workspace.
        let item = {
            let docs = self.documents.read().await;
            let per_file_hit = docs.get(&uri).and_then(|s| {
                s.index
                    .iter()
                    .find(|sym| {
                        sym.name == name
                            && matches!(
                                sym.kind,
                                MSymbolKind::Function | MSymbolKind::Task | MSymbolKind::Method
                            )
                    })
                    .map(|sym| {
                        hierarchy_features::call_hierarchy_item(
                            &sym.name, sym.kind, sym.name_range, sym.full_range, &uri,
                        )
                    })
            });
            drop(docs);
            if let Some(item) = per_file_hit {
                Some(item)
            } else {
                let ws = self.workspace.read().await;
                ws.index
                    .lookup(&name)
                    .iter()
                    .find(|e| {
                        matches!(
                            e.symbol.kind,
                            MSymbolKind::Function | MSymbolKind::Task | MSymbolKind::Method
                        )
                    })
                    .map(|e| {
                        hierarchy_features::call_hierarchy_item(
                            &e.symbol.name,
                            e.symbol.kind,
                            e.symbol.name_range,
                            e.symbol.full_range,
                            &e.url,
                        )
                    })
            }
        };
        debug!(name = %name, found = item.is_some(), "prepare_call_hierarchy");
        Ok(item.map(|i| vec![i]))
    }

    #[instrument(level = "debug", skip_all, fields(item = %params.item.name))]
    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> LspResult<Option<Vec<CallHierarchyIncomingCall>>> {
        mimir_core::time_scope!("lsp.incoming_calls");
        let callee_name = params.item.name.clone();

        // Snapshot open-doc trees (excluding the callee's own file where it's
        // declared — but we DO want to scan it for call sites, so include all).
        let open_trees: Vec<(Url, SyntaxTree)> = {
            let docs = self.documents.read().await;
            docs.iter()
                .filter_map(|(u, s)| s.tree.as_ref().map(|t| (u.clone(), t.clone())))
                .collect()
        };
        let open_urls: std::collections::HashSet<Url> =
            open_trees.iter().map(|(u, _)| u.clone()).collect();

        // Closed-file trees from the workspace, pre-filtered by presence.
        let closed_trees: Vec<(Url, SyntaxTree)> = {
            let ws = self.workspace.read().await;
            let candidates = ws.files_containing(&callee_name);
            ws.trees
                .iter()
                .filter(|(url, _)| !open_urls.contains(url))
                .filter(|(url, _)| candidates.is_some_and(|s| s.contains(url)))
                .map(|(url, tree)| (url.clone(), tree.clone()))
                .collect()
        };

        let all_trees: Vec<(Url, SyntaxTree)> =
            open_trees.into_iter().chain(closed_trees).collect();

        let results = {
            let ws = self.workspace.read().await;
            hierarchy_features::collect_incoming_calls(&callee_name, &all_trees, &ws.index)
        };
        debug!(count = results.len(), callee = %callee_name, "incoming_calls");
        Ok(Some(results))
    }

    #[instrument(level = "debug", skip_all, fields(item = %params.item.name))]
    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> LspResult<Option<Vec<CallHierarchyOutgoingCall>>> {
        mimir_core::time_scope!("lsp.outgoing_calls");
        let item = &params.item;
        let uri = &item.uri;

        let Some(tree) = self.syntax.cached_tree(uri).await else {
            return Ok(Some(vec![]));
        };

        let results = {
            let ws = self.workspace.read().await;
            hierarchy_features::collect_outgoing_calls(item.range, &tree, &ws.index)
        };
        debug!(count = results.len(), caller = %item.name, "outgoing_calls");
        Ok(Some(results))
    }

    // ------------------------------------------------------------------
    // typeHierarchy/*
    // ------------------------------------------------------------------

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> LspResult<Option<Vec<TypeHierarchyItem>>> {
        mimir_core::time_scope!("lsp.prepare_type_hierarchy");
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let mpos = MPosition::new(pos.line, pos.character);

        let Some(tree) = self.syntax.cached_tree(&uri).await else {
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());
        let Some(name) = mimir_syntax::symbols::identifier_at(&tree, &rope, mpos) else {
            return Ok(None);
        };
        let name = name.to_owned();

        // Candidate class names to look up, in priority order. The identifier
        // may name a class directly, or it may be an instance/handle whose
        // declared type is the class — resolve that via the AST and try it as
        // a fallback so the type hierarchy can be opened on a concrete object.
        let mut candidates = vec![name.clone()];
        if let Some(ast) = self.adapter.cached_ast().await {
            if let Some(path) =
                crate::paths::uri_to_path_string(&uri)
            {
                let mimir_pos = ast_features::lsp_to_mimir_pos(pos);
                if let Some(type_name) = ast_features::type_name_at(&ast, &path, mimir_pos, &rope) {
                    if type_name != name {
                        candidates.push(type_name);
                    }
                }
            }
        }

        let item = {
            let docs = self.documents.read().await;
            let ws = self.workspace.read().await;
            candidates.iter().find_map(|cand| {
                let per_file_hit = docs.get(&uri).and_then(|s| {
                    s.index
                        .iter()
                        .find(|sym| sym.name == *cand && sym.kind == MSymbolKind::Class)
                        .map(|sym| {
                            hierarchy_features::type_hierarchy_item(
                                &sym.name, sym.name_range, sym.full_range, &uri,
                            )
                        })
                });
                per_file_hit.or_else(|| {
                    ws.index
                        .lookup(cand)
                        .iter()
                        .find(|e| e.symbol.kind == MSymbolKind::Class)
                        .map(|e| {
                            hierarchy_features::type_hierarchy_item(
                                &e.symbol.name,
                                e.symbol.name_range,
                                e.symbol.full_range,
                                &e.url,
                            )
                        })
                })
            })
        };
        debug!(name = %name, found = item.is_some(), "prepare_type_hierarchy");
        Ok(item.map(|i| vec![i]))
    }

    #[instrument(level = "debug", skip_all, fields(item = %params.item.name))]
    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> LspResult<Option<Vec<TypeHierarchyItem>>> {
        mimir_core::time_scope!("lsp.supertypes");
        let class_name = params.item.name.clone();
        let ast = self.adapter.cached_ast().await;
        let results = {
            let ws = self.workspace.read().await;
            hierarchy_features::collect_supertypes(
                &class_name,
                &ws.index,
                ast.as_deref(),
            )
        };
        debug!(count = results.len(), class = %class_name, "supertypes");
        Ok(Some(results))
    }

    #[instrument(level = "debug", skip_all, fields(item = %params.item.name))]
    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> LspResult<Option<Vec<TypeHierarchyItem>>> {
        mimir_core::time_scope!("lsp.subtypes");
        let class_name = params.item.name.clone();
        let results = {
            let ws = self.workspace.read().await;
            hierarchy_features::collect_subtypes(&class_name, &ws.index)
        };
        debug!(count = results.len(), class = %class_name, "subtypes");
        Ok(Some(results))
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
        mimir_core::time_scope!("lsp.document_highlight");
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
    /// 1. **Same file**: scope-aware via `occurrences_of_at`. Two `phase`
    ///    locals in different methods don't bleed into each other; a
    ///    free-standing reference whose declaration isn't visible
    ///    (`super.x`) falls back to whole-file matching, matching what
    ///    `documentHighlight` does.
    /// 2. **Other open buffers**: whole-file lexical match via
    ///    `occurrences_of`. Parse trees for open docs are already cached
    ///    in [`Backend::documents`], so this is essentially free.
    /// 3. **Closed filelist-hydrated files**: the workspace index only
    ///    retains `Symbol` (name + ranges), not parse trees, so we
    ///    contribute *declaration sites only* (`entry.symbol.name_range`).
    ///    Cross-file *usages* in non-open files are a v2 follow-up that
    ///    would require re-parsing on demand.
    ///
    /// Honours [`ReferenceContext::include_declaration`]: when `false`,
    /// declarations identified by the workspace index are filtered out.
    /// Caps total results at `REFERENCES_LIMIT` (in `references_features`)
    /// to keep the editor UI
    /// responsive; logs a `warn!` when truncation kicks in.
    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position.text_document.uri),
    )]
    async fn references(&self, params: ReferenceParams) -> LspResult<Option<Vec<Location>>> {
        mimir_core::time_scope!("lsp.references");
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

        let locations = self
            .collect_workspace_references(
                &uri,
                &cursor_tree,
                &cursor_rope,
                target,
                &name,
                include_declaration,
            )
            .await;
        debug!(count = locations.len(), name = %name, "references returned");
        Ok(Some(locations))
    }

    /// Validate that the cursor is on a renameable identifier and return its
    /// current span. The editor uses the span to pre-fill the rename input.
    ///
    /// Returns `None` (no rename box) when the cursor is on whitespace,
    /// a keyword, punctuation, or outside the document — anything that
    /// isn't a `simple_identifier` or `system_tf_identifier` token.
    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document.uri),
    )]
    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> LspResult<Option<PrepareRenameResponse>> {
        mimir_core::time_scope!("lsp.prepare_rename");
        let uri = params.text_document.uri;
        let pos = params.position;
        let target = MPosition::new(pos.line, pos.character);

        let Some(tree) = self.cached_tree(&uri).await else {
            debug!("prepare_rename: no tree available");
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());
        let Some((_name, range)) =
            mimir_syntax::symbols::identifier_and_range_at(&tree, &rope, target)
        else {
            debug!("prepare_rename: cursor not on identifier");
            return Ok(None);
        };
        debug!(range = ?range, "prepare_rename: identifier found");
        Ok(Some(PrepareRenameResponse::Range(m_range_to_lsp(range))))
    }

    /// Rename the identifier under the cursor to `params.new_name` across
    /// every file in the workspace. Uses the same reference engine as
    /// `textDocument/references` (scope-aware within a file,
    /// workspace-wide across open buffers and filelist-hydrated files).
    ///
    /// Returns a [`WorkspaceEdit`] grouping one [`TextEdit`] per occurrence
    /// per file. The editor applies the edits atomically. Returns `None`
    /// when the cursor is not on an identifier or when no occurrences are
    /// found (e.g. the symbol was never used).
    ///
    /// v1 limitations (matching `references`): tree-sitter only — no
    /// slang-backed scope/type-aware resolution, so `pkg_a::foo` and
    /// `pkg_b::foo` are conflated by name; no hierarchical-name support.
    #[instrument(
        level = "debug",
        skip_all,
        fields(uri = %params.text_document_position.text_document.uri, new_name = %params.new_name),
    )]
    async fn rename(&self, params: RenameParams) -> LspResult<Option<WorkspaceEdit>> {
        mimir_core::time_scope!("lsp.rename");
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let target = MPosition::new(pos.line, pos.character);
        let new_name = params.new_name;

        let Some(cursor_tree) = self.cached_tree(&uri).await else {
            debug!("rename: no tree available");
            return Ok(None);
        };
        let cursor_rope = Rope::from_str(cursor_tree.source());

        let Some(name) =
            mimir_syntax::symbols::identifier_at(&cursor_tree, &cursor_rope, target)
        else {
            debug!("rename: cursor not on identifier");
            return Ok(None);
        };
        let name = name.to_owned();

        let locations = self
            .collect_workspace_references(
                &uri,
                &cursor_tree,
                &cursor_rope,
                target,
                &name,
                true, // always include the declaration site in a rename
            )
            .await;

        if locations.is_empty() {
            debug!(name = %name, "rename: no occurrences found");
            return Ok(None);
        }

        // Group one TextEdit per location, keyed by URL.
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for loc in &locations {
            changes.entry(loc.uri.clone()).or_default().push(TextEdit {
                range: loc.range,
                new_text: new_name.clone(),
            });
        }

        debug!(
            files = changes.len(),
            occurrences = locations.len(),
            name = %name,
            new_name = %new_name,
            "rename: workspace edit prepared",
        );
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
    }

    /// Hover: declaration line for the symbol under the cursor, with a
    /// synthesized signature for callables and the full `define` body
    /// for macros. Receiver-aware for `this.X` / `super.X` / `obj.X` /
    /// multi-hop chains — uses `chain_resolve` for class-member lookup.
    ///
    /// Slang-first when configured: calls `slang.definition` and reads the
    /// declaration line at the resolved location. On transport error or
    /// an empty slang result, falls through to the tree-sitter path —
    /// hover is a UX feature, not a correctness one, so we prefer "show
    /// the textually-first match" to "show nothing" when slang declines
    /// to resolve.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document_position_params.text_document.uri))]
    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        mimir_core::time_scope!("lsp.hover");
        // Base hover (symbol declaration / signature / keyword docs), then a
        // macro-expansion footer when the cursor sits on a `` `macro `` usage
        // and slang can expand it. The footer is gated by a cheap textual
        // check so ordinary hovers never pay for a preprocessor round-trip.
        let base = self.hover_impl(&params).await?;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let uri = params.text_document_position_params.text_document.uri.clone();

        let (rope, version) = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(s) => (s.document.rope().clone(), s.document.version()),
                None => return Ok(base),
            }
        };
        if !cursor_on_macro_usage(&rope, target) {
            return Ok(base);
        }

        // 1. Fresh cache hit — same document version. Costs nothing: skips the
        //    elaborate-param assembly and the sidecar round-trip entirely.
        if let Some(r) = self.adapter.cached_expansion(&uri, version, target).await {
            if r.found {
                return Ok(Some(append_hover_footer(base, macro_footer_markdown(&r, false))));
            }
        }

        let Some(file_path) =
            crate::paths::uri_to_path_string(&uri)
        else {
            return Ok(base);
        };
        let Some(eparams) =
            self.slang.build_expand_macro_params(file_path, target).await
        else {
            return Ok(base); // slang not configured / no project
        };

        // 2. Opportunistic, non-blocking: if a background elaborate is holding
        //    the sidecar connection, this returns `None` rather than stalling
        //    the hover on a multi-second compile (which would leave VS Code
        //    showing "Loading…" until the compile finished). A `Some(found)`
        //    result is rendered fresh; `Some(!found)` means slang says the
        //    cursor isn't on a macro usage, so we show no footer.
        match self.adapter.expand_macro_if_idle(&uri, version, target, &eparams).await {
            Some(r) if r.found => {
                return Ok(Some(append_hover_footer(base, macro_footer_markdown(&r, false))));
            }
            Some(_) => return Ok(base),
            None => {}
        }

        // 3. Sidecar busy/unresponsive: fall back to the last-good expansion for
        //    this usage if we have one, clearly marked stale. Keeps the footer
        //    visible while slang re-elaborates instead of flickering away on
        //    every edit. The base hover already shows the up-to-date `define.
        if let Some(r) = self.adapter.stale_expansion(&uri, target).await {
            if r.found {
                return Ok(Some(append_hover_footer(base, macro_footer_markdown(&r, true))));
            }
        }
        Ok(base)
    }

    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> LspResult<Option<DocumentSymbolResponse>> {
        mimir_core::time_scope!("lsp.document_symbol");
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
    /// up to [`crate::workspace_symbols::WORKSPACE_SYMBOL_LIMIT`], matching the IDE convention of
    /// showing "everything" when the picker first opens.
    #[instrument(level = "debug", skip_all, fields(query = %params.query))]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> LspResult<Option<Vec<SymbolInformation>>> {
        mimir_core::time_scope!("lsp.workspace_symbol");
        let ws = self.workspace.read().await;
        let results = rank_workspace_symbols(&params.query, ws.index.entries());
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
        mimir_core::time_scope!("lsp.folding_range");
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

    /// Smart "expand selection" — for each requested position, return the
    /// chain of nested syntactic ranges (token → expression → statement →
    /// block → … → file) the editor steps through as the user grows the
    /// selection. Pure tree-sitter.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> LspResult<Option<Vec<SelectionRange>>> {
        mimir_core::time_scope!("lsp.selection_range");
        let uri = params.text_document.uri;
        let Some(tree) = self.cached_tree(&uri).await else {
            debug!("selection_range: no tree available");
            return Ok(None);
        };
        let rope = Rope::from_str(tree.source());

        let out: Vec<SelectionRange> = params
            .positions
            .iter()
            .map(|pos| {
                let chain =
                    mimir_syntax::selection::selection_ranges_at(
                        &tree,
                        &rope,
                        MPosition::new(pos.line, pos.character),
                    );
                build_selection_range(&chain)
            })
            .collect();

        debug!(count = out.len(), "selection_range returned");
        Ok(Some(out))
    }

    /// Clickable `` `include "..." `` directives. Scans the document for
    /// include directives, resolves each filename against the file's own
    /// directory then the project include dirs (same order as slang's
    /// preprocessor), and returns a `DocumentLink` per resolved path. Pure
    /// tree-sitter-adjacent text scan — no slang round-trip.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> LspResult<Option<Vec<DocumentLink>>> {
        mimir_core::time_scope!("lsp.document_link");
        let uri = params.text_document.uri;

        let text = {
            let docs = self.documents.read().await;
            match docs.get(&uri) {
                Some(state) => state.document.text(),
                None => {
                    debug!("document_link: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let rope = Rope::from_str(&text);

        // The directory of the current file is searched first.
        let current_dir = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let include_dirs = self.slang.current_include_dirs().await;

        let mut links: Vec<DocumentLink> = Vec::new();
        for span in includes::scan_includes_with_spans(&text) {
            let resolved = includes::resolve_include_with(
                &span.name,
                current_dir.as_deref().unwrap_or_else(|| Path::new(".")),
                &include_dirs,
                |p| p.is_file(),
            );
            let Some(target_path) = resolved else {
                continue; // unresolved (macro path, missing header) → no link
            };
            let Ok(target) = Url::from_file_path(&target_path) else {
                continue;
            };
            let start = MPosition::from_byte_offset(&rope, span.start);
            let end = MPosition::from_byte_offset(&rope, span.end);
            links.push(DocumentLink {
                range: m_range_to_lsp(MRange::new(start, end)),
                target: Some(target),
                tooltip: Some(format!("Open {}", target_path.display())),
                data: None,
            });
        }

        debug!(count = links.len(), "document_link returned");
        Ok(Some(links))
    }

    /// CodeLens: "▷ overrides Base::method" above each method that overrides
    /// an ancestor. Tree-sitter only (no slang). Scope (`uvm` / `all` /
    /// `none`) comes from `[code_lens] overrides`. The lens is computed in
    /// one stage — the title needs the base class name, so there's nothing
    /// useful to defer to `codeLens/resolve`.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn code_lens(&self, params: CodeLensParams) -> LspResult<Option<Vec<CodeLens>>> {
        mimir_core::time_scope!("lsp.code_lens");
        let mode = self.slang.current_code_lens_mode().await;
        if mode == code_lens::OverrideLensMode::None {
            return Ok(None);
        }
        let uri = params.text_document.uri;
        let Some((_, index)) = self.cached_tree_and_index(&uri).await else {
            debug!("code_lens: no cached parse for this URI");
            return Ok(None);
        };
        let lenses = {
            let ws = self.workspace.read().await;
            code_lens::override_lenses(&index, &ws.index, mode)
        };
        debug!(count = lenses.len(), "code_lens returned");
        Ok(Some(lenses))
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
        mimir_core::time_scope!("lsp.semantic_tokens_full");
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
                Some(state) => state.document.rope().clone(),
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
        mimir_core::time_scope!("lsp.semantic_tokens_range");
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
                Some(state) => state.document.rope().clone(),
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
        mimir_core::time_scope!("lsp.signature_help");
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // --- AST path ---
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| s.document.rope().clone())
            };
            let file_path = crate::paths::uri_to_path_string(&uri);
            if let (Some(rope), Some(path)) = (rope, file_path) {
                if let Some(sig) = ast_features::signature_help(&ast, &path, mimir_pos, &rope) {
                    return Ok(Some(sig));
                }
            }
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
                let ws = self.workspace.read().await;
                let found = ws.index.lookup(&call.name).first().map(|e| e.symbol.clone());
                drop(ws);
                found
            }
        };

        // Built-in method fallback: `push_back`, `rand_mode`, `len`, etc.
        // are LRM-defined and will never appear in the workspace index.
        let sym = sym.or_else(|| {
            mimir_syntax::builtin_methods::find_method_by_name(&call.name)
                .map(|m| builtin_to_symbol(m, call.name_range))
        });

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
        mimir_core::time_scope!("lsp.inlay_hint");
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

        // Slang reference map (receiver-aware method resolution). Fetched
        // once for the whole viewport; `None` when no compile has landed or
        // the URI isn't a real file path.
        let cached_ast = self.adapter.cached_ast().await;
        let file_path = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned));

        let call_sites = call_sites_in(&tree, &rope, vp_range);
        debug!(
            calls = call_sites.len(),
            "inlay_hint trace: scanning AST for call sites in viewport",
        );

        let hint_mode = self.slang.current_method_hint_mode().await;

        let ws = self.workspace.read().await;
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

                // Slang-first: the reference map resolves the method name
                // token to its declaration regardless of where in the
                // inheritance chain it lives (including UVM bases the
                // tree-sitter workspace index never sees). Read its params
                // and render hints directly, skipping the tree-sitter
                // resolver entirely on a hit.
                if let (Some(ast), Some(path)) = (&cached_ast, &file_path) {
                    let name_pos = mimir_ast::MimirPos {
                        line: call.name_range.start.line,
                        character: call.name_range.start.character,
                    };
                    if let Some(params) =
                        ast_features::method_params_at(ast, path, name_pos)
                    {
                        let sym = synth_method_symbol(call, params);
                        let labels = hints_for(call, &sym, hint_mode);
                        debug!(
                            name = %call.name,
                            receiver = recv,
                            via = "slang-refmap",
                            sym_params = sym.params.as_ref().map(|p| p.len()).unwrap_or(0),
                            call_args = call.args.len(),
                            labels = labels.len(),
                            "inlay_hint trace: method resolved",
                        );
                        hints.extend(labels.into_iter().map(param_inlay_hint));
                        continue;
                    }
                }

                let resolved = resolve_method_symbol(call, recv, &tree, &rope, &index, &ws.index);
                match resolved {
                    MethodResolution::Resolved(sym, source_label) => {
                        let labels = hints_for(call, &sym, hint_mode);
                        debug!(
                            name = %call.name,
                            receiver = recv,
                            via = source_label,
                            sym_params = sym.params.as_ref().map(|p| p.len()).unwrap_or(0),
                            call_args = call.args.len(),
                            labels = labels.len(),
                            "inlay_hint trace: method resolved",
                        );
                        hints.extend(labels.into_iter().map(param_inlay_hint));
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
                ws.index.lookup(&call.name).first().map(|e| e.symbol.clone())
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

            hints.extend(hints_for(call, &sym, hint_mode).into_iter().map(param_inlay_hint));
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
        mimir_core::time_scope!("lsp.completion");
        // All return paths funnel through `into_incomplete` so every response
        // is `isIncomplete: true` — see that helper for why.
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // Read the document text once; used by all paths below.
        let text = {
            let docs = self.documents.read().await;
            docs.get(&uri).map(|s| s.document.text())
        };
        let rope = text.as_deref().map(Rope::from_str);

        // Route 1: `` ` `` trigger — macro-name completion.
        if let Some(rope) = &rope {
            if let Some(macro_prefix) = detect_macro_trigger(rope, target) {
                return Ok(Box::pin(self.syntax_macro_completion(&uri, &macro_prefix))
                    .await
                    .map(into_incomplete));
            }
        }

        // Route 2: `.` or `::` trigger — member / package-scope completion.
        let member_trigger = rope
            .as_ref()
            .and_then(|r| detect_member_access(r, target));
        let has_member_trigger = member_trigger.is_some();

        if has_member_trigger {
            // AST path: use MimirAst member/package completion.
            if let (Some(ast), Some(_rope), Some((is_pkg, receiver))) = (
                self.adapter.cached_ast().await,
                rope.as_ref(),
                member_trigger.as_ref().cloned(),
            ) {
                let file_path = crate::paths::uri_to_path_string(&uri);
                if let Some(path) = file_path {
                    if let Some(resp) = ast_features::member_completion(
                        &ast, &path, mimir_pos, &receiver, is_pkg,
                    ) {
                        return Ok(Some(into_incomplete(resp)));
                    }
                }
            }

            // Tree-sitter fallback for `.` (not `::`).
            let is_dot_trigger = member_trigger.as_ref().is_some_and(|(is_pkg, _)| !is_pkg);
            if is_dot_trigger {
                if let (Some(tree), Some(r)) = (self.cached_tree(&uri).await, rope.as_ref()) {
                    let prefix = member_trigger.map(|(_, p)| p).unwrap_or_default();
                    let ws = self.workspace.read().await;
                    if let Some(resp) = syntax_member_completion(&ws.index, &tree, r, target, &prefix) {
                        return Ok(Some(into_incomplete(resp)));
                    }
                }
            }
            return Ok(Some(into_incomplete(CompletionResponse::Array(vec![]))));
        }

        // Route 3: plain identifier — scope-aware completion.
        // AST path first (scope-correct local scope from cached symbol table),
        // then always augment with workspace global-scope declarations (classes,
        // modules, packages, typedefs, top-level functions/tasks) so that
        // cross-file types are always reachable even when slang has only
        // resolved the current file's local scope.
        if let Some(ast) = self.adapter.cached_ast().await {
            let file_path = crate::paths::uri_to_path_string(&uri);
            if let Some(path) = file_path {
                let mut items = ast_features::identifier_completion(&ast, &path, mimir_pos);
                if !items.is_empty() {
                    let seen: std::collections::HashSet<String> =
                        items.iter().map(|i| i.label.clone()).collect();
                    let ws = self.workspace.read().await;
                    for entry in ws.index.entries() {
                        if matches!(
                            entry.symbol.kind,
                            MSymbolKind::Class
                                | MSymbolKind::Module
                                | MSymbolKind::Interface
                                | MSymbolKind::Package
                                | MSymbolKind::Typedef
                                | MSymbolKind::Function
                                | MSymbolKind::Task
                                | MSymbolKind::Program
                        ) && !seen.contains(&entry.symbol.name)
                        {
                            let detail = entry
                                .url
                                .path_segments()
                                .and_then(|mut s| s.next_back())
                                .map(str::to_owned);
                            items.push(CompletionItem {
                                label: entry.symbol.name.clone(),
                                kind: Some(symbol_kind_to_completion_kind(entry.symbol.kind)),
                                detail,
                                data: make_resolve_data(
                                    &entry.url,
                                    entry.symbol.name_range.start.line,
                                ),
                                ..Default::default()
                            });
                        }
                    }
                    // Also add SV keywords so the AST-augmented path is
                    // complete (syntax_completion isn't reached when AST
                    // returns non-empty results).
                    for kw in mimir_syntax::keywords::KEYWORDS.iter().copied() {
                        if !seen.contains(kw) {
                            items.push(keyword_completion_item(kw));
                        }
                    }
                    return Ok(Some(into_incomplete(CompletionResponse::Array(items))));
                }
            }
        }

        Ok(Box::pin(self.syntax_completion(&uri, target)).await.map(into_incomplete))
    }

    /// Whole-file formatting via `verible-verilog-format`. Wraps
    /// `` `ifdef `` blocks in format-off pragmas first (when enabled) so
    /// Verible can parse the rest, strips them from the output, and
    /// returns a single whole-document edit. Returns `None` when the
    /// feature is toggled off, the document isn't open, or Verible fails.
    #[instrument(level = "debug", skip_all, fields(uri = %params.text_document.uri))]
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> LspResult<Option<Vec<TextEdit>>> {
        mimir_core::time_scope!("lsp.formatting");
        let features = self.current_features().await;
        if !features.formatting {
            debug!("formatting: disabled by feature toggle");
            return Ok(None);
        }
        let cfg = self.current_formatter_config().await;
        let (source, rope) = {
            let docs = self.documents.read().await;
            match docs.get(&params.text_document.uri) {
                Some(state) => (state.document.text(), state.document.rope().clone()),
                None => {
                    debug!("formatting: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let wrapped = if cfg.wrap_ifdefs {
            let w = wrap_ifdefs(&source);
            if w.has_ifdefs {
                warn!(
                    "formatting: file contains `ifdef/`ifndef blocks; \
                     wrapping them with format-off pragmas so Verible can parse the rest"
                );
            }
            w
        } else {
            WrappedSource::unchanged(&source)
        };
        let has_ifdefs = wrapped.has_ifdefs;
        match invoke_verible(&cfg, &wrapped.text, None).await {
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
        mimir_core::time_scope!("lsp.range_formatting");
        let features = self.current_features().await;
        if !features.formatting {
            debug!("range_formatting: disabled by feature toggle");
            return Ok(None);
        }
        let cfg = self.current_formatter_config().await;
        let (source, rope) = {
            let docs = self.documents.read().await;
            match docs.get(&params.text_document.uri) {
                Some(state) => (state.document.text(), state.document.rope().clone()),
                None => {
                    debug!("range_formatting: URI not in open-doc store");
                    return Ok(None);
                }
            }
        };
        let wrapped = if cfg.wrap_ifdefs {
            let w = wrap_ifdefs(&source);
            if w.has_ifdefs {
                warn!(
                    "range_formatting: file contains `ifdef/`ifndef blocks; \
                     wrapping them with format-off pragmas"
                );
            }
            w
        } else {
            WrappedSource::unchanged(&source)
        };
        let has_ifdefs = wrapped.has_ifdefs;
        // Verible's `--lines` flag is 1-based and references the text it is
        // given — the *wrapped* text — so the selection's 0-based LSP lines
        // must first be shifted past any pragma lines inserted above them.
        let start_line = wrapped.wrapped_line(params.range.start.line) + 1;
        let end_line = wrapped.wrapped_line(params.range.end.line) + 1;
        match invoke_verible(&cfg, &wrapped.text, Some(start_line..=end_line)).await {
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
                // Return an edit confined to the requested range.
                // Verible emits the whole file; we extract only the lines
                // that changed within [lsp_start_line, lsp_end_line].
                let lsp_start = params.range.start.line;
                let lsp_end = params.range.end.line;
                Ok(range_lines_edit(&rope, &formatted, lsp_start, lsp_end)
                    .or_else(|| Some(vec![])))
            }
            Err(e) => {
                error!(error = %e, "verible-verilog-format failed; returning no edits");
                Ok(None)
            }
        }
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
        mimir_core::time_scope!("lsp.completion_resolve");
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
                read_line_trimmed(state.document.rope(), resolve.line)
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

/// Eager-hydrate the workspace index from a project filelist.
///
/// Spawned from `initialize` once `.mimir.toml` has resolved. Delegates
/// the parse work to [`TreeSitterProvider::hydrate_paths`] and folds the
/// resulting symbols into the workspace index under one write-lock
/// transaction at the end. Marks the index hydrated regardless of how
/// many files actually parsed — a partial result still beats a cold
/// index, and `is_hydrated` is just a "did we attempt this once" flag.
async fn hydrate_workspace_index(
    paths: Vec<PathBuf>,
    include_dirs: Vec<PathBuf>,
    ts: Arc<TreeSitterProvider>,
    workspace: Arc<RwLock<WorkspaceState>>,
) {
    let count_requested = paths.len();
    let entries = ts.hydrate_paths(&paths, &include_dirs).await;

    let parsed = entries.len();
    {
        let mut ws = workspace.write().await;
        for (url, syms, tree) in entries {
            let names = mimir_syntax::symbols::identifier_names(tree.source());
            ws.update_presence(url.clone(), names);
            ws.index.update(url.clone(), &syms);
            ws.trees.insert(url, tree);
        }
    }
    info!(
        files = parsed,
        requested = count_requested,
        "workspace index hydrated",
    );
}









//
// These tests cover the *pure-logic* helpers. The full `Backend` requires
// a `tower_lsp::Client`, which only `LspService::new` can mint — so an
// end-to-end test would have to spawn the server and do JSON-RPC. That's
// a follow-up; here we just exercise what we can in isolation.

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a `Symbol` of the given name + kind. Range values
    /// are arbitrary — the tests only care about identity.
    fn sym(name: &str, kind: MSymbolKind, line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 1)),
            full_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 10)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        }
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
            let mut ws = backend.workspace.write().await;
            ws.index.update(
                url.clone(),
                &[sym("my_class", MSymbolKind::Class, 0)],
            );
        }
        assert!(
            !backend.workspace.read().await.index.lookup("my_class").is_empty(),
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
            backend.workspace.read().await.index.lookup("my_class").is_empty(),
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
        assert_eq!(backend.workspace.read().await.index.entries().count(), 0);
    }

    /// `rename` must advertise prepare support in `initialize`.
    #[tokio::test]
    async fn initialize_advertises_rename_with_prepare() {
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
        match result.capabilities.rename_provider {
            Some(OneOf::Right(opts)) => {
                assert_eq!(opts.prepare_provider, Some(true));
            }
            other => panic!("expected RenameOptions with prepare_provider, got {other:?}"),
        }
    }
}
