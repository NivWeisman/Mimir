//! `mimir-server` — entrypoint for the SystemVerilog LSP binary.
//!
//! The binary is intentionally trivial: install logging, build the
//! tower-lsp service, and start serving on stdio. All real logic lives in
//! [`backend::Backend`] (in `backend.rs`) so it can be unit-tested without
//! a real client.
//!
//! ## Running
//!
//! ```bash
//! RUST_LOG=mimir=debug cargo run --release -p mimir-server
//! ```
//!
//! Editors should launch this binary and pipe LSP messages over its
//! stdin/stdout. See `editors/` for examples.

use std::sync::Arc;

use tower_lsp::{LspService, Server};

mod ast_features;
mod backend;
mod chain_resolve;
mod code_lens;
mod completion_score;
mod diag_policy;
mod diagnostics;
mod elaborate_service;
mod filelist;
mod format;
mod hierarchy_features;
mod hover_features;
mod includes;
mod lsp_convert;
mod member_features;
mod parse_provider;
mod paths;
mod project;
mod references_features;
mod slang_adapter;
mod slang_service;
mod source_io;
mod syntax_service;
mod uvm_db_features;
mod workspace_index;
mod workspace_symbols;

/// Environment variable: filesystem path to the slang sidecar binary. When
/// set and the spawn succeeds, `mimir-server` consults the sidecar for
/// elaboration-driven diagnostics. When unset (today's default) or the
/// spawn fails, the server falls back to tree-sitter-only mode and logs
/// the reason so the user knows why deeper diagnostics are quiet.
pub const SLANG_PATH_ENV: &str = "MIMIR_SLANG_PATH";

/// Environment variable that enables fault-injection requests used by the
/// integration tests (currently just `mimir/debug/panic`). Never set it in
/// an editor configuration.
pub const DEBUG_HOOKS_ENV: &str = "MIMIR_DEBUG_HOOKS";

#[tokio::main]
async fn main() {
    // Logging goes to stderr — see `mimir_core::logging` for the rationale.
    // We swallow the error if a subscriber is already installed (e.g. if a
    // test harness wired one up), because there's nothing actionable to do.
    let _ = mimir_core::logging::init();

    // Install a panic hook that routes the panic message and backtrace through
    // `tracing` so it lands on stderr (and thus in the editor's LSP output
    // channel) before the process exits.  Without this, panics are silent from
    // the editor's perspective, making crashes very hard to diagnose.
    // `RUST_BACKTRACE=1` (or "full") must be set for the backtrace to be
    // populated; editors can set it via their server-env configuration.
    //
    // The hook also *ends the process* when the panic is on the main thread.
    // Every LSP handler is polled there (it is the thread inside
    // `block_on`), so a panic on it means the request loop is gone. Left to
    // unwind, the runtime's shutdown then waits for its blocking-pool
    // threads — one of which is parked in a `read` on stdin — and the
    // process lingers indefinitely: no responses, no exit, sidecar children
    // still running, and the editor never sees the "server died" event that
    // makes it restart us. Exiting is the recovery path.
    //
    // Panics on other threads (background tasks — tokio contains those to
    // the task) are only logged.
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!(
            panic = %info,
            backtrace = %backtrace,
            "mimir-server panicked — please report this at \
             https://github.com/nvweisman/mimir/issues",
        );
        if panic_is_fatal(std::thread::current().name()) {
            // 101 is what an unwinding Rust `main` exits with. stderr is
            // unbuffered, so the log line above is already out.
            std::process::exit(101);
        }
    }));

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "mimir-server starting on stdio",
    );

    // Try to spawn the slang sidecar if the user pointed us at one. A
    // missing env var, a bad path, or a sidecar that fails to start all
    // resolve to `None` and the server keeps running on tree-sitter alone
    // — this path is critical because the C++ sidecar binary doesn't
    // exist yet (Stage 1) and we must not regress today's behavior.
    let slang = spawn_slang_if_configured().await;

    // tower-lsp's `Server::new(stdin, stdout, socket)` expects an async
    // reader and writer. `tokio::io::stdin/stdout` are line-buffered async
    // wrappers around the OS streams.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // `LspService::new` takes a closure that gets the `Client` handle and
    // returns our `Backend`. The `Client` is how we send notifications back
    // to the editor (e.g. `publishDiagnostics`). We move the optional
    // slang client into the closure so it ends up owned by the `Backend`.
    // `build(...).custom_method(...)` registers the non-standard requests
    // alongside the standard LSP surface:
    //   * `mimir/expandMacro` — rust-analyzer-style macro expansion; the
    //     VS Code extension sends it via `client.sendRequest(...)`.
    //   * `mimir/uvmDb` — workspace-wide uvm_config_db / uvm_resource_db
    //     call listing; feeds the extension's UVM DB tree view.
    let (service, socket) = LspService::build(move |client| backend::Backend::new(client, slang))
        .custom_method("mimir/expandMacro", backend::Backend::expand_macro)
        .custom_method("mimir/uvmDb", backend::Backend::uvm_db)
        .custom_method("mimir/debug/panic", backend::Backend::debug_panic)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;

    tracing::info!("mimir-server shutting down");
}

/// Whether a panic on the thread named `thread_name` takes the LSP request
/// loop down with it (see the panic hook in [`main`]): true for the main
/// thread, false for runtime worker / blocking-pool threads.
fn panic_is_fatal(thread_name: Option<&str>) -> bool {
    thread_name == Some("main")
}

/// Read [`SLANG_PATH_ENV`] and spawn the sidecar if set. Logs at info on
/// success, at warn on failure, and returns `None` in either failure case.
///
/// Kept out of `main` so the spawn-and-log policy is testable in isolation
/// once we have an integration harness — today this just keeps `main`
/// readable.
async fn spawn_slang_if_configured() -> Option<Arc<mimir_slang::Client>> {
    let path = std::env::var_os(SLANG_PATH_ENV)?;
    match mimir_slang::Client::spawn(&path, std::iter::empty::<&str>()).await {
        Ok(client) => {
            tracing::info!(path = ?path, "slang sidecar spawned");
            Some(Arc::new(client))
        }
        Err(e) => {
            // Don't crash the server — tree-sitter still works. Surface
            // the reason so the user can see in the server's stderr what
            // went wrong (most often: bad path, or sidecar binary not
            // built yet).
            tracing::warn!(
                path = ?path,
                error = %e,
                "could not spawn slang sidecar; continuing without",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a main-thread panic ends the process; background-task panics on
    /// runtime threads are survivable and merely logged.
    #[test]
    fn only_main_thread_panics_are_fatal() {
        assert!(panic_is_fatal(Some("main")));
        assert!(!panic_is_fatal(Some("tokio-runtime-worker")));
        assert!(!panic_is_fatal(None));
    }
}
