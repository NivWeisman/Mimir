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

use tower_lsp::{LspService, Server};

mod backend;

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

    // tower-lsp's `Server::new(stdin, stdout, socket)` expects an async
    // reader and writer. `tokio::io::stdin/stdout` are line-buffered async
    // wrappers around the OS streams.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // `LspService::new` takes a closure that gets the `Client` handle and
    // returns our `Backend`. The `Client` is how we send notifications back
    // to the editor (e.g. `publishDiagnostics`).
    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;

    tracing::info!("mimir-server shutting down");
}
