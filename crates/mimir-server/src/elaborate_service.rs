//! Elaborate lifecycle service: debounce, dedup, and diagnostic publishing.
//!
//! This module owns the full elaborate round-trip: debounce timer, input-hash
//! dedup (skip the sidecar when nothing changed), the actual
//! `SlangService::elaborate` call, and the `publishDiagnostics` fan-out.
//!
//! # Relationship to `Backend` and `SlangService`
//!
//! `ElaborateService` holds an `Arc<SlangService>` so it can call
//! `build_elaborate_params` and `elaborate` on demand. It holds a `Client`
//! clone so it can push diagnostics back to the editor without going through
//! `Backend`. `Backend` therefore only needs to call
//! [`ElaborateService::schedule`] after any document mutation — the rest of
//! the elaborate lifecycle is self-contained here.
//!
//! # Debounce / dedup model
//!
//! Each call to [`ElaborateService::schedule`] does three things:
//!
//! 1. Cancels any in-flight task already waiting for the same trigger URI.
//! 2. Spawns a new tokio task that sleeps `debounce_ms` before elaborating.
//! 3. On wakeup: if the input hash matches the last successful run, skips the
//!    round-trip to the sidecar entirely.
//!
//! Call [`ElaborateService::invalidate_hash`] to force the next elaborate to
//! run regardless of inputs — useful after a project reload where the file
//! list may have changed without touching any open buffer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use mimir_ast::{MimirDiag};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{Diagnostic, Url};
use tower_lsp::Client;
use tracing::{debug, info, warn};

use crate::diag_policy::{demoted_severity, DiagAction, DiagnosticPolicy};
use crate::diagnostics::mimir_diag_to_lsp;
use crate::slang_adapter::SlangAdapter;
use crate::slang_service::{path_to_url, SlangService};
use crate::workspace_index::WorkspaceState;
use mimir_syntax::SymbolKind;

// --------------------------------------------------------------------------
// Internal types
// --------------------------------------------------------------------------

/// One debounce-map entry: the owning task's generation tag plus its handle.
type PendingTask = (u64, JoinHandle<()>);

/// The debounce map: one tagged in-flight elaborate task per trigger URI.
type PendingMap = HashMap<Url, PendingTask>;

/// One round of slang publishes, computed without touching tower-lsp.
struct SlangPublishPlan {
    /// In-order publish calls to make. Each `(url, diags)` becomes one
    /// `publish_diagnostics` call. Empty `diags` clears that URL.
    publishes: Vec<(Url, Vec<Diagnostic>)>,
    /// URLs that ended up with non-empty slang diagnostics this cycle —
    /// stored in `published` so the *next* cycle can clear any that drop off.
    new_published: HashSet<Url>,
}

// --------------------------------------------------------------------------
// ElaborateService
// --------------------------------------------------------------------------

/// Elaborate lifecycle: debounce → dedup → sidecar compile → diagnostic publish.
///
/// Holds an `Arc<SlangAdapter>` (which wraps `SlangService`) and a `Client`
/// clone for pushing diagnostics. The adapter owns the compile RPC call and
/// the cached MimirAst; this service owns debounce, dedup, and publishing.
pub(crate) struct ElaborateService {
    /// Compile RPC driver + MimirAst cache — shared with `Backend`.
    adapter: Arc<SlangAdapter>,
    /// Channel for pushing `publishDiagnostics` back to the editor.
    client: Client,
    /// One in-flight (sleeping or running) elaborate task per trigger URI.
    /// Aborted and replaced on every new edit for the same URI — the
    /// debounce. Each entry is tagged with the generation of the task that
    /// owns it so a finishing task only removes *its own* entry, never a
    /// successor's (see [`remove_pending_generation`]).
    pending: Arc<RwLock<PendingMap>>,
    /// Monotonic source for the per-task generation tags in `pending`.
    next_generation: Arc<AtomicU64>,
    /// Hash of inputs sent to the last *successful* compile. The same hash
    /// on the next call → skip the sidecar entirely.
    last_hash: Arc<RwLock<Option<u64>>>,
    /// URLs we published non-empty slang diagnostics to last cycle.
    /// Diffed each cycle so stale squiggles are cleared when errors are fixed.
    published: Arc<RwLock<HashSet<Url>>>,
    /// Latched `true` after the first successful compile logs its per-file
    /// "indexed by startup slang elaborate" messages.
    startup_logged: Arc<AtomicBool>,
    /// Shared workspace index — read at publish time to suppress
    /// `UnknownDirective` diagnostics for macros the tree-sitter scan found.
    workspace: Arc<RwLock<WorkspaceState>>,
}

impl ElaborateService {
    /// Construct the service from a shared `SlangAdapter`, a `Client` clone,
    /// and the workspace state. All three are cheap (reference-counted).
    pub(crate) fn new(
        adapter: Arc<SlangAdapter>,
        client: Client,
        workspace: Arc<RwLock<WorkspaceState>>,
    ) -> Self {
        Self {
            adapter,
            client,
            pending: Arc::new(RwLock::new(HashMap::new())),
            next_generation: Arc::new(AtomicU64::new(0)),
            last_hash: Arc::new(RwLock::new(None)),
            published: Arc::new(RwLock::new(HashSet::new())),
            startup_logged: Arc::new(AtomicBool::new(false)),
            workspace,
        }
    }

    /// Schedule a debounced slang elaborate triggered by an edit to `trigger_uri`.
    ///
    /// Returns immediately. The elaborate itself happens on a background task:
    ///
    /// 1. Sleep `debounce_ms` (from the project config).
    /// 2. Build elaborate params via `SlangService::build_elaborate_params`.
    /// 3. Hash inputs; skip if unchanged since the last successful run.
    /// 4. Call `SlangService::elaborate`.
    /// 5. Publish diagnostics via the `Client`.
    ///
    /// No-op when slang isn't configured or no project has been discovered
    /// (both are normal "tree-sitter only" states).
    pub(crate) async fn schedule(&self, trigger_uri: Url) {
        let debounce = match self.adapter.slang().current_debounce().await {
            Some(d) => d,
            None => return,
        };

        let adapter = self.adapter.clone();
        let pending = self.pending.clone();
        let published = self.published.clone();
        let last_hash = self.last_hash.clone();
        let startup_logged = self.startup_logged.clone();
        let lsp_client = self.client.clone();
        let workspace = self.workspace.clone();
        let trigger_for_task = trigger_uri.clone();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        // Abort-prior, spawn, and insert happen under one write-lock hold so
        // two concurrent `schedule` calls for the same URI can't interleave
        // and leave an orphaned (never-aborted) task behind. The spawned
        // task's own cleanup waits on this same lock, so it can't run before
        // its entry is inserted.
        let mut pending_guard = self.pending.write().await;
        if let Some((_, prior)) = pending_guard.remove(&trigger_uri) {
            prior.abort();
            debug!(uri = %trigger_uri, "cancelled prior pending elaborate");
        }

        let handle = tokio::spawn(async move {
            mimir_core::time_scope!("elaborate.task_total");
            {
                mimir_core::time_scope!("elaborate.debounce_sleep");
                tokio::time::sleep(debounce).await;
            }

            let Some((params, files_in_request)) =
                adapter.slang().build_elaborate_params().await
            else {
                remove_pending_generation(&pending, &trigger_for_task, generation).await;
                return;
            };

            let input_hash = {
                mimir_core::time_scope!("elaborate.hash_inputs");
                SlangService::hash_inputs(&params)
            };
            if *last_hash.read().await == Some(input_hash) {
                debug!(
                    hash = input_hash,
                    files = params.files.len(),
                    "slang inputs unchanged since last compile; skipping",
                );
                remove_pending_generation(&pending, &trigger_for_task, generation).await;
                return;
            }

            debug!(
                files = params.files.len(),
                include_dirs = params.include_dirs.len(),
                hash = input_hash,
                "sending compile request",
            );
            if let Some(outcome) = adapter.compile(&params, files_in_request).await {
                let known_macros: HashSet<String> = {
                    mimir_core::time_scope!("elaborate.collect_known_macros");
                    let ws = workspace.read().await;
                    ws.index
                        .entries()
                        .filter(|e| e.symbol.kind == SymbolKind::Macro)
                        .map(|e| e.symbol.name.clone())
                        .collect()
                };
                let policy = adapter.slang().current_diagnostic_policy().await;
                {
                    mimir_core::time_scope!("elaborate.publish_diagnostics");
                    publish_slang_result(
                        &lsp_client,
                        &outcome.files_in_request,
                        outcome.diagnostics,
                        &published,
                        &known_macros,
                        &policy,
                    )
                    .await;
                }
                *last_hash.write().await = Some(input_hash);

                if startup_logged
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    for url in &outcome.files_in_request {
                        info!(file = %url, "indexed by startup slang compile");
                    }
                }
            } else {
                debug!("compile returned no outcome; diagnostic state unchanged");
            }

            remove_pending_generation(&pending, &trigger_for_task, generation).await;
        });

        pending_guard.insert(trigger_uri, (generation, handle));
    }

    /// Reset the input hash so the next [`Self::schedule`] call always runs the
    /// sidecar, even if the file contents haven't changed.
    ///
    /// Call this after a project reload where the filelist or include dirs
    /// may have changed without touching any open buffer.
    pub(crate) async fn invalidate_hash(&self) {
        *self.last_hash.write().await = None;
    }
}

// --------------------------------------------------------------------------
// Pending-map helpers
// --------------------------------------------------------------------------

/// Remove the `pending` entry for `uri` only when it still belongs to the
/// task tagged `generation`.
///
/// A finishing elaborate task must not blindly `remove(uri)`: a newer
/// `schedule` call for the same URI may already have replaced the entry
/// with its own handle, and removing *that* would silently break the
/// next debounce-abort for the URI.
async fn remove_pending_generation(pending: &RwLock<PendingMap>, uri: &Url, generation: u64) {
    let mut map = pending.write().await;
    if map.get(uri).is_some_and(|(gen, _)| *gen == generation) {
        map.remove(uri);
    }
}

// --------------------------------------------------------------------------
// Diagnostic helpers
// --------------------------------------------------------------------------

/// Publish a completed slang elaborate result as LSP diagnostics.
///
/// Diffs against `slang_published` to ensure URLs that were flagged last
/// cycle but are clean this cycle receive an explicit empty publish —
/// otherwise the editor keeps showing stale red squiggles after the user
/// fixes the root-cause error.
///
/// The version number passed to `publish_diagnostics` is `None` because
/// slang's response can lag behind the editor's view of the document by
/// several edits — letting the editor decide whether to display means we
/// don't need to invent a synthetic version.
async fn publish_slang_result(
    lsp_client: &Client,
    files_in_request: &[Url],
    diagnostics: Vec<(String, MimirDiag)>,
    slang_published: &Arc<RwLock<HashSet<Url>>>,
    known_macros: &HashSet<String>,
    policy: &DiagnosticPolicy,
) {
    let prev_snapshot = slang_published.read().await.clone();
    let plan =
        plan_slang_publishes(files_in_request, diagnostics, &prev_snapshot, known_macros, policy);

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
    diagnostics: Vec<(String, MimirDiag)>,
    previous_published: &HashSet<Url>,
    known_macros: &HashSet<String>,
    policy: &DiagnosticPolicy,
) -> SlangPublishPlan {
    let mut by_url: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    for (path, mut d) in diagnostics {
        if suppressed_unknown_directive(&d, known_macros) {
            continue;
        }
        // Path-based demote / ignore (`[diagnostics]` in `.mimir.toml`): quiet
        // diagnostics for vendored code (UVM, third-party IP) the user can't
        // fix, without losing them. `path` is the slang-reported file path.
        match policy.action_for(&path) {
            DiagAction::Drop => continue,
            DiagAction::DemoteFloor(floor) => {
                d.severity = demoted_severity(d.severity, floor);
            }
            DiagAction::Keep => {}
        }
        let Some(url) = path_to_url(&path) else {
            warn!(path = %path, "could not map slang path back to a URL; dropping");
            continue;
        };
        by_url
            .entry(url)
            .or_default()
            .push(mimir_diag_to_lsp(&d));
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

// --------------------------------------------------------------------------
// Diagnostic suppression helpers
// --------------------------------------------------------------------------

/// Extract the bare macro name from a slang `UnknownDirective` message.
///
/// Slang formats this as `"unknown macro or compiler directive '`name'"`.
/// Returns the name without the leading backtick, or `None` if the message
/// doesn't match that format.
fn extract_macro_name(message: &str) -> Option<&str> {
    let end = message.rfind('\'')?;
    let before_end = &message[..end];
    let start = before_end.rfind('\'')? + 1;
    let raw = &message[start..end];
    Some(raw.strip_prefix('`').unwrap_or(raw))
}

/// Returns `true` when `diag` is an `UnknownDirective` for a macro that the
/// tree-sitter workspace index already knows about.
///
/// Slang emits `UnknownDirective` for every backtick macro it hasn't seen
/// a `define for. When the workspace index found the macro via `include
/// scanning (hover already works), showing the error alongside correct hover
/// info is contradictory. Suppress it to avoid false-positive red squiggles
/// on UVM macros that slang just wasn't given the include path for.
fn suppressed_unknown_directive(diag: &MimirDiag, known_macros: &HashSet<String>) -> bool {
    if diag.code != "UnknownDirective" {
        return false;
    }
    extract_macro_name(&diag.message).is_some_and(|name| known_macros.contains(name))
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_ast::{DiagSeverity, MimirPos, MimirRange};

    /// Build a `(path, MimirDiag)` test fixture at the given path and code.
    fn mimir_diag_at(path: &str, code: &str) -> (String, MimirDiag) {
        (
            path.into(),
            MimirDiag {
                range: MimirRange {
                    start: MimirPos { line: 0, character: 0 },
                    end:   MimirPos { line: 0, character: 1 },
                },
                severity: DiagSeverity::Error,
                code:    code.into(),
                message: "boom".into(),
            },
        )
    }

    /// Plan: every requested file gets a publish, including files with
    /// no diagnostics (empty publish overwrites tree-sitter).
    #[test]
    fn plan_publishes_every_requested_file_even_when_clean() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let url_b = Url::parse("file:///proj/b.sv").unwrap();
        let diags = vec![mimir_diag_at("/proj/a.sv", "X")];
        let plan = plan_slang_publishes(&[url_a.clone(), url_b.clone()], diags, &HashSet::new(), &HashSet::new(), &DiagnosticPolicy::default());

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
        let prev = HashSet::from([url_dropped.clone()]);

        let plan = plan_slang_publishes(std::slice::from_ref(&url_a), vec![], &prev, &HashSet::new(), &DiagnosticPolicy::default());

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
        let prev = HashSet::from([url_a.clone()]);

        let plan = plan_slang_publishes(std::slice::from_ref(&url_a), vec![], &prev, &HashSet::new(), &DiagnosticPolicy::default());

        assert_eq!(plan.publishes.len(), 1);
        assert!(plan.publishes[0].1.is_empty());
    }

    /// Plan: diagnostics for a file we didn't request (transitive
    /// include) still get published.
    #[test]
    fn plan_publishes_transitive_include_diagnostics() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let diags = vec![mimir_diag_at("/proj/inc/uvm.svh", "X")];

        let plan = plan_slang_publishes(std::slice::from_ref(&url_a), diags, &HashSet::new(), &HashSet::new(), &DiagnosticPolicy::default());

        assert_eq!(plan.publishes.len(), 2);
        let inc_url = Url::parse("file:///proj/inc/uvm.svh").unwrap();
        assert!(plan
            .publishes
            .iter()
            .any(|(u, d)| u == &inc_url && d.len() == 1));
        assert!(plan.new_published.contains(&inc_url));
    }

    fn unknown_directive_diag(path: &str, directive: &str) -> (String, MimirDiag) {
        (
            path.into(),
            MimirDiag {
                range: MimirRange {
                    start: MimirPos { line: 0, character: 0 },
                    end:   MimirPos { line: 0, character: 1 },
                },
                severity: DiagSeverity::Error,
                code:    "UnknownDirective".into(),
                message: format!("unknown macro or compiler directive '`{directive}'"),
            },
        )
    }

    /// `UnknownDirective` for a macro in `known_macros` is suppressed.
    #[test]
    fn unknown_directive_suppressed_when_macro_known() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let diags = vec![unknown_directive_diag("/proj/a.sv", "uvm_field_utils_begin")];
        let known = HashSet::from(["uvm_field_utils_begin".to_string()]);

        let plan = plan_slang_publishes(std::slice::from_ref(&url_a), diags, &HashSet::new(), &known, &DiagnosticPolicy::default());

        let a_pub = plan.publishes.iter().find(|(u, _)| u == &url_a).unwrap();
        assert!(a_pub.1.is_empty(), "diagnostic should be suppressed");
        assert!(plan.new_published.is_empty());
    }

    /// `UnknownDirective` for a macro NOT in `known_macros` is kept.
    #[test]
    fn unknown_directive_kept_when_macro_unknown() {
        let url_a = Url::parse("file:///proj/a.sv").unwrap();
        let diags = vec![unknown_directive_diag("/proj/a.sv", "truly_undefined_macro")];
        let known = HashSet::from(["uvm_field_utils_begin".to_string()]);

        let plan = plan_slang_publishes(std::slice::from_ref(&url_a), diags, &HashSet::new(), &known, &DiagnosticPolicy::default());

        let a_pub = plan.publishes.iter().find(|(u, _)| u == &url_a).unwrap();
        assert_eq!(a_pub.1.len(), 1, "unrecognized macro diagnostic should pass through");
    }

    /// `extract_macro_name` correctly strips the backtick prefix.
    #[test]
    fn extract_macro_name_strips_backtick() {
        assert_eq!(
            extract_macro_name("unknown macro or compiler directive '`uvm_field_utils_begin'"),
            Some("uvm_field_utils_begin"),
        );
    }

    /// `extract_macro_name` returns `None` for unrecognised message formats.
    #[test]
    fn extract_macro_name_none_on_bad_format() {
        assert_eq!(extract_macro_name("some other error"), None);
    }

    /// `remove_pending_generation` removes only the entry owned by the
    /// finishing task's generation: a stale task can't evict the newer
    /// task that replaced it in the debounce map.
    #[tokio::test]
    async fn remove_pending_generation_respects_ownership() {
        let pending: RwLock<PendingMap> = RwLock::new(HashMap::new());
        let uri = Url::parse("file:///proj/a.sv").unwrap();

        // Entry currently belongs to generation 2 (a newer schedule).
        let newer = tokio::spawn(async {});
        pending.write().await.insert(uri.clone(), (2, newer));

        // The finishing generation-1 task must leave it alone…
        remove_pending_generation(&pending, &uri, 1).await;
        assert!(pending.read().await.contains_key(&uri), "stale task evicted its successor");

        // …while the owning generation-2 task removes it.
        remove_pending_generation(&pending, &uri, 2).await;
        assert!(!pending.read().await.contains_key(&uri));
    }
}
