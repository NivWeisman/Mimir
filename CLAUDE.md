# CLAUDE.md

`mimir` is an **incremental, async SystemVerilog Language Server in Rust**,
focused on **verification** code (UVM, SVA, functional coverage, constraints)
rather than synthesis-only RTL. Editors launch the binary; the server speaks
LSP/JSON-RPC over stdio.

## Status

Active development. The core LSP skeleton (document sync, parse-error
diagnostics, semantic tokens, hover, completion, signature help, go-to-definition,
declaration, type-definition, implementation, references, rename, document
highlights, document symbols, workspace symbols, inlay hints, folding ranges,
formatting, call hierarchy, and type hierarchy) are all shipped. Everything
else is tracked in [README.md](./README.md) under "Feature checklist".
That checklist is canonical — flip an item in the same change that lands the
feature.

## Tech stack

| Layer        | Crate / tool                                      | Pinned at                                           |
| ------------ | ------------------------------------------------- | --------------------------------------------------- |
| Async / RT   | `tokio`                                           | [Cargo.toml:36](./Cargo.toml)                       |
| LSP          | `tower-lsp` 0.20                                  | [Cargo.toml:35](./Cargo.toml)                       |
| Parser       | `tree-sitter` 0.25 + `tree-sitter-systemverilog` 0.3.1 | [Cargo.toml:48-49](./Cargo.toml)               |
| Text buffer  | `ropey`                                           | [Cargo.toml:42](./Cargo.toml)                       |
| Logging      | `tracing` + `tracing-subscriber`                  | [Cargo.toml:55-56](./Cargo.toml)                    |
| Errors       | `thiserror` (typed) / `anyhow` (top-level glue)   | [Cargo.toml:59-60](./Cargo.toml)                    |
| Toolchain    | Rust stable, MSRV 1.75                            | [rust-toolchain.toml](./rust-toolchain.toml)        |

## Workspace layout

| Path                         | Role                                                                                        |
| ---------------------------- | ------------------------------------------------------------------------------------------- |
| `crates/mimir-core/`         | Pure-data: `TextDocument` (rope), `Position`/`Range` math, tracing init. No tokio, no parser.|
| `crates/mimir-syntax/`       | tree-sitter wrapper: `SyntaxParser`, parse-error / `MISSING`-node diagnostics, symbols, semantic tokens, calls, inlay, folding, hover, keywords.|
| `crates/mimir-slang/`        | Async client for the C++ slang sidecar. Owns process lifecycle, NDJSON framing, and typed protocol types. No parser, no LSP.|
| `crates/mimir-ast/`          | Backend-agnostic AST types (`MimirAst`, `MimirDecl`, `MimirDiag`, `MimirScope`, …). Pure data + serde, no I/O.|
| `crates/mimir-server/`       | tower-lsp `Backend` + `mimir-server` binary. Owns the document store and LSP wire glue.     |
| `editors/vscode/`            | TypeScript extension client. Spawns the binary; no real logic.                              |
| `editors/emacs/init.el`      | eglot / lsp-mode config snippet.                                                            |

Dependencies flow strictly downward:
`mimir-server → {mimir-syntax, mimir-slang, mimir-ast, mimir-core}`,
`mimir-syntax → mimir-core`.
`mimir-slang` and `mimir-ast` have no deps on other workspace crates.
Don't introduce cycles; don't pull `tower-lsp` or `tokio` into the lower crates.

## Build, test, lint

```bash
cargo check  --workspace
cargo test   --workspace                   # 436 unit tests today
cargo clippy --workspace -- -D warnings
cargo build  --release                     # produces target/release/mimir-server
```

Run the server attached to a terminal (mostly for hand-fed JSON-RPC
debugging — it'll just sit on stdin otherwise):

```bash
RUST_LOG=mimir=debug cargo run -p mimir-server
```

Per-crate test entry points (unit tests co-located with source):
- `mimir-core`: [crates/mimir-core/src/document.rs](./crates/mimir-core/src/document.rs), [crates/mimir-core/src/logging.rs](./crates/mimir-core/src/logging.rs)
- `mimir-syntax`: [crates/mimir-syntax/src/parser.rs](./crates/mimir-syntax/src/parser.rs), [crates/mimir-syntax/src/diagnostics.rs](./crates/mimir-syntax/src/diagnostics.rs), [crates/mimir-syntax/src/symbols.rs](./crates/mimir-syntax/src/symbols.rs), [crates/mimir-syntax/src/keywords.rs](./crates/mimir-syntax/src/keywords.rs)
- `mimir-slang`: [crates/mimir-slang/src/client.rs](./crates/mimir-slang/src/client.rs), [crates/mimir-slang/src/protocol.rs](./crates/mimir-slang/src/protocol.rs)
- `mimir-server`: [crates/mimir-server/src/backend.rs](./crates/mimir-server/src/backend.rs), [crates/mimir-server/src/filelist.rs](./crates/mimir-server/src/filelist.rs), [crates/mimir-server/src/project.rs](./crates/mimir-server/src/project.rs)
- Integration / semantic-token tests: [crates/mimir-syntax/tests/semantic_tokens.rs](./crates/mimir-syntax/tests/semantic_tokens.rs)

## Pre-commit test protocol

Run **all three tiers** before every commit:

```bash
# Tier 1 — unit tests (always, ~1 s)
cargo test --workspace

# Tier 2 — integration tests (needs release binary + examples/riscv-dv clone)
cargo build --release -p mimir-server
make integration

# Tier 3 — stress tests (for any change touching parsing, indexing, or IPC)
MIMIR_STRESS_DURATION=30 python3 -m unittest tests.test_stress -v
```

If `examples/riscv-dv` is absent, Tier 2 tests that depend on it are skipped
automatically — this is expected on CI and fresh checkouts. The hermetic tests
(`test_hello_world.py`, `test_hierarchy.py`) always run regardless.

Tier 3 is optional for purely mechanical changes (doc edits, formatting
fixes) but required whenever the server's hot-path logic changes.

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

## Single responsibility

Every struct, module, and file owns exactly one concern. When a unit starts
doing two things, split it — don't add a second responsibility to an existing
one.

**Concrete rules:**

- **One job per struct.** A struct that holds both "how to talk to the sidecar"
  and "how to debounce elaboration" must be split. State fields are the tell:
  if you can't describe the struct's fields with a single noun phrase, it has
  too many jobs.
- **One job per module.** A module that tokenizes filelists *and* parses TOML
  schema *and* resolves project paths is three modules. Each module's `//!`
  doc must fit in one sentence — if you need "and", split.
- **Handlers are coordinators, not implementors.** LSP handler methods in
  `backend.rs` call services; they don't contain business logic. If a handler
  grows beyond ~10 lines of non-delegation code, the logic belongs in a
  service module.
- **Services own their state.** A service struct's fields are private to that
  service. Callers pass inputs and receive outputs; they don't reach into the
  service's internals or duplicate its state.
- **Existing splits to preserve:**

  | Unit | Single responsibility |
  |------|-----------------------|
  | `TreeSitterProvider` | `SyntaxParser` ownership + all parse operations |
  | `SlangService` | Sidecar IPC + param assembly |
  | `SlangAdapter` | Compile RPC round-trip + `MimirAst` cache + `CompileOutcome` |
  | `SyntaxService` | Document store + workspace index access |
  | `ElaborateService` | Debounce + dedup + diagnostic publish |
  | `ast_features` | Feature lookups (`definition`, `hover`, `completion`, …) on `MimirAst` |
  | `hierarchy_features` | `callHierarchy/*` + `typeHierarchy/*` (sync helpers, no locks) |
  | `diagnostics` | `MimirDiag` → LSP `Diagnostic` conversion (one place, all backends) |
  | `workspace_index` | Tree-sitter symbol index + identifier presence index |
  | `filelist` | `.f` tokenization + path resolution + `${VAR}` expansion |
  | `project` | `.mimir.toml` schema + `ResolvedProject` discovery/load |
  | `format` | Verible subprocess management for `formatting` / `rangeFormatting` |
  | `includes` | `` `include `` directive scanning for transitive header expansion |
  | `completion_score` | Fuzzy subsequence scorer (used by completion + workspace symbols) |
  | `mimir-slang` crate | Sidecar process lifecycle + NDJSON framing + protocol types |
  | `mimir-ast` crate | Backend-agnostic AST data types — no I/O, no parser, no LSP deps |
  | `backend` | LSP wire glue — thin coordinator only |

## When making changes

- New public function → add a `#[cfg(test)] mod tests` test in the same file.
- New fallible op → extend the crate's existing `*Error` enum (`thiserror`).
- Logging → `tracing::{debug, info, warn, error}` with structured fields, not
  formatted strings. Never `println!` / `eprintln!`.
- More debug logging to help identify issues, especially with sidecar communication.
- New LSP feature → flip its checklist item in [README.md](./README.md) in
  the same commit.
- Ask me questions to clarify the product requirements, technical requirements, engineering principles, and hard constraints.
- Update [README.md](./README.md) after all changes done.
- Update crates versions or sub-versions.
- **Run the full pre-commit test protocol** (see [Pre-commit test protocol](#pre-commit-test-protocol) above) before creating any commit. All three tiers must pass.
- Create a commit.

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
