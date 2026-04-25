# Architectural patterns

Patterns that recur across Mimir's codebase. Read before adding a new module —
these are conventions to follow, not suggestions.

---

## 1. Cargo workspace with hoisted dependency versions

The root `Cargo.toml` declares every dependency once under
`[workspace.dependencies]` ([Cargo.toml:31-69](../../Cargo.toml)). Each crate
opts in via `dep.workspace = true` ([crates/mimir-syntax/Cargo.toml:14-21](../../crates/mimir-syntax/Cargo.toml)).
Crate-level package metadata (`version`, `edition`, `rust-version`, `license`,
…) is also inherited via `{ workspace = true }`.

**Why:** one place to bump a version; impossible to drift between crates.
**Apply to new crates:** copy the metadata stanza from any existing crate's
`Cargo.toml` and only add new `[dependencies]` if they're already in
`[workspace.dependencies]` (add them there first).

---

## 2. Co-located unit tests, never separate test files

Every source file with logic ends in a `#[cfg(test)] mod tests { ... }` block.
Examples:
[crates/mimir-core/src/document.rs:339](../../crates/mimir-core/src/document.rs),
[crates/mimir-core/src/logging.rs:82](../../crates/mimir-core/src/logging.rs),
[crates/mimir-syntax/src/parser.rs:128](../../crates/mimir-syntax/src/parser.rs),
[crates/mimir-syntax/src/diagnostics.rs:150](../../crates/mimir-syntax/src/diagnostics.rs),
[crates/mimir-server/src/backend.rs:310](../../crates/mimir-server/src/backend.rs).

**Why:** tests stay next to the code they exercise; refactoring one moves the
other; no `tests/` directory to forget.
**Apply:** new public function → at least one test in the same file's
`mod tests`. Reach for a `tests/` dir only for true cross-module integration
tests (none exist today).

---

## 3. Module-level rustdoc on every file

Every `lib.rs` and submodule starts with a `//!` block explaining purpose,
design choices, and trade-offs. The user explicitly required heavy
documentation; this is enforced by `missing_docs = "warn"` (lint configured at
the package level in each `Cargo.toml`, e.g.
[crates/mimir-core/Cargo.toml:13](../../crates/mimir-core/Cargo.toml)) and
`#![warn(missing_docs)]` in each `lib.rs` (e.g.
[crates/mimir-core/src/lib.rs:30](../../crates/mimir-core/src/lib.rs)).

**Apply:** new file → top-of-file `//!` block. New `pub` item → rustdoc on it.
Comments explain *why*, not *what*.

---

## 4. Typed errors with `thiserror`, propagated with `?`

Each crate that can fail exposes one `pub enum *Error` deriving `thiserror::Error`:
- `TextDocumentError` — [crates/mimir-core/src/document.rs:33](../../crates/mimir-core/src/document.rs)
- `SyntaxParserError` — [crates/mimir-syntax/src/parser.rs:23](../../crates/mimir-syntax/src/parser.rs)

Variants carry context fields (`line`, `total_lines`, etc.), not just strings,
so callers can match on them and the `Display` impl reads cleanly in logs.

**Apply:** new fallible operation → add a variant to the existing crate-level
error or create a new `*Error` enum if the failure modes don't belong with the
existing one. Don't return `String` or `Box<dyn Error>` from public APIs.

---

## 5. Mirror LSP types instead of leaking `lsp_types`

`mimir-syntax` defines its own `Diagnostic` and `DiagnosticSeverity`
([crates/mimir-syntax/src/diagnostics.rs:21-44](../../crates/mimir-syntax/src/diagnostics.rs))
that mirror the LSP shapes but live in our crate. Conversion to the wire
format happens at the boundary in
[crates/mimir-server/src/backend.rs:272](../../crates/mimir-server/src/backend.rs).

**Why:** keeps `lsp_types`/`tower-lsp` out of the parser/core crates, so they
stay testable without an async runtime and reusable outside the LSP context.
**Apply:** when `mimir-server` needs a new shape from a downstream crate
(hovers, completions, …), define the canonical type in the producing crate
and convert at the server boundary.

---

## 6. Re-export the public API at the crate root

Each `lib.rs` ends with `pub use submodule::{Type1, Type2}`:
[crates/mimir-core/src/lib.rs:36](../../crates/mimir-core/src/lib.rs),
[crates/mimir-syntax/src/lib.rs:35-36](../../crates/mimir-syntax/src/lib.rs).

**Why:** callers write `mimir_core::Position`, not
`mimir_core::document::Position`. Internal module reorganization doesn't
break consumers.

---

## 7. UTF-8 storage, UTF-16 conversion at the LSP boundary

LSP wire positions are UTF-16 code unit offsets; Rust strings are UTF-8.
Conversion lives in `Position::to_byte_offset` /
`Position::from_byte_offset`
([crates/mimir-core/src/document.rs:100,159](../../crates/mimir-core/src/document.rs)).
Never do conversion ad-hoc inside `mimir-server` — always go through these.

**Apply:** when a new LSP request gives you `(line, character)`, convert via
`Position` exactly once and work in bytes after that.

---

## 8. Single mutable resource behind `tokio::sync::Mutex`; document store behind `RwLock`

`tree_sitter::Parser` is not `Sync`, so the server holds one parser behind a
`Mutex` ([crates/mimir-server/src/backend.rs:75](../../crates/mimir-server/src/backend.rs)).
The document store is `RwLock` because reads (parse callbacks) outnumber
writes (edits) ([crates/mimir-server/src/backend.rs:67](../../crates/mimir-server/src/backend.rs)).

**Lock-then-clone-then-work:** reparse pulls the source text under a read
lock, drops the lock, then parses
([crates/mimir-server/src/backend.rs:99-126](../../crates/mimir-server/src/backend.rs)).
Never hold a lock across a slow operation.

---

## 9. `#[instrument]` on async / IO entry points

Every `async fn` that handles an LSP request gets
`#[instrument(level = "debug", skip_all, fields(uri = %...))]`. See
[crates/mimir-server/src/backend.rs:182,207,253](../../crates/mimir-server/src/backend.rs)
and [crates/mimir-syntax/src/parser.rs:104](../../crates/mimir-syntax/src/parser.rs).

**Why:** spans show up in `tracing` output with the URI attached, so debug
logs are filterable by document. `skip_all` keeps large `params` structs out
of the log.

---

## 10. Logs to stderr only, structured fields not strings

`tracing` is pinned to stderr because stdout carries JSON-RPC
([crates/mimir-core/src/logging.rs:5,52](../../crates/mimir-core/src/logging.rs)).
Use `debug!(field = value, other = %display_thing, "message")`, not formatted
strings — the fields become queryable in tracing's structured output.

**Apply:** never `println!` / `eprintln!`. Never put dynamic data inside the
message string when it could be a field.
