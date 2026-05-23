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
//! 4. **Extract a symbol index.** [`symbols::index`] walks the tree and
//!    emits one [`Symbol`] per declaration; [`symbols::identifier_at`]
//!    looks up the identifier under an LSP position. Powers
//!    `documentSymbol` and same-file go-to-definition in `mimir-server`.
//! 5. **IEEE 1800-2017 keyword list.** [`keywords::KEYWORDS`] and
//!    [`keywords::matches_prefix`] provide a static list of all SV reserved
//!    words for completion (no tree required).
//! 6. **Foldable region extraction.** [`folding::folding_ranges`] walks the
//!    tree and emits one [`FoldRange`] per top-level construct (module,
//!    class, function, task, package, …). Powers `textDocument/foldingRange`
//!    in `mimir-server`.
//! 7. **Semantic-token classifier.** [`semantic_tokens::semantic_tokens`]
//!    walks the tree and classifies every keyword, identifier, type,
//!    string, number, and comment into a stable legend. Powers
//!    `textDocument/semanticTokens` (full + range) in `mimir-server`.
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

pub mod builtin_methods;
pub mod calls;
pub mod diagnostics;
pub mod folding;
pub mod hover_format;
pub mod inlay;
pub mod keywords;
pub mod parser;
pub mod semantic_tokens;
pub mod signature;
pub mod symbols;

pub use calls::{ArgSpan, CallKind, CallSite, EnclosingCallable};
pub use diagnostics::{Diagnostic, DiagnosticSeverity};
pub use folding::FoldRange;
pub use inlay::{InlayLabel, MethodHintMode};
pub use parser::{SyntaxParser, SyntaxParserError, SyntaxTree};
pub use signature::{ParamInfo, SignatureInfo};
pub use symbols::{Param, Symbol, SymbolKind};
