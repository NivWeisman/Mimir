//! Backend-agnostic AST types for the Mimir SystemVerilog language server.
//!
//! Any semantic backend (slang, verilator, a future in-house elaborator)
//! converts its native representation into these types. All LSP feature logic
//! in `mimir-server` operates exclusively on `MimirAst`, keeping the feature
//! implementation independent of the active backend.
//!
//! # Design invariants
//!
//! * No `tokio`, no `tower-lsp`, no parser dependencies — pure data only.
//! * All positions use UTF-16 code units (`character` field), matching the LSP
//!   wire format. Backends must convert before populating these types.
//! * Serde derives are always enabled (not feature-gated) because serialization
//!   is a first-class concern: the slang sidecar emits JSON, and the adapter
//!   deserializes it into these types.

pub mod types;

pub use types::{
    DeclKind, DiagSeverity, MimirAst, MimirDecl, MimirDiag, MimirFile, MimirPos, MimirRange,
    MimirRef, MimirScope, Visibility,
};
