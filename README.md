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

## Requirements

### To run the server

| Requirement     | Version                                | Notes                                                                                                  |
| --------------- | -------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| **Rust**        | 1.75 or newer (stable)                 | Pinned via [`rust-toolchain.toml`](./rust-toolchain.toml); `rustup` will auto-install on first build.  |
| **C compiler**  | Any working `cc` (gcc, clang, MSVC)    | tree-sitter's grammar crate has a `build.rs` that compiles a C parser.                                 |
| **OS**          | Linux, macOS, Windows                  | Anywhere Rust + tree-sitter build. CI currently exercises Linux only.                                  |

### To use the VS Code extension

| Requirement | Version             | Notes                                                                  |
| ----------- | ------------------- | ---------------------------------------------------------------------- |
| **VS Code** | 1.85 or newer       | Older versions may work but aren't tested.                             |
| **Node.js** | 18 LTS or newer     | Only needed to *build* the extension; not at runtime.                  |
| **npm**     | Bundled with Node   | Used for `npm install` and `npm run compile` inside `editors/vscode/`. |

### To use Emacs

| Requirement | Version           | Notes                                                                                              |
| ----------- | ----------------- | -------------------------------------------------------------------------------------------------- |
| **Emacs**   | 29 or newer       | Recommended — built-in `eglot` and `verilog-ts-mode` (tree-sitter major mode) are available.       |
| **Emacs**   | 27 or 28          | Works, but you must install `eglot` (or `lsp-mode`) from MELPA manually.                           |

---

## Installation & setup

### 1. Get the source

```bash
git clone <your-fork-url> mimir
cd mimir
```

### 2. Build and install the server

The fastest way that puts the binary on your `$PATH`:

```bash
cargo install --path crates/mimir-server
```

That drops `mimir-server` into `~/.cargo/bin/` (which `rustup` adds to `$PATH`
on a default install). To verify:

```bash
which mimir-server
```

**Alternative** — if you don't want a global install, build only:

```bash
cargo build --release
# binary lives at ./target/release/mimir-server
```

In that case point your editor at the absolute path
(`<repo>/target/release/mimir-server`) instead of relying on `$PATH`.

### 3a. Configure VS Code

```bash
cd editors/vscode
npm install
npm run compile          # produces out/extension.js
```

For day-to-day **development of the extension itself**, open
`editors/vscode/` in VS Code and press `F5` — that launches an Extension
Development Host with the extension loaded.

For a **persistent install on your own VS Code**, package and install:

```bash
npx vsce package         # produces mimir-vscode-0.1.0.vsix
code --install-extension mimir-vscode-0.1.0.vsix
```

If `mimir-server` isn't on `$PATH`, set this in VS Code settings:

```jsonc
{
  "mimir.server.path": "/absolute/path/to/target/release/mimir-server"
}
```

See [`editors/vscode/README.md`](./editors/vscode/README.md) for more.

### 3b. Configure Emacs

Copy the relevant block from [`editors/emacs/init.el`](./editors/emacs/init.el)
into your own `init.el`. The minimum (eglot, Emacs 29+):

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '((verilog-mode verilog-ts-mode) . ("mimir-server"))))

(add-hook 'verilog-mode-hook    #'eglot-ensure)
(add-hook 'verilog-ts-mode-hook #'eglot-ensure)

(add-to-list 'auto-mode-alist '("\\.sv\\'"  . verilog-mode))
(add-to-list 'auto-mode-alist '("\\.svh\\'" . verilog-mode))
```

If `mimir-server` isn't on Emacs's `exec-path`, prepend the cargo bin dir:

```elisp
(add-to-list 'exec-path (expand-file-name "~/.cargo/bin"))
```

See [`editors/emacs/README.md`](./editors/emacs/README.md) for the lsp-mode
variant and logging tips.

### 4. Verify it works

Open any `.sv` file in the configured editor. Introduce a syntax error
(e.g. delete a `;`) and you should see a red squiggle within a few hundred
milliseconds. If nothing happens, see [Logging / debugging](#logging--debugging)
to inspect the server's stderr.

---

## Project configuration

For single-file syntax-error checking nothing needs configuring — drop a
`.sv` file anywhere and tree-sitter diagnostics flow.

For real UVM / RTL projects you'll want **slang elaboration**, and slang
needs to know your file set, include directories, and `+define+`s. That's
what `.mimir.toml` is for. The server walks up from the file you opened
(up to eight parent directories) looking for one.

### `.mimir.toml`

Minimal — point at a filelist and pick a top:

```toml
[slang]
filelist = "sim/uvm.f"
top      = "tb_top"
```

Full schema (every field is optional; the canonical types live in
[`crates/mimir-server/src/project.rs`](./crates/mimir-server/src/project.rs)):

```toml
[slang]
# Path to a .f filelist, relative to .mimir.toml.
filelist     = "sim/uvm.f"

# Extra include search paths, on top of anything the filelist contributes.
# Relative entries resolve against .mimir.toml's directory.
include_dirs = ["rtl", "verif/inc"]

# Extra +define+s. "NAME" defines to empty; "NAME=VALUE" carries a value.
defines      = ["UVM_NO_DPI", "BUS_WIDTH=32"]

# Top module / program. Omit to elaborate every top slang finds
# ("lint the whole project" mode).
top          = "tb_top"

# Quiet time (ms) before re-elaborating after the user stops typing.
debounce_ms  = 350
```

Inline `include_dirs` and `defines` are merged with whatever the filelist
pulls in — inline values come first, filelist values after. Unknown keys
are an error, not silently ignored, so a typo (`includ_dirs`) fails loudly
instead of disabling your config.

### Filelists (`.f`)

The verification industry's standard "what files belong together" format.
Every commercial simulator (VCS, Xcelium, Questa) and Verilator reads it,
so most projects already have one. Mimir parses the same dialect.

Whitespace-separated tokens. `\` followed by newline continues a line.
`//` and `#` start line comments. `${VAR}` interpolates from the process
environment — unknown variables expand to empty (matches `make` / most
simulators).

| Token                            | Meaning                                                 |
| -------------------------------- | ------------------------------------------------------- |
| `path/to/file.sv`                | Source file. Relative paths resolve against the `.f`.   |
| `+incdir+A` or `+incdir+A+B+...` | One or more include search paths.                       |
| `+define+NAME` / `+define+N=V`   | Predefine a macro (multiple `+`-separated allowed).     |
| `-f nested.f` or `-fnested.f`    | Recursively read another filelist.                      |

Recursion is bounded at 16 levels and cycles are detected by canonical
path, so a misconfigured `-f a.f` that points back at itself fails fast
instead of looping.

Example `sim/uvm.f`:

```
// UVM testbench, mirrors what `simv +UVM_TESTNAME=...` would compile
+incdir+${UVM_HOME}/src
+incdir+../verif/inc
+define+UVM_NO_DPI

${UVM_HOME}/src/uvm_pkg.sv

// DUT
../rtl/dut_pkg.sv
../rtl/dut.sv

// Testbench + tests
../verif/tb_top.sv
../verif/sequences.sv
-f ../verif/tests/all_tests.f
```

Slang elaboration is opt-in: set `MIMIR_SLANG_PATH` to a
`slang-sidecar` binary and the server uses your `.mimir.toml` automatically.
Without it, mimir falls back to tree-sitter-only diagnostics and the
`.mimir.toml` is simply ignored.

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

## Development

For hacking on Mimir itself (not just installing it):

```bash
cargo build  --workspace                    # debug build of all crates
cargo test   --workspace                    # run all unit tests (116 today)
cargo clippy --workspace -- -D warnings     # lint with warnings as errors
cargo fmt    --all                          # format
```

A typical inner-loop while adding a feature:

```bash
RUST_LOG=mimir=debug cargo run -p mimir-server   # run the server attached to your terminal's stdio
```

That isn't useful by itself (it'll just sit waiting for LSP messages on
stdin), but it's how you'd hand-feed JSON-RPC frames for debugging, or how
you'd attach an editor to a freshly-built binary without reinstalling.

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
- 🚧 Semantic diagnostics (type mismatches, undeclared identifiers) — via the slang sidecar; opt-in with `MIMIR_SLANG_PATH` + `.mimir.toml`
- ⬜ UVM-aware diagnostics (missing `super.build_phase`, factory misuse)
- ⬜ SVA diagnostics (malformed property/sequence)

### Editing assistance

- ⬜ `textDocument/semanticTokens` ("LSP syntax highlighting")
- ⬜ `textDocument/hover`
- ⬜ `textDocument/completion`
- ⬜ `textDocument/signatureHelp`
- ✅ `textDocument/documentSymbol` (flat, from the tree-sitter symbol index)
- ⬜ `workspace/symbol`
- ⬜ `textDocument/foldingRange`
- ⬜ `textDocument/documentHighlight`
- ⬜ `textDocument/inlayHint`
- ⬜ `textDocument/codeLens`

### Navigation

- ✅ `textDocument/definition` — tree-sitter index, same-file + workspace-wide (open docs and `.mimir.toml` filelist). Routes through slang's semantic resolver (scope-aware, hierarchical-name-aware) when `MIMIR_SLANG_PATH` is configured; falls back to the syntax index on transport error. Slang resolves variable / port / parameter / class-field references, hierarchical paths (`u_dut.fsm.state`), `obj.member`, subroutine calls (`f(x)`, `obj.method()`), type references in declarations (`my_t x;` → typedef/class), and module/interface instantiations (`apb_master u_dut(...)` → the module). Macro `` `define `` resolution still deferred.
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

- ✅ Filelist (`.f` / `-f`) parsing for compilation units (via `.mimir.toml`'s `slang.filelist`)
- ✅ `+define+` / `+incdir+` macro & include path config
- 🚧 Multi-file elaboration & cross-file symbol resolution (slang elaborates the whole compilation unit; cross-file *navigation* is not yet wired)
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
