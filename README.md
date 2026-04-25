# Mimir — SystemVerilog Language Server

> _Mimir, in Norse myth, is the keeper of knowledge and wisdom. Fitting for a
> tool that reads and understands your verification code._

`mimir` is an **incremental, asynchronous Language Server for SystemVerilog**,
written in Rust, focused first on **verification** (UVM, SVA, functional
coverage, constraints) rather than synthesis-only RTL editing.

It speaks the [Language Server Protocol (LSP)][lsp] over stdio, so it works
with any editor that supports LSP — VS Code, Emacs (eglot/lsp-mode), Neovim,
Helix, Sublime, Zed, etc.

[lsp]: https://microsoft.github.io/language-server-protocol/

---

## Status

Mimir is in **early development**. The skeleton is in place; most LSP features
are not implemented yet. See the [feature checklist](#feature-checklist) below
for the live state of every LSP request, kept in sync with the code.

---

## Architecture

A Cargo workspace with three crates, each independently testable:

| Crate           | Role                                                                      |
| --------------- | ------------------------------------------------------------------------- |
| `mimir-core`    | Pure-data types: `TextDocument` (rope-backed), positions, logging setup.  |
| `mimir-syntax`  | tree-sitter wrapper. Parses SystemVerilog, extracts diagnostics.          |
| `mimir-server`  | `tower-lsp` `Backend`. Owns the document store. Ships the binary.         |

**Why three crates?** Splitting boundaries lets us unit-test parsing without
spinning up a tokio runtime, and keeps tree-sitter's native code out of the
core types crate. It also forces clean module APIs — anything `mimir-server`
needs from `mimir-syntax` has to be `pub`.

```
┌──────────────┐   LSP/JSON-RPC over stdio    ┌────────────────────────────┐
│   editor     │ ───────────────────────────▶ │   mimir-server (binary)    │
│  (vscode/    │ ◀─────────────────────────── │   - tower-lsp Backend      │
│   emacs/...) │   diagnostics, hovers, …     │   - document store         │
└──────────────┘                              └────────────┬───────────────┘
                                                           │
                                          ┌────────────────┴───────────────┐
                                          ▼                                ▼
                              ┌──────────────────────┐       ┌────────────────────────┐
                              │     mimir-syntax     │       │       mimir-core       │
                              │  tree-sitter parser, │       │  TextDocument (rope),  │
                              │  parse diagnostics   │       │  positions, logging    │
                              └──────────────────────┘       └────────────────────────┘
```

---

## Building

```bash
cargo build --release          # produces target/release/mimir-server
cargo test  --workspace        # run all unit tests
cargo clippy --workspace -- -D warnings
```

The release binary is what you point your editor at.

---

## Logging / debugging

All logs go to **stderr** (LSP requires stdout for JSON-RPC traffic). Logging
uses the [`tracing`][tracing] ecosystem and is controlled by the standard
`RUST_LOG` env var:

```bash
RUST_LOG=mimir=debug mimir-server          # debug all mimir crates
RUST_LOG=mimir_syntax=trace mimir-server   # trace just the parser
RUST_LOG=warn mimir-server                 # quiet mode
```

When VS Code launches the server, set the env var in your settings:

```jsonc
{
  "mimir.server.env": { "RUST_LOG": "mimir=debug" }
}
```

[tracing]: https://docs.rs/tracing

---

## Editor integration

Editor configurations live under [`editors/`](./editors):

* [`editors/vscode/`](./editors/vscode) — TypeScript extension that launches `mimir-server`.
* [`editors/emacs/init.el`](./editors/emacs/init.el) — Emacs config snippet using eglot.

---

## Feature checklist

Legend: ✅ implemented · 🚧 in progress · ⬜ not yet · ❌ won't do

### Core LSP lifecycle

- ✅ `initialize` / `initialized` / `shutdown` / `exit`
- ✅ `textDocument/didOpen`
- ✅ `textDocument/didChange` (incremental, rope-backed)
- ✅ `textDocument/didClose`
- ⬜ `textDocument/didSave`
- ⬜ `workspace/didChangeConfiguration`
- ⬜ `workspace/didChangeWatchedFiles`

### Diagnostics

- ✅ Syntax / parse-error diagnostics from tree-sitter (`ERROR` & `MISSING` nodes)
- ⬜ Lint diagnostics (style, naming, dead code)
- ⬜ Semantic diagnostics (type mismatches, undeclared identifiers)
- ⬜ UVM-aware diagnostics (missing `super.build_phase`, factory misuse)
- ⬜ SVA diagnostics (malformed property/sequence)

### Editing assistance

- ⬜ `textDocument/semanticTokens` ("LSP syntax highlighting")
- ⬜ `textDocument/hover`
- ⬜ `textDocument/completion`
- ⬜ `textDocument/signatureHelp`
- ⬜ `textDocument/documentSymbol`
- ⬜ `workspace/symbol`
- ⬜ `textDocument/foldingRange`
- ⬜ `textDocument/documentHighlight`
- ⬜ `textDocument/inlayHint`
- ⬜ `textDocument/codeLens`

### Navigation

- ⬜ `textDocument/definition`
- ⬜ `textDocument/declaration`
- ⬜ `textDocument/typeDefinition`
- ⬜ `textDocument/implementation`
- ⬜ `textDocument/references`
- ⬜ `callHierarchy/*`
- ⬜ `typeHierarchy/*`

### Refactoring

- ⬜ `textDocument/rename`
- ⬜ `textDocument/codeAction` (quick-fixes)
- ⬜ `textDocument/formatting`
- ⬜ `textDocument/rangeFormatting`

### Verification-focused features (the actual product goals)

- ⬜ UVM class-tree navigation (component/object hierarchy)
- ⬜ UVM phase awareness (jump to overridden `build_phase`, `run_phase`, …)
- ⬜ UVM factory registration validation (`uvm_object_utils`, `uvm_component_utils`)
- ⬜ UVM sequence ↔ sequencer ↔ driver navigation
- ⬜ SVA property/sequence index, hover-preview of expansion
- ⬜ Functional coverage: covergroup/coverpoint/cross structure view
- ⬜ Constraint blocks: list `rand` variables, navigate constraint references
- ⬜ Test/testbench discovery & runner integration
- ⬜ Waveform-aware hover (signal width, last-driven location)

### Project / build integration

- ⬜ Filelist (`.f` / `-f`) parsing for compilation units
- ⬜ `+define+` / `+incdir+` macro & include path config
- ⬜ Multi-file elaboration & cross-file symbol resolution
- ⬜ Integration with simulator-specific build files (Verilator, Xcelium, VCS, Questa)

---

## Engineering principles

1. **Small, well-tested units.** Every crate has its own test suite. Public
   functions get a `#[cfg(test)]` block in the same file.
2. **No silent failures.** Errors are typed (`thiserror`), bubble up, and end
   up logged with `tracing` at an appropriate level.
3. **Incremental everything.** Documents are stored as ropes, parsing is
   incremental, and we never re-parse the world to answer a single request.
4. **Async by default.** Everything that touches I/O is `async`. CPU-heavy
   parsing runs on `tokio::task::spawn_blocking` so it doesn't stall the
   reactor.
5. **Verification first.** When choosing what to build next, the question is
   "does this help a verification engineer?" — not "does this look good in a
   feature comparison table."

---

## License

Dual-licensed under MIT or Apache-2.0, at your option.
