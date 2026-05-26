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
    Diagnostic as MDiagnostic, Symbol, SymbolKind as MSymbolKind,
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
use crate::completion_score;
use crate::hierarchy_features;
use crate::elaborate_service::ElaborateService;
use crate::format::{invoke_verible, strip_mimir_pragmas, wrap_ifdefs};
use crate::parse_provider::TreeSitterProvider;
use crate::project::{FeatureToggles, FormatterConfig, ResolvedProject};
use crate::slang_adapter::SlangAdapter;
use crate::slang_service::{SlangService, detect_member_access, detect_macro_trigger, m_range_to_lsp};
use crate::syntax_service::{SyntaxCandidates, SyntaxService};
use crate::workspace_index::{self, WorkspaceIndex, WorkspaceState};

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

    /// Drives the `compile` RPC and caches the resulting [`MimirAst`].
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
            syntax: SyntaxService::new(
                documents.clone(),
                ts.clone(),
                workspace.clone(),
            ),
            elaborate: ElaborateService::new(adapter.clone(), client.clone()),
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
        let parse_result = self.ts.parse(&text, &edits, prior_tree).await;
        let (diags, new_state) = match parse_result {
            Some(r) => (r.diagnostics, Some((r.symbols, r.tree))),
            None => (Vec::new(), None),
        };

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
                self.slang.set_project(Some(resolved)).await;
                let ts = self.ts.clone();
                let workspace = self.workspace.clone();
                tokio::spawn(async move {
                    hydrate_workspace_index(paths, include_dirs, ts, workspace).await;
                });
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

        // Multi-hop member chain fallback (e.g. `a.b.c`, `this.ap.write`).
        // `resolve_definition` handles bare names and single-hop via workspace
        // index; deeper chains need the chain resolver.
        if let Some(chain) = mimir_syntax::symbols::parse_member_chain_at(&tree, &rope, target) {
            if chain.target_idx > 0 {
                let ws = self.workspace.read().await;
                if let Some((url, sym)) =
                    chain_resolve::resolve_member_chain(&chain, target, &tree, &rope, &ws.index)
                {
                    debug!(name, "chain definition resolved");
                    return Some(GotoDefinitionResponse::Array(vec![Location {
                        uri: url,
                        range: m_range_to_lsp(sym.name_range),
                    }]));
                }
            }
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
                // typeHierarchyProvider: lsp-types 0.95.1 omits this field (LSP
                // 3.17 gap). Placed in `experimental` as a workaround; VS Code
                // reads `typeHierarchyProvider` from the top-level capabilities
                // object and will not see the value here until lsp-types gains
                // the typed field — move it out of experimental at that point.
                experimental: Some(serde_json::json!({"typeHierarchyProvider": true})),
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
        let uri = params.text_document.uri;
        {
            let mut docs = self.documents.write().await;
            docs.remove(&uri);
        }
        // A file moving open→closed becomes disk-authoritative again. The
        // user may have changed it via another editor since we last read it,
        // so drop the entire cache rather than risk serving stale text.
        self.slang.clear_closed_file_cache().await;
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
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // AST path: MimirAst from the last successful compile.
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| Rope::from_str(&s.document.text()))
            };
            let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
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
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let target = MPosition::new(pos.line, pos.character);
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // AST path: same as definition — MimirDecl.range points to the name token.
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| Rope::from_str(&s.document.text()))
            };
            let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
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
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // AST path: find the type of the symbol and jump to its declaration.
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| Rope::from_str(&s.document.text()))
            };
            let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
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

        let item = {
            let docs = self.documents.read().await;
            let per_file_hit = docs.get(&uri).and_then(|s| {
                s.index
                    .iter()
                    .find(|sym| sym.name == name && sym.kind == MSymbolKind::Class)
                    .map(|sym| {
                        hierarchy_features::type_hierarchy_item(
                            &sym.name, sym.name_range, sym.full_range, &uri,
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
                    .find(|e| e.symbol.kind == MSymbolKind::Class)
                    .map(|e| {
                        hierarchy_features::type_hierarchy_item(
                            &e.symbol.name,
                            e.symbol.name_range,
                            e.symbol.full_range,
                            &e.url,
                        )
                    })
            }
        };
        debug!(name = %name, found = item.is_some(), "prepare_type_hierarchy");
        Ok(item.map(|i| vec![i]))
    }

    #[instrument(level = "debug", skip_all, fields(item = %params.item.name))]
    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> LspResult<Option<Vec<TypeHierarchyItem>>> {
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

        // Collect closed-file trees from the workspace tree cache: all
        // URLs present in the cache that aren't the cursor file and aren't
        // already covered by the open-document store. Open files are
        // authoritative; closed-file trees carry the last successfully
        // parsed content from disk.
        let open_urls: HashSet<&Url> = other_open
            .iter()
            .map(|(u, _)| u)
            .chain(std::iter::once(&uri))
            .collect();
        let closed_trees: Vec<(Url, SyntaxTree)> = {
            let ws = self.workspace.read().await;
            // Pre-filter by identifier presence: skip files that definitely
            // do not contain `name` as any token. O(1) check per file URL.
            let candidates = ws.files_containing(&name);
            ws.trees
                .iter()
                .filter(|(url, _)| !open_urls.contains(url))
                .filter(|(url, _)| candidates.is_some_and(|s| s.contains(url)))
                .map(|(url, tree)| (url.clone(), tree.clone()))
                .collect()
        };

        // Merge: open-buffer trees first (scope-aware cursor file is
        // handled separately inside collect_references), then closed-file
        // trees. All are scanned with occurrences_of_scoped.
        let all_other_trees: Vec<(Url, SyntaxTree)> =
            other_open.into_iter().chain(closed_trees).collect();

        let ws = self.workspace.read().await;
        let locations = collect_references(
            &name,
            &uri,
            &cursor_tree,
            &cursor_rope,
            target,
            &all_other_trees,
            &ws.index,
            include_declaration,
        );
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

        // Snapshot open-buffer trees (excluding cursor file) under one lock,
        // then drop the lock before acquiring the workspace lock.
        let other_open: Vec<(Url, SyntaxTree)> = {
            let docs = self.documents.read().await;
            docs.iter()
                .filter(|(other_uri, _)| **other_uri != uri)
                .filter_map(|(other_uri, state)| {
                    state.tree.as_ref().map(|t| (other_uri.clone(), t.clone()))
                })
                .collect()
        };

        // Collect closed-file trees from the workspace tree cache, pre-filtered
        // by identifier presence (same pattern as `references`).
        let open_urls: HashSet<&Url> = other_open
            .iter()
            .map(|(u, _)| u)
            .chain(std::iter::once(&uri))
            .collect();
        let closed_trees: Vec<(Url, SyntaxTree)> = {
            let ws = self.workspace.read().await;
            let candidates = ws.files_containing(&name);
            ws.trees
                .iter()
                .filter(|(url, _)| !open_urls.contains(url))
                .filter(|(url, _)| candidates.is_some_and(|s| s.contains(url)))
                .map(|(url, tree)| (url.clone(), tree.clone()))
                .collect()
        };

        let all_other_trees: Vec<(Url, SyntaxTree)> =
            other_open.into_iter().chain(closed_trees).collect();

        let ws = self.workspace.read().await;
        let locations = collect_references(
            &name,
            &uri,
            &cursor_tree,
            &cursor_rope,
            target,
            &all_other_trees,
            &ws.index,
            true, // always include the declaration site in a rename
        );
        drop(ws);

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
    /// Slang-first when configured: routes through
    /// Slang-first when configured: calls `slang.definition` and reads the
    /// declaration line at the resolved location. On transport error or
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
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

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

        // AST path: use cached MimirAst for type info and docs.
        if let Some(ast) = self.adapter.cached_ast().await {
            let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
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
        let mimir_pos = ast_features::lsp_to_mimir_pos(pos);

        // --- AST path ---
        if let Some(ast) = self.adapter.cached_ast().await {
            let rope = {
                let docs = self.documents.read().await;
                docs.get(&uri).map(|s| Rope::from_str(&s.document.text()))
            };
            let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
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

            for label in hints_for(call, &sym, hint_mode) {
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
                return Ok(Box::pin(self.syntax_macro_completion(&uri, &macro_prefix)).await);
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
                let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
                if let Some(path) = file_path {
                    if let Some(resp) = ast_features::member_completion(
                        &ast, &path, mimir_pos, &receiver, is_pkg,
                    ) {
                        return Ok(Some(resp));
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
                        return Ok(Some(resp));
                    }
                }
            }
            return Ok(Some(CompletionResponse::Array(vec![])));
        }

        // Route 3: plain identifier — scope-aware completion.
        // AST path first (scope-correct local scope from cached symbol table),
        // then always augment with workspace global-scope declarations (classes,
        // modules, packages, typedefs, top-level functions/tasks) so that
        // cross-file types are always reachable even when slang has only
        // resolved the current file's local scope.
        if let Some(ast) = self.adapter.cached_ast().await {
            let file_path = uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned));
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
                    return Ok(Some(CompletionResponse::Array(items)));
                }
            }
        }

        Ok(Box::pin(self.syntax_completion(&uri, target)).await)
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

/// Build a [`TextEdit`] that replaces only lines `lsp_start..=lsp_end`
/// (0-based, inclusive) in the document with the corresponding lines from
/// `formatted_text` (the full Verible output).
///
/// Used by `range_formatting` so that the returned edit is confined to the
/// requested range — clients validate that edits don't escape the viewport.
///
/// Returns `None` when the two snippets are identical (no change needed).
fn range_lines_edit(
    original: &ropey::Rope,
    formatted_text: &str,
    lsp_start: u32,
    lsp_end: u32,
) -> Option<Vec<TextEdit>> {
    let fmt_rope = ropey::Rope::from_str(formatted_text);

    let orig_lines = original.len_lines();
    let fmt_lines = fmt_rope.len_lines();

    let s = lsp_start as usize;
    // `lsp_end` is inclusive in the LSP range; the edit covers through the
    // *start* of line lsp_end+1 so that the trailing newline is included.
    let e = (lsp_end as usize + 1).min(orig_lines).min(fmt_lines);

    let orig_start_byte = original.line_to_byte(s);
    let orig_end_byte = original.line_to_byte(e.min(orig_lines));
    let fmt_start_byte = fmt_rope.line_to_byte(s.min(fmt_lines));
    let fmt_end_byte = fmt_rope.line_to_byte(e.min(fmt_lines));

    let orig_slice = &original.to_string()[orig_start_byte..orig_end_byte];
    let fmt_slice = &formatted_text[fmt_start_byte..fmt_end_byte];

    if orig_slice == fmt_slice {
        return None;
    }

    // End the edit at the last character of lsp_end (not the start of
    // lsp_end+1) so that the edit range stays within the requested viewport.
    // Use the formatted line's length in UTF-16 code units (the LSP wire
    // format) to handle non-ASCII identifiers correctly.
    let end_line_content = fmt_rope
        .line(e.saturating_sub(1).min(fmt_lines.saturating_sub(1)));
    let end_char: u32 = end_line_content
        .chars()
        .filter(|&c| c != '\n' && c != '\r')
        .map(|c| c.len_utf16() as u32)
        .sum();

    Some(vec![TextEdit {
        range: Range {
            start: Position { line: lsp_start, character: 0 },
            end: Position { line: lsp_end, character: end_char },
        },
        new_text: fmt_slice.to_owned(),
    }])
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
///     [`chain_resolve::find_method_in_class`].
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
            chain_resolve::find_method_in_class(wi, &parent, &call.name)
                .map(|(_, s)| MethodResolution::Resolved(s, "super/inheritance walk"))
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
            chain_resolve::find_method_in_class(wi, &cls, "new")
                .map(|(_, s)| MethodResolution::Resolved(s, "class_new/LHS-type"))
                .unwrap_or(MethodResolution::NotResolved(
                    "constructor not found for resolved class",
                ))
        }
        _ => {
            // `obj.method` or `a.b.method` style. `recv` is the whole
            // hierarchical_identifier including the method name. Strip the
            // trailing segment to get the receiver chain, then build a
            // MemberChain and resolve with the chain resolver (supports up to
            // 2 intermediate hops on the tree-sitter path).
            let receiver_chain = match recv.rsplit_once('.') {
                Some((before, _method)) => before,
                None => recv,
            };
            let chain = chain_resolve::build_chain_for_receiver(receiver_chain, &call.name);
            if let Some((_, sym)) = chain_resolve::resolve_member_chain(
                &chain, call.name_range.start, tree, rope, wi,
            ) {
                return MethodResolution::Resolved(sym, "obj.method/chain");
            }
            // Single-segment receiver fast path for built-in methods.
            let cls_opt = if !receiver_chain.contains('.') {
                find_variable_type_at(tree, rope, call.name_range.start, receiver_chain)
                    .as_deref()
                    .and_then(normalize_type_name)
                    .map(|s| s.to_string())
            } else {
                None
            };
            if let Some(cls) = cls_opt {
                if let Some(m) = mimir_syntax::builtin_methods::find_method(&cls, &call.name)
                    .or_else(|| mimir_syntax::builtin_methods::find_universal(&call.name))
                {
                    return MethodResolution::Resolved(
                        builtin_to_symbol(m, call.name_range),
                        "obj.method/builtin",
                    );
                }
            }
            MethodResolution::NotResolved(
                "method not found in resolved receiver class",
            )
        }
    }
}

/// Synthesise a [`Symbol`] from a [`mimir_syntax::builtin_methods::BuiltinMethod`]
/// so it can be passed to [`hints_for`] and [`signature_for`].
///
/// The `name_range` is taken from the call site so the symbol has a
/// plausible source location; `full_range` matches it (we have no
/// declaration site for built-ins).
fn builtin_to_symbol(
    m: &mimir_syntax::builtin_methods::BuiltinMethod,
    range: mimir_core::Range,
) -> Symbol {
    Symbol {
        name: m.name.to_owned(),
        kind: mimir_syntax::SymbolKind::Method,
        name_range: range,
        full_range: range,
        params: Some(
            m.params
                .iter()
                .map(|p| mimir_syntax::symbols::Param {
                    name: p.name.to_owned(),
                    ty: p.ty.map(str::to_owned),
                })
                .collect(),
        ),
        parent_class_name: None,
        return_type: None,
        decl_type: None,
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
// --------------------------------------------------------------------------
// Syntax-only member completion (AST fallback for `super.` / `this.` / `obj.` / chains)
// --------------------------------------------------------------------------

/// Extract the plain identifier immediately before the `.` trigger.
///
/// Given `super.` at cursor, returns `"super"`. Given `obj.fo` at cursor
/// (partial prefix), returns `"obj"`. Returns `None` when the trigger is
/// `::` (package scope) or when nothing plain sits left of the dot (e.g. a
/// closing `)` from a chained call like `get_obj().`).
///
/// Single-segment variant kept for tests; [`receiver_chain_before_dot`] is
/// the production path used by [`syntax_member_completion`].
#[cfg(test)]
fn receiver_ident_before_dot(rope: &Rope, pos: MPosition) -> Option<String> {
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

    // Strip the completion prefix (e.g. "fo" in "obj.fo").
    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$') {
        i -= 1;
    }

    // Only handle `.` — not `::`.
    if i == 0 || chars[i - 1] != '.' {
        return None;
    }
    i -= 1; // skip the `.`

    // Read the receiver identifier backwards.
    let end = i;
    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$') {
        i -= 1;
    }
    if i == end {
        return None;
    }
    Some(chars[i..end].iter().collect())
}

/// Extend [`receiver_ident_before_dot`] to handle multi-hop chains.
///
/// For `a.b.` at cursor returns `["a", "b"]`; for `obj.` returns `["obj"]`.
/// Returns `None` when the trigger is `::`, or when a non-identifier character
/// (e.g. `)` from a call return) sits left of the dot.
fn receiver_chain_before_dot(rope: &Rope, pos: MPosition) -> Option<Vec<String>> {
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

    // Strip the completion prefix.
    while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$') {
        i -= 1;
    }

    // Only handle `.` — not `::`.
    if i == 0 || chars[i - 1] != '.' {
        return None;
    }
    i -= 1; // skip the `.`

    // Read segments backwards, stopping at a non-identifier non-dot char.
    let mut segments: Vec<String> = Vec::new();
    loop {
        let end = i;
        while i > 0 && matches!(chars[i - 1], 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$') {
            i -= 1;
        }
        if i == end {
            return None; // non-identifier char (e.g. `)`) — bail
        }
        segments.push(chars[i..end].iter().collect());
        if i > 0 && chars[i - 1] == '.' {
            i -= 1; // consume the `.` and read the next segment
        } else {
            break;
        }
    }
    segments.reverse();
    Some(segments)
}

/// Enumerate all member symbols declared in `class_name` and its ancestors
/// (via `Symbol::parent_class_name`). Capped at 16 hops to guard against
/// cycles in malformed code.
///
/// Closest-ancestor wins: if a subclass overrides a parent's method, only
/// the subclass version is included (matching SV override semantics).
///
/// Returns `(Symbol, Url)` pairs so callers can show the declaring file name
/// in the completion item's `detail` field.
fn collect_class_members(
    wi: &WorkspaceIndex,
    class_name: &str,
) -> Vec<(Symbol, Url)> {
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut result: Vec<(Symbol, Url)> = Vec::new();
    let mut current = class_name.to_string();
    let mut visited: HashSet<String> = HashSet::new();

    for _ in 0..16 {
        if !visited.insert(current.clone()) {
            break;
        }
        let Some(class_entry) = wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == MSymbolKind::Class)
            .cloned()
        else {
            break;
        };
        let class_url = class_entry.url.clone();
        let class_range = class_entry.symbol.full_range;

        for e in wi.entries() {
            if e.url != class_url {
                continue;
            }
            if e.symbol.kind == MSymbolKind::Class {
                continue;
            }
            if !range_contains(class_range, e.symbol.full_range) {
                continue;
            }
            if seen_names.insert(e.symbol.name.clone()) {
                result.push((e.symbol.clone(), class_url.clone()));
            }
        }

        match class_entry.symbol.parent_class_name {
            Some(parent) => current = parent,
            None => break,
        }
    }
    result
}

/// Best-effort member completion backed by the cached AST and workspace
/// index. Used when slang is unavailable or busy with a background elaborate.
///
/// Handles single-hop and multi-hop receiver chains:
/// - `super` → members of the parent class (from `extends` on the enclosing class)
/// - `this`  → members of the enclosing class and its ancestors
/// - `<ident>` → resolves the identifier's declared type, then enumerates members
/// - `a.b.` → resolves `a` to a type, then `b` to a member type, then enumerates
///   members of that type (up to 2 intermediate hops on the tree-sitter path)
///
/// Returns `None` when the receiver's type cannot be determined from syntax
/// alone (e.g. undeclared variable, deeper chain). This avoids the workspace-
/// dump anti-pattern — no irrelevant candidates are ever returned.
fn syntax_member_completion(
    wi: &WorkspaceIndex,
    tree: &SyntaxTree,
    rope: &Rope,
    pos: MPosition,
    prefix: &str,
) -> Option<CompletionResponse> {
    let segments = receiver_chain_before_dot(rope, pos)?;

    // `dim_suffix` carries `"[$]"`, `"[]"`, or `"[K]"` when the receiver is a
    // queue / dynamic array / associative array so we can append the right
    // built-in table after workspace members.
    let mut dim_suffix: Option<String> = None;

    let class_name: String = if segments.len() == 1 {
        match segments[0].as_str() {
            "super" => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?;
                info.parent_class_name?
            }
            "this" => {
                let info = mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?;
                info.class_name
            }
            ident => {
                let type_info =
                    mimir_syntax::symbols::find_variable_type_info_at(tree, rope, pos, ident)?;
                dim_suffix = type_info.suffix.clone();
                mimir_syntax::symbols::normalize_type_name(&type_info.base)?
            }
        }
    } else {
        // Multi-hop: walk the receiver segments manually to find the type at
        // the end of the chain, then enumerate that type's members.
        let root_name = &segments[0];
        let root_type = match root_name.as_str() {
            "this" => mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?.class_name,
            "super" => {
                mimir_syntax::symbols::enclosing_class_info_at(tree, rope, pos)?.parent_class_name?
            }
            _ => {
                let raw =
                    mimir_syntax::symbols::find_variable_type_at(tree, rope, pos, root_name)?;
                mimir_syntax::symbols::normalize_type_name(&raw)?
            }
        };
        let mut current_type = root_type;
        for seg in &segments[1..] {
            let (_, sym) = chain_resolve::find_member(wi, &current_type, seg)?;
            let raw = sym.decl_type.as_deref().or(sym.return_type.as_deref())?;
            current_type = mimir_syntax::symbols::normalize_type_name(raw)?;
        }
        current_type
    };

    let workspace_members = collect_class_members(wi, &class_name);
    let builtins = mimir_syntax::builtin_methods::methods_for_type(&class_name);
    let universals = mimir_syntax::builtin_methods::universal_methods();
    // Only return None when there is truly nothing to offer — workspace
    // members, type-specific builtins, AND universal methods are all empty.
    // (universals is never empty in practice, but guard explicitly.)
    if workspace_members.is_empty() && builtins.is_empty() && universals.is_empty() {
        return None;
    }

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut items: Vec<CompletionItem> = workspace_members
        .into_iter()
        .filter(|(s, _)| {
            prefix_lower.is_empty() || s.name.to_ascii_lowercase().starts_with(&prefix_lower)
        })
        .map(|(sym, url)| {
            let detail = url
                .path_segments()
                .and_then(|mut segs| segs.next_back())
                .map(str::to_owned);
            CompletionItem {
                label: sym.name.clone(),
                kind: Some(symbol_kind_to_completion_kind(sym.kind)),
                detail,
                data: make_resolve_data(&url, sym.name_range.start.line),
                ..Default::default()
            }
        })
        .collect();

    // Helper: append a builtin slice, deduplicating against existing items.
    let append_builtins = |items: &mut Vec<CompletionItem>,
                           table: &'static [mimir_syntax::builtin_methods::BuiltinMethod],
                           prefix_lower: &str| {
        for m in table {
            if !prefix_lower.is_empty()
                && !m.name.to_ascii_lowercase().starts_with(prefix_lower)
            {
                continue;
            }
            if items.iter().any(|i| i.label == m.name) {
                continue;
            }
            items.push(CompletionItem {
                label: m.name.to_owned(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some("built-in".to_owned()),
                documentation: Some(Documentation::String(m.doc.to_owned())),
                ..Default::default()
            });
        }
    };

    // Type-specific built-ins (e.g. string methods). Workspace wins on collision.
    append_builtins(&mut items, builtins, &prefix_lower);
    // Dimension-based built-ins: queue / dynamic-array / associative-array methods.
    if let Some(sfx) = dim_suffix.as_deref() {
        append_builtins(
            &mut items,
            mimir_syntax::builtin_methods::methods_for_suffix(sfx),
            &prefix_lower,
        );
    }
    // Universal methods (rand_mode, constraint_mode, randomize) on any class.
    append_builtins(&mut items, universals, &prefix_lower);

    debug!(
        class = %class_name,
        receiver = ?segments,
        count = items.len(),
        "member completion: syntax fallback",
    );
    Some(CompletionResponse::Array(items))
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
        return Some(hover_from_markdown(
            mimir_syntax::hover_format::format_sv_signature(&sig.label),
        ));
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

    // 2a. For typedefs, append the expanded base type after the declaration.
    if sym.kind == MSymbolKind::Typedef {
        if let Some(base) = typedef_base_from_line(&line, &sym.name) {
            let md = format!(
                "```systemverilog\n{}\n```\n\n**Expands to:** `{}`",
                line, base
            );
            return Some(hover_from_markdown(md));
        }
    }

    Some(hover_markdown(&line))
}

/// Extract the base type from a typedef declaration line.
///
/// Given `"typedef logic [31:0] addr_t;"` and alias `"addr_t"`, returns
/// `Some("logic [31:0]")`. Returns `None` for forward declarations
/// (`typedef class Foo;`) or malformed input.
fn typedef_base_from_line(line: &str, alias: &str) -> Option<String> {
    // Strip leading whitespace and "typedef" keyword.
    let after = line.trim().strip_prefix("typedef")?.trim_start();
    // Find the alias name from the right so struct/enum field names don't confuse us.
    let alias_pos = after.rfind(alias)?;
    let base = after[..alias_pos].trim_end().trim_end_matches(';').trim();
    // Reject forward declarations: base would be "class" or empty.
    if base.is_empty() || base == "class" {
        return None;
    }
    Some(base.to_string())
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
/// Hover for IEEE 1800-2017 built-in methods (`push_back`, `rand_mode`,
/// `len`, `toupper`, `exists`, …).
///
/// Runs after [`hover_via_tree_sitter`] returns `None` so any user-defined
/// method with the same name shadows the built-in entry. The fallback chain:
///
/// * `this` / `super` receiver → universal methods only.
/// * `obj.method` → type-aware lookup for the receiver's declared type
///   (accurate for `string`), then universal table (accurate for
///   `rand_mode` / `constraint_mode` on any class).  When the type cannot
///   be resolved, falls to name-only.
/// * No receiver → name-only scan across all tables (hover is UX, not
///   correctness — better to show something than nothing).
fn builtin_method_hover_at(tree: &SyntaxTree, rope: &Rope, target: MPosition) -> Option<Hover> {
    use mimir_syntax::symbols::{
        find_variable_type_info_at, hover_receiver_at, identifier_at, normalize_type_name,
        HoverReceiver,
    };

    let name = identifier_at(tree, rope, target)?;
    let receiver = hover_receiver_at(tree, rope, target);

    let m: &mimir_syntax::builtin_methods::BuiltinMethod = match &receiver {
        Some(HoverReceiver::This) | Some(HoverReceiver::Super) => {
            mimir_syntax::builtin_methods::find_universal(name)?
        }
        Some(HoverReceiver::Object(recv)) => {
            let type_info = find_variable_type_info_at(tree, rope, target, recv);
            let cls = type_info.as_ref().and_then(|t| normalize_type_name(&t.base));
            if let Some(cls) = cls {
                // Try type-specific then universal (class receiver).
                mimir_syntax::builtin_methods::find_method(&cls, name)
                    .or_else(|| mimir_syntax::builtin_methods::find_universal(name))
                    .or_else(|| {
                        // Class lookup missed — fall back to dimension-suffix
                        // table (e.g. `int q[$]` → QUEUE_METHODS).
                        type_info
                            .as_ref()
                            .and_then(|t| t.suffix.as_deref())
                            .and_then(|sfx| {
                                mimir_syntax::builtin_methods::methods_for_suffix(sfx)
                                    .iter()
                                    .find(|m| m.name == name)
                            })
                    })?
            } else if let Some(sfx) = type_info.as_ref().and_then(|t| t.suffix.as_deref()) {
                // No class name at all (e.g. bare `int q[$]`) — go straight
                // to the dimension-suffix table.
                mimir_syntax::builtin_methods::methods_for_suffix(sfx)
                    .iter()
                    .find(|m| m.name == name)?
            } else {
                mimir_syntax::builtin_methods::find_method_by_name(name)?
            }
        }
        None => mimir_syntax::builtin_methods::find_method_by_name(name)?,
    };

    Some(hover_from_markdown(format!(
        "{}\n\n{}",
        mimir_syntax::hover_format::format_sv_signature(m.signature),
        m.doc
    )))
}

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
    other_trees: &[(Url, SyntaxTree)],
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

    // 2. Other trees (open buffers + closed filelist files) — scope-pruned
    //    file-wide match. occurrences_of_scoped skips occurrences inside
    //    nested scopes that locally re-declare `name`, so a local
    //    `int foo` inside a function won't pollute results when the caller
    //    is searching for a module-level `foo`.
    'outer: for (other_uri, other_tree) in other_trees {
        let other_rope = Rope::from_str(other_tree.source());
        for r in mimir_syntax::symbols::occurrences_of_scoped(other_tree, &other_rope, name) {
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

/// Map a tree-sitter (`mimir-syntax`) diagnostic onto the LSP wire format.
///
/// Delegates to [`crate::diagnostics`] so the severity/range/code mapping
/// lives in exactly one place across both the tree-sitter and slang paths.
fn syntax_to_lsp_diagnostic(d: MDiagnostic) -> Diagnostic {
    crate::diagnostics::mimir_diag_to_lsp(&crate::diagnostics::syntax_diag_to_mimir(&d))
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
fn merge_diagnostics(syntax: Vec<MDiagnostic>) -> Vec<Diagnostic> {
    syntax.into_iter().map(syntax_to_lsp_diagnostic).collect()
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
    use mimir_syntax::DiagnosticSeverity as MSeverity;

    /// Helper: a tree-sitter diagnostic at a given severity.
    fn syntax_diag(sev: MSeverity) -> MDiag {
        MDiag {
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            message: "syntax".to_string(),
            severity: sev,
            code: "syntax",
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

    /// `merge_diagnostics` maps tree-sitter diagnostics to LSP and
    /// returns them in order.
    #[test]
    fn merge_passes_through_syntax_diagnostics() {
        let merged = merge_diagnostics(vec![syntax_diag(MSeverity::Error)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].code,
            Some(NumberOrString::String("syntax".into()))
        );
    }

    /// Empty input returns empty — guards against accidental diagnostic invention.
    #[test]
    fn merge_empty_in_empty_out() {
        assert!(merge_diagnostics(vec![]).is_empty());
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
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
        };
        let f = Symbol {
            name: "f".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(1, 18), MPosition::new(1, 19)),
            full_range: MRange::new(MPosition::new(1, 4), MPosition::new(2, 12)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let g = Symbol {
            name: "g".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(3, 9), MPosition::new(3, 10)),
            full_range: MRange::new(MPosition::new(3, 4), MPosition::new(4, 8)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
        };
        let b = Symbol {
            name: "b".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(2, 7), MPosition::new(2, 8)),
            full_range: MRange::new(MPosition::new(2, 0), MPosition::new(3, 9)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
        };
        let cls = Symbol {
            name: "c".into(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(1, 6), MPosition::new(1, 7)),
            full_range: MRange::new(MPosition::new(1, 0), MPosition::new(6, 8)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let m = Symbol {
            name: "f".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(2, 18), MPosition::new(2, 19)),
            full_range: MRange::new(MPosition::new(2, 4), MPosition::new(3, 12)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        assert_eq!(
            hover_markdown_value(&h),
            "```systemverilog\nclass apb_monitor extends uvm_monitor;\n```",
        );
    }

    /// Callable symbol (function with params) → formatted markdown signature.
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
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        let v = hover_markdown_value(&h);
        // Signature is now rich markdown rather than a fenced code block.
        assert!(v.contains("**function**"), "keyword not bolded: {v:?}");
        assert!(v.contains("`add`"), "name not inline-coded: {v:?}");
        assert!(v.contains("*int*"), "type not italicized: {v:?}");
        assert!(!v.contains("```"), "no code fence expected: {v:?}");
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
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
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
            return_type: None,
            decl_type: None,
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
        let mut parser = mimir_syntax::SyntaxParser::new().expect("grammar load");
        parser.parse(text, None).expect("parse")
    }

    /// Build a populated `WorkspaceIndex` from a slice of `(url, text)`
    /// pairs. Mirrors how the eager hydration pass folds parsed-from-disk
    /// files into the index on `initialize`.
    fn workspace_index_from(files: &[(&Url, &str)]) -> WorkspaceIndex {
        let mut wi = WorkspaceIndex::default();
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
        let wi = WorkspaceIndex::default();
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
        let wi = WorkspaceIndex::default();
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

    /// A file present in `other_trees` (simulating a closed filelist file
    /// cached in `workspace_trees`) contributes all occurrence sites —
    /// declaration *and* usages — not just the declaration the workspace
    /// index knows about.
    #[test]
    fn references_returns_usages_from_closed_file() {
        let here = url("file:///a.sv");
        let closed = url("file:///lib.sv");
        let cursor_text = "module m;\n  my_class c;\nendmodule\n";
        // closed file: 1 declaration + 2 usages of my_class.
        let closed_text =
            "class my_class;\nendclass\nmodule uses;\n  my_class x;\n  my_class y;\nendmodule\n";
        let wi = workspace_index_from(&[(&closed, closed_text)]);
        // Pass closed file via other_trees (simulates workspace_trees cache).
        let out = run_references(
            "my_class",
            &here,
            cursor_text,
            &[(closed.clone(), closed_text)],
            &wi,
            true,
        );
        let closed_hits = out.iter().filter(|l| l.uri == closed).count();
        // The scoped scan must return all 3 occurrences (decl + 2 usages),
        // not just the 1 declaration the workspace index alone would provide.
        assert!(
            closed_hits > 1,
            "expected usages from closed file, not just declaration; got {closed_hits}"
        );
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
        let wi = WorkspaceIndex::default();
        let out = run_references("foo", &here, &text, &[], &wi, true);
        assert_eq!(out.len(), REFERENCES_LIMIT);
    }

    // ----------------------------------------------------------------------
    // rename — locations_to_workspace_edit
    // ----------------------------------------------------------------------

    /// Helper: build a `WorkspaceEdit` from a vec of `Location`s the same
    /// way the `rename` handler does.
    fn locations_to_workspace_edit(locs: Vec<Location>, new_name: &str) -> WorkspaceEdit {
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for loc in locs {
            changes.entry(loc.uri).or_default().push(TextEdit {
                range: loc.range,
                new_text: new_name.to_owned(),
            });
        }
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    /// Rename in the same file: every occurrence is replaced, producing
    /// one entry in `changes`.
    #[test]
    fn rename_single_file_produces_one_entry_per_occurrence() {
        let here = url("file:///a.sv");
        let text = "module m;\n  int foo;\n  initial foo = 1;\nendmodule\n";
        let wi = WorkspaceIndex::default();
        let locs = run_references("foo", &here, text, &[], &wi, true);
        // decl + one use = 2 occurrences
        assert_eq!(locs.len(), 2);

        let edit = locations_to_workspace_edit(locs, "bar");
        let file_edits = edit.changes.unwrap();
        assert_eq!(file_edits.len(), 1, "single file → one URL key");
        let edits = &file_edits[&here];
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|e| e.new_text == "bar"));
    }

    /// Cross-file rename: two URLs should appear in `changes`.
    #[test]
    fn rename_cross_file_produces_entry_per_file() {
        let here = url("file:///a.sv");
        let other = url("file:///b.sv");
        let cursor_text = "module a;\n  my_class c;\nendmodule\n";
        let other_text = "class my_class;\nendclass\n";
        let wi = workspace_index_from(&[(&other, other_text)]);
        let locs =
            run_references("my_class", &here, cursor_text, &[(other.clone(), other_text)], &wi, true);

        let edit = locations_to_workspace_edit(locs, "renamed_class");
        let file_edits = edit.changes.unwrap();
        assert_eq!(file_edits.len(), 2, "two URLs should appear");
        assert!(file_edits.contains_key(&here));
        assert!(file_edits.contains_key(&other));
        let all_new: Vec<&str> = file_edits
            .values()
            .flatten()
            .map(|e| e.new_text.as_str())
            .collect();
        assert!(all_new.iter().all(|&s| s == "renamed_class"));
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

    // ----------------------------------------------------------------------
    // syntax_member_completion helpers
    // ----------------------------------------------------------------------

    /// `receiver_ident_before_dot` extracts the identifier left of the `.`.
    #[test]
    fn receiver_ident_before_dot_super() {
        let rope = Rope::from_str("    super.");
        // cursor after the dot (col 10)
        let pos = MPosition::new(0, 10);
        assert_eq!(receiver_ident_before_dot(&rope, pos).as_deref(), Some("super"));
    }

    #[test]
    fn receiver_ident_before_dot_with_prefix() {
        // partial prefix: "obj.fo" — cursor at end
        let rope = Rope::from_str("obj.fo");
        let pos = MPosition::new(0, 6);
        assert_eq!(receiver_ident_before_dot(&rope, pos).as_deref(), Some("obj"));
    }

    #[test]
    fn receiver_ident_before_dot_chained_call_returns_none() {
        // "get_obj()." — nothing plain before the dot
        let rope = Rope::from_str("get_obj().");
        let pos = MPosition::new(0, 10);
        assert!(receiver_ident_before_dot(&rope, pos).is_none());
    }

    #[test]
    fn receiver_ident_before_dot_scope_trigger_returns_none() {
        // "::" is not a `.` trigger
        let rope = Rope::from_str("pkg::");
        let pos = MPosition::new(0, 5);
        assert!(receiver_ident_before_dot(&rope, pos).is_none());
    }

    /// `collect_class_members` returns all symbols inside a class body,
    /// walks the `extends` chain, and closest-ancestor wins on name collision.
    #[test]
    fn collect_class_members_single_class() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void foo(); endfunction
  int bar;
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let members = collect_class_members(&wi, "Base");
        let names: Vec<&str> = members.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"foo"), "should include method foo");
        assert!(names.contains(&"bar"), "should include field bar");
        assert!(!names.contains(&"Base"), "should not include the class itself");
    }

    #[test]
    fn collect_class_members_walks_extends_chain() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void base_fn(); endfunction
endclass
class Child extends Base;
  function void child_fn(); endfunction
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let members = collect_class_members(&wi, "Child");
        let names: Vec<&str> = members.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"child_fn"), "own method");
        assert!(names.contains(&"base_fn"), "inherited method");
    }

    #[test]
    fn collect_class_members_override_deduplication() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void run(); endfunction
endclass
class Child extends Base;
  function void run(); endfunction
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let members = collect_class_members(&wi, "Child");
        let run_count = members.iter().filter(|(s, _)| s.name == "run").count();
        assert_eq!(run_count, 1, "overridden method should appear only once");
    }

    /// `syntax_member_completion` returns candidates for `super.` when the
    /// enclosing class has a known parent.
    #[test]
    fn syntax_member_completion_super() {
        let url = url("file:///a.sv");
        let src = "\
class Base;
  function void base_method(); endfunction
endclass
class Child extends Base;
  function void my_fn();
    super.
  endfunction
endclass
";
        let wi = workspace_index_from(&[(&url, src)]);
        let tree = parse_tree(src);
        let rope = Rope::from_str(src);
        // Line 5 (0-indexed): "    super." — cursor after the dot at col 10.
        let pos = MPosition::new(5, 10);
        let resp = syntax_member_completion(&wi, &tree, &rope, pos, "");
        assert!(resp.is_some(), "should return Some for super.");
        if let Some(CompletionResponse::Array(items)) = resp {
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"base_method"), "parent method should appear");
        }
    }

    /// `syntax_member_completion` always offers universal methods (rand_mode,
    /// constraint_mode, randomize) on any class receiver, even when the
    /// class has no workspace-indexed members.
    #[test]
    fn syntax_member_completion_universal_methods_on_any_class() {
        // Index has MyClass but no members — universal methods must still appear.
        let url = Url::parse("file:///test/my.sv").unwrap();
        let mut wi = WorkspaceIndex::default();
        wi.update(url, &[Symbol {
            name: "MyClass".to_string(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 7)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(5, 0)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        }]);

        let src = "class wrapper;\n  MyClass obj;\n  function void run();\n    obj.\n  endfunction\nendclass\n";
        let tree = parse_tree(src);
        let rope = Rope::from_str(src);
        // Line 3 (0-indexed): "    obj." — cursor after the dot.
        let pos = MPosition::new(3, 8);
        let resp = syntax_member_completion(&wi, &tree, &rope, pos, "");
        assert!(resp.is_some(), "universal methods should make Some");
        if let Some(CompletionResponse::Array(items)) = resp {
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"rand_mode"), "rand_mode should appear");
            assert!(labels.contains(&"constraint_mode"), "constraint_mode should appear");
            assert!(labels.contains(&"randomize"), "randomize should appear");
        }
    }

    /// `syntax_member_completion` returns `None` for an unknown receiver
    /// (undeclared variable) — no workspace dump.
    #[test]
    fn syntax_member_completion_unknown_receiver_returns_none() {
        let wi = WorkspaceIndex::default();
        let src = "module m; initial unknown_var. endmodule\n";
        let tree = parse_tree(src);
        let rope = Rope::from_str(src);
        let pos = MPosition::new(0, 30);
        assert!(
            syntax_member_completion(&wi, &tree, &rope, pos, "").is_none(),
            "undeclared variable should return None"
        );
    }

    // typedef_base_from_line

    #[test]
    fn typedef_base_logic_vector() {
        assert_eq!(
            typedef_base_from_line("typedef logic [31:0] addr_t;", "addr_t"),
            Some("logic [31:0]".to_string())
        );
    }

    #[test]
    fn typedef_base_enum() {
        assert_eq!(
            typedef_base_from_line("typedef enum logic { A, B } my_e;", "my_e"),
            Some("enum logic { A, B }".to_string())
        );
    }

    #[test]
    fn typedef_base_struct() {
        assert_eq!(
            typedef_base_from_line("typedef struct { int x; int y; } point_t;", "point_t"),
            Some("struct { int x; int y; }".to_string())
        );
    }

    #[test]
    fn typedef_base_forward_class_returns_none() {
        assert_eq!(
            typedef_base_from_line("typedef class MyClass;", "MyClass"),
            None
        );
    }

    #[test]
    fn typedef_base_simple_alias() {
        assert_eq!(
            typedef_base_from_line("typedef int my_int_t;", "my_int_t"),
            Some("int".to_string())
        );
    }
}
