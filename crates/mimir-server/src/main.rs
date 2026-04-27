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

mod backend;
mod project;

/// Environment variable: filesystem path to the slang sidecar binary. When
/// set and the spawn succeeds, `mimir-server` consults the sidecar for
/// elaboration-driven diagnostics. When unset (today's default) or the
/// spawn fails, the server falls back to tree-sitter-only mode and logs
/// the reason so the user knows why deeper diagnostics are quiet.
pub const SLANG_PATH_ENV: &str = "MIMIR_SLANG_PATH";

#[tokio::main]
async fn main() {
    // Logging goes to stderr — see `mimir_core::logging` for the rationale.
    // We swallow the error if a subscriber is already installed (e.g. if a
    // test harness wired one up), because there's nothing actionable to do.
    let _ = mimir_core::logging::init();

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
    let (service, socket) =
        LspService::new(move |client| backend::Backend::new(client, slang));
    Server::new(stdin, stdout, socket).serve(service).await;

    tracing::info!("mimir-server shutting down");
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
