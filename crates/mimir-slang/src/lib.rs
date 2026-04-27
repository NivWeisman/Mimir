//! # `mimir-slang` — async client for the slang sidecar
//!
//! tree-sitter is great for incremental UI-tier syntax (highlighting, folds,
//! rough error spans) but it has no SystemVerilog preprocessor and no
//! semantic layer. On real verification code that means false positives:
//! every `` `include `` inside a `package` body produces a cascading
//! `missing endpackage` (see the apb.sv investigation in
//! `crates/mimir-syntax/src/lib.rs` doc comments).
//!
//! This crate is the Rust half of mimir's answer to that gap. It speaks to a
//! long-lived **sidecar process** built around the
//! [slang](https://github.com/MikePopoloski/slang) C++ library, which owns
//! the preprocessor + parser + elaborator. The sidecar is a separate binary
//! shipped per platform (kept out of `cargo build` so contributors don't
//! need a C++ toolchain to hack on the Rust side).
//!
//! ## Two layers, separated for tests
//!
//! * [`Connection`] — the framing layer. Owns nothing more than an async
//!   reader and writer; lets tests use [`tokio::io::duplex`] instead of
//!   spawning a process.
//! * [`Client`] — the process layer. Spawns the sidecar binary, owns its
//!   `Child`, and forwards calls into a [`Connection`] over the child's
//!   stdio.
//!
//! ## Wire protocol
//!
//! NDJSON: one JSON object per line, request OR response. JSON-RPC-shaped
//! (`id`, `method`, `params` for requests; `id`, `result` xor `error` for
//! responses) but without the `jsonrpc` field — we don't need the
//! interoperability JSON-RPC was invented to provide. See [`protocol`] for
//! the exact shapes.
//!
//! ## What this crate is *not*
//!
//! * It does **not** ship the sidecar binary or build slang. The C++ shim
//!   lives in `slang-sidecar/` at the workspace root (Stage 1).
//! * It does **not** read project config (`.f` filelists, `.mimir.toml`).
//!   That's [`mimir-server`]'s job; this crate only knows how to send a
//!   fully-resolved [`protocol::ElaborateParams`] over the wire (Stage 2).
//! * It does **not** debounce or schedule re-elaboration. That belongs to
//!   the server's diagnostic pipeline (Stage 3).

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod client;
pub mod protocol;

pub use client::{Client, ClientError, Connection, ConnectionError};
pub use protocol::{
    Diagnostic, ElaborateParams, ElaborateResult, MacroDefine, Severity, SourceFile,
};
