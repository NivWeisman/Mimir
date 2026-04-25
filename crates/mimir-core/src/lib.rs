//! # `mimir-core` — shared primitives for the Mimir SystemVerilog LSP
//!
//! This crate is intentionally small and dependency-light. It contains the
//! types that *everyone* in the workspace touches:
//!
//! * [`Position`] / [`Range`] — UTF-16 line/column pairs, the unit the LSP
//!   protocol uses on the wire.
//! * [`TextDocument`] — a rope-backed in-memory document with O(log n)
//!   incremental edits.
//! * [`logging`] — a single function to wire up `tracing` to stderr (LSP
//!   forbids logging to stdout, which carries JSON-RPC traffic).
//!
//! ## Why a separate crate?
//!
//! `mimir-syntax` (parser) and `mimir-server` (LSP backend) both need a
//! shared definition of "what is a document" and "what is a position". By
//! pulling those into a leaf crate, neither downstream crate has to depend
//! on the other. The dependency graph stays a DAG and tests stay fast.
//!
//! ## A word on UTF-8 vs UTF-16
//!
//! The LSP protocol historically uses **UTF-16 code units** for column
//! positions, not bytes and not Unicode codepoints. This is a wart but a
//! mandatory one — VS Code's line counts are computed in UTF-16. We do all
//! storage in UTF-8 (Rust's native string encoding) and convert at the
//! boundary. See [`document::Position::to_byte_offset`] for the conversion.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod document;
pub mod logging;

// Re-export the most commonly-used items at the crate root so callers can
// write `mimir_core::Position` instead of `mimir_core::document::Position`.
pub use document::{Position, Range, TextDocument, TextDocumentError};
