//! # `mimir-syntax` — SystemVerilog parsing for Mimir
//!
//! This crate is a thin wrapper around [`tree-sitter`] and the
//! `tree-sitter-verilog` grammar. It owns three responsibilities:
//!
//! 1. **Construct parsers.** [`SyntaxParser`] holds a `tree_sitter::Parser`
//!    pre-loaded with the SystemVerilog language.
//! 2. **Parse and re-parse documents.** [`SyntaxParser::parse`] turns a
//!    string of source into a [`SyntaxTree`]. We hold on to the previous
//!    tree to enable tree-sitter's incremental reparse.
//! 3. **Extract parse-error diagnostics.** [`diagnostics::collect`] walks
//!    the syntax tree and produces one [`Diagnostic`] per `ERROR` or
//!    `MISSING` node.
//!
//! ## Why tree-sitter?
//!
//! The user picked tree-sitter for the incremental story: rather than
//! re-parsing the whole file on every keystroke, tree-sitter takes the
//! previous tree + a list of edits and produces a new tree in time
//! proportional to the size of the edit. That's what makes the LSP "feel
//! responsive" on multi-thousand-line UVM testbenches.
//!
//! Trade-off: the SV grammar is incomplete around advanced constructs
//! (some SVA, complex constraints, exotic UVM macros). When we hit gaps we
//! get `ERROR` nodes — they don't crash us, they just turn into "syntax
//! error here" diagnostics. Long-term we'll either upstream grammar fixes
//! or layer a deeper parser (sv-parser / slang) on top for analysis.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod diagnostics;
pub mod parser;

pub use diagnostics::{Diagnostic, DiagnosticSeverity};
pub use parser::{SyntaxParser, SyntaxParserError, SyntaxTree};
