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
    Client as SlangClient, Diagnostic as SlangDiagnostic, ElaborateParams, ElaborateResult,
    Severity as SlangSeverity, SourceFile,
};
use mimir_syntax::{
    Diagnostic as MDiagnostic, DiagnosticSeverity as MSeverity, Symbol, SymbolKind as MSymbolKind,
    SyntaxParser,
};
use ropey::Rope;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, error, info, instrument, warn};

use crate::project::ResolvedProject;

/// Per-document state held inside the store.
///
/// We keep the parser-side data (last `tree`) here so the next parse can be
/// incremental. *Right now we don't actually feed the previous tree into
/// the next parse* because we'd also need to apply `Tree::edit` for each
/// change first; that's the next slice. The plumbing is here so we don't
/// have to refactor the store later.
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
    /// Document version the `index` was built from. Used to detect a
    /// stale write — `reparse_and_publish` may finish after a fresh
    /// `did_change` has bumped the version, in which case we must not
    /// overwrite the live `index` with the stale parse's results.
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

    /// Optional slang sidecar client. `None` when the user hasn't opted in
    /// via `MIMIR_SLANG_PATH`, or the configured path failed to spawn.
    /// While `None`, the diagnostic pipeline is tree-sitter-only.
    slang: Option<Arc<SlangClient>>,

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
            slang,
            project: Arc::new(RwLock::new(None)),
            pending_elaborations: Arc::new(RwLock::new(HashMap::new())),
            slang_published: Arc::new(RwLock::new(HashSet::new())),
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

        let (diags, new_index) = match parse_result {
            Ok(tree) => {
                let rope = Rope::from_str(&text);
                let diags = mimir_syntax::diagnostics::collect(&tree, &rope);
                let index = mimir_syntax::symbols::index(&tree, &rope);
                (diags, Some(index))
            }
            Err(e) => {
                error!(error = %e, "parser returned error; publishing empty diagnostics");
                (Vec::new(), None)
            }
        };

        // Write the fresh index back into the doc store, but only if the
        // version we parsed is still the live one — otherwise a `did_change`
        // landed mid-parse and our index is already stale.
        if let Some(index) = new_index {
            let mut docs = self.documents.write().await;
            if let Some(state) = docs.get_mut(&uri) {
                if state.document.version() == version {
                    state.index = index;
                    state.index_version = version;
                }
            }
        }

        // Tree-sitter publishes immediately so the editor has prompt
        // feedback (~ms after a keystroke). The deeper slang elaborate
        // is scheduled separately on a debounce timer; when it lands it
        // *overwrites* this publish for files in its compilation unit.
        let lsp_diags = merge_diagnostics(diags, Vec::new(), /* slang_active */ false);

        debug!(count = lsp_diags.len(), "publishing tree-sitter diagnostics");
        self.client
            .publish_diagnostics(uri, lsp_diags, Some(version))
            .await;
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
        let Some(slang) = self.slang.clone() else {
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
        // during the sleep is clean; aborting after the request was sent
        // means the response is dropped (the next request will get the
        // next id, no protocol confusion).
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
            let (params, files_in_request) =
                build_elaborate_params(&project, &documents).await;
            debug!(
                files = params.files.len(),
                include_dirs = params.include_dirs.len(),
                "sending elaborate request",
            );
            match slang.elaborate(&params).await {
                Ok(result) => {
                    publish_slang_result(
                        &lsp_client,
                        &files_in_request,
                        result,
                        &slang_published,
                    )
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
                    *self.project.write().await = Some(resolved);
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
                // Stage 1 of go-to-definition: same-file lookup against the
                // tree-sitter symbol index cached on each `DocumentState`.
                // Stage 2 will broaden this to the workspace; Stage 3 will
                // route to slang when the sidecar is configured.
                definition_provider: Some(OneOf::Left(true)),
                // `documentSymbol` reuses the same cached index — same data,
                // free checkbox. The editor uses it to render the outline
                // view and to drive symbol-aware navigation shortcuts.
                document_symbol_provider: Some(OneOf::Left(true)),
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
                        if let Err(e) =
                            state.document.apply_incremental_edit(m_range, &change.text, new_version)
                        {
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

        // Snapshot text + index under the read lock; release before any
        // re-parsing happens (lock-then-clone).
        let (text, index) = {
            let docs = self.documents.read().await;
            let Some(state) = docs.get(&uri) else {
                debug!("goto_definition for unknown URI; returning None");
                return Ok(None);
            };
            (state.document.text(), state.index.clone())
        };

        // Re-parse so we can resolve `identifier_at`. The cached `index`
        // already gives us declarations, but to find the *reference*
        // under the cursor we need the tree itself. Tree-sitter parses
        // are fast (~ms) and this happens only on user-initiated F12,
        // so we don't bother caching the tree.
        let tree = {
            let mut parser = self.parser.lock().await;
            match parser.parse(&text, None) {
                Ok(t) => t,
                Err(e) => {
                    error!(error = %e, "parse failed during goto_definition");
                    return Ok(None);
                }
            }
        };
        let rope = Rope::from_str(&text);
        let Some(name) = mimir_syntax::symbols::identifier_at(&tree, &rope, target) else {
            debug!("no identifier at cursor; returning None");
            return Ok(None);
        };

        let matches = resolve_in_index(name, &index);
        if matches.is_empty() {
            debug!(name, "no symbol matches in same-file index");
            return Ok(None);
        }

        let locations: Vec<Location> = matches
            .into_iter()
            .map(|sym| Location {
                uri: uri.clone(),
                range: m_range_to_lsp(sym.name_range),
            })
            .collect();
        debug!(name, count = locations.len(), "goto_definition resolved");
        Ok(Some(GotoDefinitionResponse::Array(locations)))
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

        let symbols: Vec<SymbolInformation> = index
            .iter()
            .map(|sym| symbol_to_lsp_information(sym, &uri))
            .collect();
        debug!(count = symbols.len(), "document_symbol returned");
        // We use the flat `SymbolInformation` form rather than the nested
        // `DocumentSymbol` tree because Stage 1 doesn't model the
        // declaration hierarchy (a class's methods aren't children of the
        // class symbol). VS Code renders both fine; nesting is a Stage 2
        // follow-up if it matters.
        #[allow(deprecated)]
        Ok(Some(DocumentSymbolResponse::Flat(symbols)))
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

/// Find every symbol in `index` whose name matches `name` exactly.
///
/// Returned in source order (the index is built that way). Same-file
/// only — `mimir-server` is the only caller and it controls which index
/// it passes; Stage 2 will add a workspace-wide variant alongside.
fn resolve_in_index<'a>(name: &str, index: &'a [Symbol]) -> Vec<&'a Symbol> {
    index.iter().filter(|s| s.name == name).collect()
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
        MSymbolKind::Typedef => SymbolKind::TYPE_PARAMETER,
        MSymbolKind::Parameter => SymbolKind::CONSTANT,
        MSymbolKind::Variable => SymbolKind::VARIABLE,
        MSymbolKind::Port => SymbolKind::FIELD,
        MSymbolKind::Property | MSymbolKind::Sequence | MSymbolKind::Covergroup => {
            SymbolKind::OBJECT
        }
    }
}

/// Convert a `mimir-syntax::Symbol` to the flat LSP `SymbolInformation`
/// the editor consumes.
///
/// `SymbolInformation` is technically deprecated in favour of the
/// hierarchical `DocumentSymbol`, but every editor we care about still
/// supports it and Stage 1 doesn't model the declaration tree. Stage 2
/// will switch to `DocumentSymbol` once we model `class { method }`
/// nesting.
#[allow(deprecated)]
fn symbol_to_lsp_information(sym: &Symbol, uri: &Url) -> SymbolInformation {
    SymbolInformation {
        name: sym.name.clone(),
        kind: symbol_kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: m_range_to_lsp(sym.full_range),
        },
        container_name: None,
    }
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
            assert_eq!(syntax_to_lsp_diagnostic(syntax_diag(ours)).severity, Some(theirs));
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
        assert_eq!(lsp.code, Some(NumberOrString::String("UnknownModule".into())));
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
            assert_eq!(slang_to_lsp_diagnostic(slang_diag(ours, "x")).severity, Some(theirs));
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
        assert_eq!(merged[0].code, Some(NumberOrString::String("syntax".into())));
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
        assert_eq!(merged[0].code, Some(NumberOrString::String("UnknownModule".into())));
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
        assert!(merged.is_empty(), "expected zero diagnostics, got {merged:?}");
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
    use mimir_slang::{ElaborateResult, SourceFile};
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
        open_text.insert(scratch.clone(), (scratch_url.clone(), "module s; endmodule".into()));

        let (params, files_in_request) = assemble_elaborate_params(&project, &open_text, |_| {
            Some(String::new())
        });

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

    /// Plan: every requested file gets a publish, including files with
    /// no diagnostics (empty publish overwrites tree-sitter).
    #[test]
    fn plan_publishes_every_requested_file_even_when_clean() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let url_b = Url::parse("file:///proj/b.sv").unwrap();
        let result = ElaborateResult {
            diagnostics: vec![slang_diag_at("/proj/a.sv", "X")],
        };
        let plan = plan_slang_publishes(
            &[url_a.clone(), url_b.clone()],
            result,
            &HashSet::new(),
        );

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
        let result = ElaborateResult { diagnostics: vec![] };
        let prev = HashSet::from([url_dropped.clone()]);

        let plan = plan_slang_publishes(&[url_a.clone()], result, &prev);

        // Two publishes: empty for url_a (in request), empty for
        // url_dropped (stale-clear).
        assert_eq!(plan.publishes.len(), 2);
        assert!(plan.publishes.iter().any(|(u, d)| u == &url_a && d.is_empty()));
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
        let result = ElaborateResult { diagnostics: vec![] };
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
        assert!(plan.publishes.iter().any(|(u, d)| u == &inc_url && d.len() == 1));
        assert!(plan.new_published.contains(&inc_url));
    }

    // --- Stage 1: go-to-definition resolver ---------------------------

    /// Helper: build a `Symbol` of the given name + kind. Range values
    /// are arbitrary — the tests only care about identity/order.
    fn sym(name: &str, kind: MSymbolKind, line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 1)),
            full_range: MRange::new(MPosition::new(line, 0), MPosition::new(line, 10)),
        }
    }

    /// Empty index: nothing matches.
    #[test]
    fn resolve_in_index_empty_returns_no_match() {
        assert!(resolve_in_index("foo", &[]).is_empty());
    }

    /// Single match comes back exactly once.
    #[test]
    fn resolve_in_index_single_match() {
        let idx = vec![
            sym("foo", MSymbolKind::Module, 0),
            sym("bar", MSymbolKind::Class, 1),
        ];
        let hits = resolve_in_index("foo", &idx);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "foo");
        assert_eq!(hits[0].kind, MSymbolKind::Module);
    }

    /// Multiple symbols sharing a name (e.g. a `var x` and a class field
    /// `x` in the same file) all come back, in source order. The
    /// editor's peek list shows them and lets the user pick — that's
    /// the v1 UX for ambiguous syntactic resolution.
    #[test]
    fn resolve_in_index_multiple_matches_in_order() {
        let idx = vec![
            sym("x", MSymbolKind::Variable, 1),
            sym("y", MSymbolKind::Variable, 2),
            sym("x", MSymbolKind::Parameter, 5),
        ];
        let hits = resolve_in_index("x", &idx);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].kind, MSymbolKind::Variable);
        assert_eq!(hits[0].name_range.start.line, 1);
        assert_eq!(hits[1].kind, MSymbolKind::Parameter);
        assert_eq!(hits[1].name_range.start.line, 5);
    }

    /// Names that don't appear in the index resolve to nothing.
    #[test]
    fn resolve_in_index_miss_returns_no_match() {
        let idx = vec![sym("foo", MSymbolKind::Module, 0)];
        assert!(resolve_in_index("bar", &idx).is_empty());
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
            (MSymbolKind::Typedef, SymbolKind::TYPE_PARAMETER),
            (MSymbolKind::Parameter, SymbolKind::CONSTANT),
            (MSymbolKind::Variable, SymbolKind::VARIABLE),
            (MSymbolKind::Port, SymbolKind::FIELD),
            (MSymbolKind::Property, SymbolKind::OBJECT),
            (MSymbolKind::Sequence, SymbolKind::OBJECT),
            (MSymbolKind::Covergroup, SymbolKind::OBJECT),
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

    /// `symbol_to_lsp_information` carries name, kind, and the URL
    /// through to the wire shape. The location range is the symbol's
    /// `full_range` (the whole declaration), matching what VS Code
    /// expects for outline view selection.
    #[test]
    #[allow(deprecated)]
    fn symbol_to_lsp_information_round_trip() {
        let s = Symbol {
            name: "my_mod".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(0, 7), MPosition::new(0, 13)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 9)),
        };
        let url = Url::parse("file:///proj/m.sv").unwrap();
        let info = symbol_to_lsp_information(&s, &url);
        assert_eq!(info.name, "my_mod");
        assert_eq!(info.kind, SymbolKind::MODULE);
        assert_eq!(info.location.uri, url);
        assert_eq!(info.location.range.start.line, 0);
        assert_eq!(info.location.range.end.line, 2);
    }
}
