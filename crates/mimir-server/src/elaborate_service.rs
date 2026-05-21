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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mimir_slang::{
    Diagnostic as SlangDiagnostic, ElaborateResult, Severity as SlangSeverity,
};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Url,
};
use tower_lsp::Client;
use tracing::{debug, error, info, warn};

use crate::slang_service::{path_to_url, SlangService};

// --------------------------------------------------------------------------
// Internal types
// --------------------------------------------------------------------------

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

/// Elaborate lifecycle: debounce → dedup → sidecar call → diagnostic publish.
///
/// Holds the same `Arc<SlangService>` as `Backend` (no extra data copies) and
/// a `Client` clone for pushing diagnostics.
pub(crate) struct ElaborateService {
    /// Sidecar IPC + param assembly — shared with `Backend`.
    slang: Arc<SlangService>,
    /// Channel for pushing `publishDiagnostics` back to the editor.
    client: Client,
    /// One in-flight (sleeping or running) elaborate task per trigger URI.
    /// Aborted and replaced on every new edit for the same URI — the debounce.
    pending: Arc<RwLock<HashMap<Url, JoinHandle<()>>>>,
    /// Hash of inputs sent to the last *successful* elaborate. The same
    /// hash on the next call → skip the sidecar entirely.
    last_hash: Arc<RwLock<Option<u64>>>,
    /// URLs we published non-empty slang diagnostics to last cycle.
    /// Diffed each cycle so stale squiggles are cleared when errors are fixed.
    published: Arc<RwLock<HashSet<Url>>>,
    /// Latched `true` after the first successful elaborate logs its
    /// per-file "indexed by startup slang elaborate" messages.
    startup_logged: Arc<AtomicBool>,
}

impl ElaborateService {
    /// Construct the service from the shared `SlangService` Arc and a
    /// `Client` clone. Both are cheap (reference-counted); no data is copied.
    pub(crate) fn new(slang: Arc<SlangService>, client: Client) -> Self {
        Self {
            slang,
            client,
            pending: Arc::new(RwLock::new(HashMap::new())),
            last_hash: Arc::new(RwLock::new(None)),
            published: Arc::new(RwLock::new(HashSet::new())),
            startup_logged: Arc::new(AtomicBool::new(false)),
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
        let debounce = match self.slang.current_debounce().await {
            Some(d) => d,
            None => return,
        };

        {
            let mut pending = self.pending.write().await;
            if let Some(prior) = pending.remove(&trigger_uri) {
                prior.abort();
                debug!(uri = %trigger_uri, "cancelled prior pending elaborate");
            }
        }

        let slang_svc = self.slang.clone();
        let pending = self.pending.clone();
        let published = self.published.clone();
        let last_hash = self.last_hash.clone();
        let startup_logged = self.startup_logged.clone();
        let lsp_client = self.client.clone();
        let trigger_for_task = trigger_uri.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(debounce).await;

            let Some((params, files_in_request)) = slang_svc.build_elaborate_params().await else {
                pending.write().await.remove(&trigger_for_task);
                return;
            };

            let input_hash = SlangService::hash_inputs(&params);
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
            match slang_svc.elaborate(&params).await {
                Ok(result) => {
                    publish_slang_result(&lsp_client, &files_in_request, result, &published)
                        .await;
                    *last_hash.write().await = Some(input_hash);

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
                    error!(error = %e, "slang elaborate failed");
                }
            }

            pending.write().await.remove(&trigger_for_task);
        });

        self.pending.write().await.insert(trigger_uri, handle);
    }

    /// Reset the input hash so the next [`schedule`] call always runs the
    /// sidecar, even if the file contents haven't changed.
    ///
    /// Call this after a project reload where the filelist or include dirs
    /// may have changed without touching any open buffer.
    #[allow(dead_code)]
    pub(crate) async fn invalidate_hash(&self) {
        *self.last_hash.write().await = None;
    }
}

// --------------------------------------------------------------------------
// Diagnostic helpers
// --------------------------------------------------------------------------

/// Convert a slang [`SlangDiagnostic`] to its LSP [`Diagnostic`] shape.
///
/// `source` stays `"mimir"` so editors don't need two filter labels;
/// `code` carries slang's stable diagnostic code (e.g. `"UnknownModule"`)
/// so editors can group or filter per-code.
pub(crate) fn slang_to_lsp_diagnostic(d: SlangDiagnostic) -> Diagnostic {
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

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::{Position as MPosition, Range as MRange};
    use mimir_slang::ElaborateResult;

    fn slang_diag_at(path: &str, code: &str) -> SlangDiagnostic {
        SlangDiagnostic {
            path: path.into(),
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            severity: SlangSeverity::Error,
            code: code.into(),
            message: "boom".into(),
        }
    }

    /// slang → LSP conversion preserves the slang-specific code string and
    /// keeps the same `source` label as syntax diagnostics.
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
        for (sev, expected) in cases {
            let d = SlangDiagnostic {
                path: "a.sv".into(),
                range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
                severity: sev,
                code: "X".into(),
                message: "m".into(),
            };
            assert_eq!(slang_to_lsp_diagnostic(d).severity, Some(expected));
        }
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
        let result = ElaborateResult { diagnostics: vec![] };
        let prev = HashSet::from([url_dropped.clone()]);

        let plan = plan_slang_publishes(&[url_a.clone()], result, &prev);

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
        assert!(plan
            .publishes
            .iter()
            .any(|(u, d)| u == &inc_url && d.len() == 1));
        assert!(plan.new_published.contains(&inc_url));
    }
}
