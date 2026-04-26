# CLAUDE.md

`mimir` is an **incremental, async SystemVerilog Language Server in Rust**,
focused on **verification** code (UVM, SVA, functional coverage, constraints)
rather than synthesis-only RTL. Editors launch the binary; the server speaks
LSP/JSON-RPC over stdio.

## Status

Early development. Document sync + parse-error diagnostics ship today;
everything else is in [README.md](./README.md) under "Feature checklist".
That checklist is canonical — flip an item in the same change that lands the
feature.

## Tech stack

| Layer        | Crate / tool                                      | Pinned at                                           |
| ------------ | ------------------------------------------------- | --------------------------------------------------- |
| Async / RT   | `tokio`                                           | [Cargo.toml:36](./Cargo.toml)                       |
| LSP          | `tower-lsp` 0.20                                  | [Cargo.toml:35](./Cargo.toml)                       |
| Parser       | `tree-sitter` 0.24 + `tree-sitter-verilog` 1.0    | [Cargo.toml:48-49](./Cargo.toml)                    |
| Text buffer  | `ropey`                                           | [Cargo.toml:42](./Cargo.toml)                       |
| Logging      | `tracing` + `tracing-subscriber`                  | [Cargo.toml:55-56](./Cargo.toml)                    |
| Errors       | `thiserror` (typed) / `anyhow` (top-level glue)   | [Cargo.toml:59-60](./Cargo.toml)                    |
| Toolchain    | Rust stable, MSRV 1.75                            | [rust-toolchain.toml](./rust-toolchain.toml)        |

## Workspace layout

| Path                         | Role                                                                                   |
| ---------------------------- | -------------------------------------------------------------------------------------- |
| `crates/mimir-core/`         | Pure-data: `TextDocument` (rope), `Position`/`Range` math, tracing init. No tokio, no parser. |
| `crates/mimir-syntax/`       | tree-sitter wrapper: `SyntaxParser`, parse-error / `MISSING`-node diagnostic extraction.       |
| `crates/mimir-server/`       | tower-lsp `Backend` + `mimir-server` binary. Owns the document store and LSP wire glue.        |
| `editors/vscode/`            | TypeScript extension client. Spawns the binary; no real logic.                         |
| `editors/emacs/init.el`      | eglot / lsp-mode config snippet.                                                       |

Dependencies flow strictly downward:
`mimir-server → {mimir-syntax, mimir-core}`, `mimir-syntax → mimir-core`.
Don't introduce cycles; don't pull `tower-lsp` or `tokio` into the lower crates.

## Build, test, lint

```bash
cargo check  --workspace
cargo test   --workspace                   # 19 unit tests today
cargo clippy --workspace -- -D warnings
cargo build  --release                     # produces target/release/mimir-server
```

Run the server attached to a terminal (mostly for hand-fed JSON-RPC
debugging — it'll just sit on stdin otherwise):

```bash
RUST_LOG=mimir=debug cargo run -p mimir-server
```

Per-crate test entry points:
- `mimir-core`: [crates/mimir-core/src/document.rs:339](./crates/mimir-core/src/document.rs), [crates/mimir-core/src/logging.rs:82](./crates/mimir-core/src/logging.rs)
- `mimir-syntax`: [crates/mimir-syntax/src/parser.rs:128](./crates/mimir-syntax/src/parser.rs), [crates/mimir-syntax/src/diagnostics.rs:150](./crates/mimir-syntax/src/diagnostics.rs)
- `mimir-server`: [crates/mimir-server/src/backend.rs:310](./crates/mimir-server/src/backend.rs)

## Critical invariants

- **Logs go to stderr only.** stdout carries JSON-RPC; one stray `println!`
  corrupts the protocol. See [crates/mimir-core/src/logging.rs:5](./crates/mimir-core/src/logging.rs).
- **LSP positions are UTF-16 code units, not bytes.** Internal storage is
  UTF-8. Convert exactly once at the boundary via `Position::to_byte_offset` /
  `from_byte_offset` ([crates/mimir-core/src/document.rs:100](./crates/mimir-core/src/document.rs)).
- **`tree_sitter::Parser` is not `Sync`.** The server keeps one behind a
  `tokio::sync::Mutex` ([crates/mimir-server/src/backend.rs:75](./crates/mimir-server/src/backend.rs)).
  Don't try to share without the lock.
- **Heavy comments are a product requirement.** `missing_docs = "warn"` is on
  in every crate. Don't strip module-level `//!` blocks.

## When making changes

- New public function → add a `#[cfg(test)] mod tests` test in the same file.
- New fallible op → extend the crate's existing `*Error` enum (`thiserror`).
- Logging → `tracing::{debug, info, warn, error}` with structured fields, not
  formatted strings. Never `println!` / `eprintln!`.
- New LSP feature → flip its checklist item in [README.md](./README.md) in
  the same commit.
- Ask me questions to clarify the product requirements, technical requirements, engineering principles, and hard constraints.

## Additional documentation

Read these when the topic comes up:

- [.claude/docs/architectural_patterns.md](./.claude/docs/architectural_patterns.md)
  — recurring patterns (workspace dep hoisting, co-located tests, typed
  errors, mirror-LSP-types, lock-then-clone, instrument-on-IO, etc.). **Read
  before adding a new module or crate.**
- [README.md](./README.md) — user-facing requirements, install/setup, and the
  live feature checklist.
- [editors/vscode/README.md](./editors/vscode/README.md) — VS Code extension
  dev workflow.
- [editors/emacs/README.md](./editors/emacs/README.md) — Emacs setup notes.
