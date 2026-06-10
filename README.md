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

Mimir is in **active development and usable today**. The general-purpose LSP
surface is broad and shipped:

- **Document sync** — incremental, rope-backed `didOpen` / `didChange` /
  `didClose` / `didSave`, plus project reload on `.mimir.toml` and source
  file-watcher events.
- **Diagnostics** — tree-sitter syntax/parse errors always on; full semantic
  diagnostics (type mismatches, undeclared identifiers, elaboration errors)
  when the slang sidecar is configured.
- **Editing assistance** — semantic tokens, hover (incl. keyword / system-task
  / built-in-method docs and typedef expansion), completion (member-access,
  package-scope, scope-aware, built-in methods), signature help, document &
  workspace symbols, inlay hints, folding ranges, document highlights.
- **Navigation** — go-to definition / declaration / type-definition /
  implementation, find-all-references, call hierarchy, type hierarchy.
- **Refactoring** — workspace-wide rename, whole-file & range formatting
  (via Verible).

**Two-tier architecture.** Everything above works in **tree-sitter-only mode**
with no external tools. Pointing mimir at the optional **slang sidecar**
(`MIMIR_SLANG_PATH` + a `.mimir.toml`) unlocks the semantic tier:
receiver-aware, type-accurate resolution for definition / hover / signature
help / inlay hints (e.g. methods inherited from UVM base classes), plus
`typeDefinition`, `implementation`, and elaboration-driven diagnostics.

**What's next.** The verification-specific frontier — UVM class-tree
navigation, phase awareness, factory validation, SVA expansion, functional
coverage structure, constraint navigation — is the active focus and is **not
yet implemented**. See "[Verification-focused features](#verification-focused-features-the-actual-product-goals)"
in the checklist.

The [feature checklist](#feature-checklist) below is the canonical, per-request
source of truth, kept in sync with the code on every change.

---

## Requirements

### To run the server

| Requirement     | Version                                | Notes                                                                                                  |
| --------------- | -------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| **Rust**        | 1.75 or newer (stable)                 | Pinned via [`rust-toolchain.toml`](./rust-toolchain.toml); `rustup` will auto-install on first build.  |
| **C compiler**  | Any working `cc` (gcc, clang, MSVC)    | tree-sitter's grammar crate has a `build.rs` that compiles a C parser.                                 |
| **OS**          | Linux, macOS, Windows                  | Anywhere Rust + tree-sitter build. CI currently exercises Linux only.                                  |

### To build the slang sidecar (optional — required for semantic features)

The sidecar is a separate C++ binary that wraps [slang][slang]. It powers
semantic diagnostics, type-aware go-to-definition, `typeDefinition`, and
`implementation`. Without it, mimir runs in tree-sitter-only mode (syntax
diagnostics + structural navigation).

| Requirement        | Version                          | Notes                                                                          |
| ------------------ | -------------------------------- | ------------------------------------------------------------------------------ |
| **CMake**          | 3.20 or newer                    | Drives the out-of-source build.                                                |
| **C++20 compiler** | gcc 11+ / clang 14+ / MSVC 2022+ | slang requires C++20.                                                          |
| **Ninja** or Make  | any recent                       | Build backend; Ninja recommended.                                              |
| **git**            | any                              | CMake's `FetchContent` pulls slang (~30 MB) and nlohmann_json on first configure. |

[slang]: https://github.com/MikePopoloski/slang

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

### 2b. Build the slang sidecar (optional)

Skip this step if you only want syntax diagnostics. Run it if you want
semantic diagnostics, type-aware go-to-definition, `typeDefinition`, or
`implementation`. The sidecar lives outside the cargo workspace on purpose
— contributors hacking on the Rust side don't need a C++ toolchain.

```bash
cd slang-sidecar
cmake -G Ninja -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
# binary lives at ./build/mimir-slang-sidecar
```

The first `cmake` invocation downloads slang (~30 MB) and nlohmann_json via
`FetchContent` and caches them under `build/_deps/`. Subsequent configures
reuse the cache. Drop `-G Ninja` to fall back to your platform's default
generator (Make on Linux/macOS).

Then point mimir at the binary using **one** of these options (in priority order):

**Option A — process environment** (recommended for CI / shared machines):

```bash
export MIMIR_SLANG_PATH="$PWD/build/mimir-slang-sidecar"   # absolute path
```

VS Code (per-workspace):

```jsonc
{
  "mimir.server.env": {
    "MIMIR_SLANG_PATH": "/absolute/path/to/slang-sidecar/build/mimir-slang-sidecar"
  }
}
```

Emacs (eglot picks up the parent process's environment):

```elisp
(setenv "MIMIR_SLANG_PATH"
        (expand-file-name "~/Dev/mimir/slang-sidecar/build/mimir-slang-sidecar"))
```

**Option B — `.mimir.toml` `[env]` table** (recommended for project-local setup):

```toml
[env]
# Absolute path — works everywhere:
MIMIR_SLANG_PATH = "/absolute/path/to/slang-sidecar/build/mimir-slang-sidecar"

# Relative path — resolved against the .mimir.toml directory:
MIMIR_SLANG_PATH = "../../slang-sidecar/build/mimir-slang-sidecar"
```

Relative paths in `MIMIR_SLANG_PATH` are resolved against the directory that
contains the `.mimir.toml`, so a project can ship a ready-to-use config that
"just works" when the mimir repo is cloned alongside it.

The [example workspace configs](#example-workspaces) already include this
setting pointing at the standard sidecar build output:

```toml
[env]
MIMIR_SLANG_PATH = "../../slang-sidecar/build/mimir-slang-sidecar"
```

The process environment always takes precedence over `.mimir.toml`'s `[env]`
section, so CI can override the project config by exporting `MIMIR_SLANG_PATH`.

Without a sidecar path mimir falls back to tree-sitter-only diagnostics.

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
# Workspace-local environment variables.  Checked before the process env
# when expanding ${VAR} in filelist tokens, inline paths, and when looking
# up MIMIR_SLANG_PATH.  Process env always overrides.
# Values may reference other [env] keys — full chain expansion, so
# multi-level hierarchies like the one below all resolve correctly:
[env]
PROJECT_ROOT     = "/work/my_project"
IP_ROOT          = "${PROJECT_ROOT}/ip"         # → /work/my_project/ip
RTL_DIR          = "${IP_ROOT}/rtl"             # → /work/my_project/ip/rtl
MIMIR_SLANG_PATH = "${PROJECT_ROOT}/bin/mimir-slang-sidecar"

[slang]
# Path to a .f filelist, relative to .mimir.toml.
filelist     = "${PROJECT_ROOT}/sim/uvm.f"

# Source files listed directly in the TOML — no separate .f required.
# Useful for per-workspace additions on top of a shared team filelist.
# Relative paths resolve against .mimir.toml; ${VAR} is expanded.
# Inline entries are prepended before filelist entries.
files        = ["tb/extra_tb.sv", "${PROJECT_ROOT}/stubs/axi_stub.sv"]

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

# Parse every is_compilation_unit:true file into a single shared
# compilation unit so `define macros leak across files in the order
# they were given (mirrors slang's --single-unit CLI flag). This is the
# right knob for UVM-style flows where uvm_macros.svh is included once
# and the macros are expected to be visible to every later file —
# without it, slang treats each file as its own preprocessor scope and
# you get cascading UnknownDirective errors. Default: false.
single_unit  = true

# Default timescale applied to design elements that don't declare their
# own (mirrors slang's --timescale CLI flag). Parsed by slang's
# TimeScale::fromString; invalid strings are logged and dropped.
timescale    = "1ns/1ps"

# Long-tail libslang flags parsed by the sidecar's slang::driver::Driver.
# Use for any option that doesn't have a dedicated TOML key above —
# --allow-use-before-declare, --ignore-unknown-modules, -Wno-..., etc.
# When a flag is also expressible as a typed field (single_unit,
# timescale) the typed field wins on conflict.
extra_args   = ["--allow-use-before-declare"]

[inlay_hints]
# Label format for method / function / task call inlay hints.
# "name"      — parameter name only:        a  (default, same as macros)
# "type"      — parameter type only:        int
# "name+type" — name and type:              a: int
# Macro hints always show the parameter name regardless of this setting.
method_hint  = "name"

[diagnostics]
# Quiet diagnostics for vendored code you can't (and won't) fix — UVM, IP,
# generated files — without losing them. Patterns are matched as plain
# SUBSTRINGS of each diagnostic's file path (not globs): "uvm-1.2" matches
# every file whose path contains "uvm-1.2".
#
# Files matching `demote_paths` keep their diagnostics, but capped at
# `demote_severity` — so a UVM elaboration error shows up as a faint hint
# instead of a red error drowning out your own code in the Problems panel.
demote_paths    = ["uvm-1.2"]
demote_severity = "hint"     # error | warning | information | hint (default: hint)

# Files matching `ignore_paths` have their diagnostics dropped entirely.
# Takes precedence over `demote_paths` when a path matches both.
ignore_paths    = []
```

The `[diagnostics]` policy applies to **slang elaboration diagnostics** (the
cross-file flood you get when a `` `include ``d UVM header elaborates with
warnings). It's evaluated at publish time against the slang-reported file
path, so it works on transitively-included files you never opened. An empty
or omitted `[diagnostics]` table is a no-op — every diagnostic is published
unchanged.

Inline `files`, `include_dirs`, and `defines` are merged with whatever the
filelist pulls in — inline values come first, filelist values after.
If a relative path doesn't exist under the TOML's directory, mimir retries
it as written (useful when a path becomes absolute after `${VAR}` expansion
or is intentionally CWD-relative). Unknown keys are an error, not silently
ignored, so a typo (`includ_dirs`) fails loudly instead of disabling your
config.

### Filelists (`.f`)

The verification industry's standard "what files belong together" format.
Every commercial simulator (VCS, Xcelium, Questa) and Verilator reads it,
so most projects already have one. Mimir parses the same dialect.

Whitespace-separated tokens. `\` followed by newline continues a line.
`//` and `#` start line comments. `${VAR}` interpolates from the `[env]`
table first, then the process environment — unknown variables expand to
empty (matches `make` / most simulators).

| Token                            | Meaning                                                 |
| -------------------------------- | ------------------------------------------------------- |
| `path/to/file.sv`                | Source file. Relative paths resolve against the `.f`.   |
| `+incdir+A` or `+incdir+A+B+...` | One or more include search paths.                       |
| `+define+NAME` / `+define+N=V`   | Predefine a macro (multiple `+`-separated allowed).     |
| `-f nested.f` or `-fnested.f`    | Recursively read another filelist (the glued one-token form only when it names a `.f`/`.F` file). |
| other `-flag` / `+plusarg`       | Simulator options mimir doesn't consume (`-full64`, `+libext+.v`, …) are skipped with a warning; `-y`/`-v`/`-top`/`-l`/`-sv_lib` skip their argument too. |

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

Slang elaboration is opt-in: build the sidecar and set `MIMIR_SLANG_PATH`
(see [Build the slang sidecar](#2b-build-the-slang-sidecar-optional)) and
the server uses your `.mimir.toml` automatically. Without it, mimir falls
back to tree-sitter-only diagnostics and the `.mimir.toml` is simply
ignored.

### Example workspaces

Two real-world RTL/DV projects are used as test subjects. Clone them into
`examples/` after cloning mimir, then drop in the matching `.mimir.toml`:

**chipsalliance/riscv-dv** (SV/UVM instruction generator, ~200 files):

```bash
git clone --depth=1 https://github.com/chipsalliance/riscv-dv examples/riscv-dv
```

`examples/riscv-dv/.mimir.toml`:

```toml
[env]
RISCV_DV_ROOT    = "."
MIMIR_SLANG_PATH = "../../slang-sidecar/build/mimir-slang-sidecar"

[slang]
filelist     = "files.f"
include_dirs = ["target/rv32imc"]
```

**lowRISC/ibex** (32-bit RISC-V CPU, ~159 files):

```bash
git clone --depth=1 --no-recurse-submodules https://github.com/lowRISC/ibex examples/ibex
```

`examples/ibex/.mimir.toml`:

```toml
[env]
MIMIR_SLANG_PATH = "../../slang-sidecar/build/mimir-slang-sidecar"

[slang]
filelist = "mimir.f"
```

`MIMIR_SLANG_PATH` in the `[env]` table is resolved relative to the
`.mimir.toml` directory, so `../../slang-sidecar/build/mimir-slang-sidecar`
points at the standard sidecar build output regardless of where you cloned
mimir. If the sidecar binary isn't built yet, mimir falls back to
tree-sitter-only mode silently — build it first with `make sidecar`.

---

## Architecture

A Cargo workspace with five crates, each independently testable:

| Crate           | Role                                                                                        |
| --------------- | ------------------------------------------------------------------------------------------- |
| `mimir-core`    | Pure-data types: `TextDocument` (rope-backed), positions, logging setup.                    |
| `mimir-syntax`  | tree-sitter wrapper. Parses SystemVerilog, extracts diagnostics and structural info.         |
| `mimir-slang`   | Async client for the C++ slang sidecar. Owns process lifecycle and the NDJSON wire protocol.|
| `mimir-ast`     | Backend-agnostic AST types (`MimirAst`, `MimirDecl`, `MimirDiag`, …). Pure data, no I/O.  |
| `mimir-server`  | `tower-lsp` `Backend`. Owns the document store. Ships the binary.                           |

**Why five crates?** Splitting boundaries lets us unit-test parsing without
spinning up a tokio runtime, keeps the sidecar IPC contract explicit, and
lets feature logic operate on `MimirAst` without knowing whether slang or
a future backend produced it.

Within `mimir-server`, heavy logic is split into focused service modules:

| Module               | Owns                                                                         |
| -------------------- | ---------------------------------------------------------------------------- |
| `parse_provider`     | `SyntaxParser` mutex, single-file parse, bulk path hydration                 |
| `slang_service`      | Sidecar IPC, project config, closed-file cache, param assembly               |
| `slang_adapter`      | Compile RPC round-trip, `MimirAst` cache, `CompileOutcome` production        |
| `syntax_service`     | Document store + workspace index access                                      |
| `elaborate_service`  | Debounce, input-hash dedup, diagnostic publish lifecycle                     |
| `ast_features`       | LSP feature lookups (definition, hover, completion, …) operating on `MimirAst` |
| `hierarchy_features` | `callHierarchy/*` and `typeHierarchy/*` helpers (sync, lock-free)            |
| `diagnostics`        | Backend-agnostic `MimirDiag` → LSP `Diagnostic` conversion                  |
| `workspace_index`    | Workspace-wide tree-sitter symbol index, identifier presence index           |
| `filelist`           | `.f` tokenization, path resolution, `${VAR}` expansion                       |
| `project`            | `.mimir.toml` schema, `ResolvedProject::discover` / `load`                   |
| `format`             | Verible formatter integration for `formatting` / `rangeFormatting`           |
| `includes`           | `` `include `` directive scanner for transitive header expansion              |
| `completion_score`   | Fuzzy subsequence-with-bonus scorer for completion / workspace-symbol ranking |

```
┌──────────────┐   LSP/JSON-RPC over stdio    ┌────────────────────────────┐
│   editor     │ ───────────────────────────▶ │   mimir-server (binary)    │
│  (vscode/    │ ◀─────────────────────────── │   - tower-lsp Backend      │
│   emacs/...) │   diagnostics, hovers, …     │   - document store         │
└──────────────┘                              └──────────┬─────────────────┘
                                                         │
                      ┌──────────────────────────────────┼──────────────────────────┐
                      ▼                                  ▼                          ▼
          ┌──────────────────────┐          ┌────────────────────┐    ┌─────────────────────┐
          │     mimir-syntax     │          │    mimir-slang     │    │     mimir-ast       │
          │  tree-sitter parser, │          │  sidecar process   │    │  MimirAst, MimirDecl│
          │  parse diagnostics   │          │  client + protocol │    │  MimirDiag  (data)  │
          └──────────┬───────────┘          └────────────────────┘    └─────────────────────┘
                     │
                     ▼
          ┌────────────────────────┐
          │       mimir-core       │
          │  TextDocument (rope),  │
          │  positions, logging    │
          └────────────────────────┘
```

---

## Development

For hacking on Mimir itself (not just installing it):

```bash
cargo build  --workspace                    # debug build of all crates
cargo test   --workspace                    # run all unit tests (436 today)
cargo clippy --workspace --all-targets -- -D warnings   # lint (incl. tests) with warnings as errors
cargo fmt    --all                          # format
make integration                            # python LSP integration tests (builds release binary first)
```

Unit tests live in-tree (`#[cfg(test)] mod tests` per file). End-to-end
LSP tests live under `tests/` and drive the release server over stdio
exactly like an editor — `make integration` builds the release binary and
runs `python3 -m unittest discover` on every `test_*.py` file. Tests cover:
- `test_hello_world.py` — server handshake, basic diagnostics
- `test_riscv_dv.py` — full feature suite against `examples/riscv-dv/` (hover,
  completion, definition, references, semantic tokens, inlay hints, document
  symbols, workspace symbols, folding ranges, signature help, document highlights)
- `test_apb_monitor.py` / `test_apb_cross_file.py` — cross-file symbol resolution
- `test_interfaces.py` — interface and modport features
- `test_elaborate_cache.py` — input-hash dedup logic
- `test_hierarchy.py` — `callHierarchy/*` and `typeHierarchy/*` (hermetic, no example repo)

A randomised long-running stress test (`test_stress.py`) simulates extended
editing sessions against three large riscv-dv files. Run it manually (not
part of `make integration`):

```bash
cargo build --release -p mimir-server
MIMIR_STRESS_DURATION=60 python3 -m unittest tests.test_stress -v
```

Two integration tests are known incomplete (SV keyword completion, cross-file
class member completion) and are left failing to track the gap.

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

### Slang backend errors

Every failure from the Slang sidecar is tagged `[SlangError]` in the log so
it is easy to grep:

| Situation | Log level | Tag |
|-----------|-----------|-----|
| Sidecar binary not found / cannot spawn | `WARN` | `[SlangError]` |
| Compile RPC fails (crash, I/O, timeout) | `ERROR` | `[SlangError]` |
| Slang reports an error-severity diagnostic | `ERROR` | `[SlangError] compile diagnostic` |
| Slang reports a warning-severity diagnostic | `WARN` | `[SlangError] compile warning` |

Each diagnostic entry carries structured fields: `file`, `line`, `code`, and
`message`.  Filter them directly:

```bash
RUST_LOG=mimir=warn mimir-server 2>&1 | grep '\[SlangError\]'
```

### Crash diagnostics

Panics are routed through `tracing` before the process exits, so the full
panic message and backtrace appear in the editor's LSP output channel (not
silently on the OS console). Enable backtraces:

```jsonc
{
  "mimir.server.env": {
    "RUST_LOG": "mimir=debug",
    "RUST_BACKTRACE": "full"
  }
}
```

Every `#[instrument]`-decorated handler emits an "enter" breadcrumb at
`debug` level, so the last line of the log before a crash identifies the
exact handler that triggered it.

### Per-scope wall-clock timing (finding bottlenecks)

Set `MIMIR_DEBUG_TIMING=1` in the server environment to enable RAII scope
timers across every LSP handler and every stage of the slang elaborate
pipeline. Each scope logs one structured line on exit:

```
INFO debug_timer scope="elaborate.task_total" ms=18412
INFO debug_timer scope="elaborate.debounce_sleep" ms=350
INFO debug_timer scope="elaborate.build_params" ms=42
INFO debug_timer scope="slang.compile.adapter_total" ms=18019
INFO debug_timer scope="slang.compile.connection_total" ms=17986
INFO debug_timer scope="slang.ipc.serialize" ms=12
INFO debug_timer scope="slang.ipc.write_to_sidecar" ms=8
INFO debug_timer scope="slang.ipc.read_and_match" ms=17841
INFO debug_timer scope="slang.ipc.decode_result" ms=124
INFO debug_timer scope="slang.compile.adapter.cache_ast_write" ms=2
INFO debug_timer scope="elaborate.publish_diagnostics" ms=15
```

Filter the noise to see only the timing breakdown:

```bash
RUST_LOG=mimir=info MIMIR_DEBUG_TIMING=1 mimir-server 2>&1 | grep debug_timer
```

Or in VS Code:

```jsonc
{
  "mimir.server.env": {
    "RUST_LOG": "mimir=info",
    "MIMIR_DEBUG_TIMING": "1"
  }
}
```

When unset (default), every timer is one cached-atomic-bool check and an
`Instant::now()` — no formatting, no allocation, no log line.

**Label namespaces:**

| Prefix | Meaning |
|--------|---------|
| `lsp.*` | LSP handler entry (`lsp.hover`, `lsp.did_change`, `lsp.completion`, …) |
| `elaborate.*` | Background slang elaborate task — debounce, param assembly (`elaborate.assemble.*`), hashing, publishing |
| `slang.compile.*` | Compile RPC layers — service, adapter, client, connection |
| `slang.ipc.*` | NDJSON wire phases — serialize, write, read, decode |
| `syntax.*` | Tree-sitter parse + index update |

For the C++ sidecar, the **`MIMIR_SLANG_TIMING=1`** env var emits a single
per-compile breakdown line summarising the work inside `handle_compile`:

```
[mimir-slang-sidecar] timing build=183ms diags=85ms visit=611ms \
    refs_raw=71910 refs_remap=274ms decls=50ms emit=1ms total=...
```

The sidecar's own stderr is drained through the server's `tracing`
subscriber under the `mimir_slang_sidecar` target, so these lines show up
in the editor's LSP log alongside everything else (no separate channel to
watch). Use both env vars together for end-to-end attribution from LSP
handler entry through the sidecar's elaborate stages.

**Sidecar performance notes.** The `visit` stage builds the
receiver-aware reference map by walking the elaborated AST. Two
optimisations keep it fast: (1) out-of-scope subtrees (UVM, vendor IP,
anything not in the editor's filelist) are pruned at the
module/package/class body level rather than filtered per-expression, and
(2) repeated instances of the same module resolve to a single canonical
body that's walked once. An experimental parallel visit
(`MIMIR_SLANG_PARALLEL=1`, thread-capped via
`MIMIR_SLANG_PARALLEL_THREADS=N`) exists but is **off by default and
known to be unsafe** — slang's lazy evaluators race under concurrent
traversal. Set `MIMIR_SLANG_EMIT_REFS=0` to disable the reference map
entirely on pathologically large compilations (the consumer falls back
to its legacy resolution paths).

[tracing]: https://docs.rs/tracing

---

## Feature checklist

Legend: ✅ implemented · 🚧 in progress · ⬜ not yet · ❌ won't do

### Core LSP lifecycle

- ✅ `initialize` / `initialized` / `shutdown` / `exit`
- ✅ `textDocument/didOpen`
- ✅ `textDocument/didChange` (incremental, rope-backed)
- ✅ `textDocument/didClose`
- ✅ `textDocument/didSave` — registered via the `save` option on `text_document_sync` (so the editor sends save notifications). Handler logs the save and schedules a debounced slang elaborate so the sidecar's view of the compilation unit reflects what's now on disk. v1 deferrals / limitations: no save-triggered re-parse (incremental sync already kept state fresh — `did_change` is authoritative); no save-time diagnostics flush; `save.includeText: false` (the buffer is already in our rope, no need for the editor to re-send it); no `willSave` / `willSaveWaitUntil`. No-op when slang isn't configured.
- ✅ `workspace/didChangeConfiguration` — on receipt, re-discovers the project from the stored workspace root (the same root captured during `initialize`) and re-hydrates the workspace symbol index from the resulting filelist. mimir's runtime configuration lives in `.mimir.toml`, not in editor settings, so this notification acts as a "reload project" trigger. Slang project config (filelist, include dirs, `[env]` table) is also updated via `SlangService::set_project`. Fire-and-forget: hydration runs on a `tokio::spawn`d task; the notification handler returns promptly. When no workspace root was recorded (rootless clients), the notification is a no-op.
- ✅ `workspace/didChangeWatchedFiles` — dynamically registered in `initialized` via `client/registerCapability` with two watcher globs: `**/.mimir.toml` and `**/*.{sv,svh,v}`. Routes by event kind: `.mimir.toml` Created/Changed → re-discover the project from its workspace root and re-hydrate the workspace symbol index from the new filelist (fire-and-forget on `tokio::spawn`, mirroring the initialize-time hydration); SV-source Created/Changed → re-parse the file and replace its entry in the workspace symbol index *only if the file isn't currently open in the editor* (open buffers always win — they're authoritative for unsaved content, matching the existing `WorkspaceIndex` ownership contract); any Deleted event → evict the URL from the workspace symbol index. v1 deferrals / limitations: requires a client that advertises `workspace.didChangeWatchedFiles.dynamicRegistration` (older clients silently get no watcher and continue to see the documented external-edit gap; registration failure is logged at warn but doesn't fail the session). Watcher globs are fixed — out-of-workspace files (slang `+incdir+` paths outside the editor's root, vendor sources under `~/uvm/`, …) won't be watched. No re-hydrate re-entrancy guard yet — two near-simultaneous `.mimir.toml` events fire two `tokio::spawn`s that both call `hydrate_workspace_index`; both eventually overwrite each entry, last write wins. No client-side progress reporting during re-hydration — the editor may briefly show stale workspace-symbol results while a hydrate is in flight.

### Diagnostics

- ✅ Syntax / parse-error diagnostics from tree-sitter (`ERROR` & `MISSING` nodes)
- ⬜ Lint diagnostics (style, naming, dead code)
- ✅ Semantic diagnostics (type mismatches, undeclared identifiers) — via the slang sidecar `compile` RPC which exports a full `MimirAst`; opt-in with `MIMIR_SLANG_PATH` + `.mimir.toml`
- 🚧 UVM-aware diagnostics — **missing `super.<phase>()` shipped**; factory misuse pending. A tree-sitter check (works without slang) flags any UVM phase-method override (`build_phase`, `connect_phase`, `run_phase`, … — identified by a `uvm_phase` parameter, so unrelated same-named helpers are ignored) whose body never chains to `super.<phase>(...)`. Covers in-class and out-of-class (`function void cls::build_phase`) bodies; `extern`/pure prototypes are skipped. Configure under `[diagnostics]` in `.mimir.toml`: `uvm_phase_super_call` (on/off, default true), `uvm_phase_super_severity` (`error`/`warning`/`information`/`hint`, default `warning`), `uvm_phases` (override the checked phase list; default = the UVM common phases). Lives in [`mimir_syntax::uvm`](./crates/mimir-syntax/src/uvm.rs).
- ⬜ SVA diagnostics (malformed property/sequence)

### Editing assistance

- ✅ Macro expansion ("Expand Macro recursively") — the verification analog of rust-analyzer's expand-macro. Exposed two ways: (1) a **`Mimir: Expand Macro`** command (editor context menu / command palette) that opens the fully-recursive expansion of the macro under the cursor in a read-only SystemVerilog tab beside the editor — ideal for the dozens-of-lines `` `uvm_component_utils_begin `` / `` `uvm_field_int `` expansions UVM is built on; (2) a **hover footer** on `` `macro `` usages showing the expansion's line count plus a short fenced preview and the command CTA. Requires the slang sidecar (`MIMIR_SLANG_PATH` + `.mimir.toml`) — the expansion is computed by slang's preprocessor in the sidecar (`expandMacro` RPC), so nested macros, token-paste, stringification, and argument substitution are all handled correctly (no textual re-implementation). The sidecar runs the preprocessor over the same buffer sequence a compile uses (respecting `single_unit`), so macros defined in an earlier-included header like `uvm_macros.svh` expand exactly as they elaborate. **Include-member files are supported**: when the file under the cursor isn't itself a filelist/compilation-unit member but is reached via `` `include `` (the common UVM layout — a component included by its package), the sidecar feeds the whole compilation unit so the including context defines the macros, then attributes the expansion to the target by canonical file path (not buffer id, since the `` `include `` lands in a fresh buffer). Without this, expanding `` `uvm_component_utils `` in such a file returned nothing, because preprocessed standalone the macro is undefined. Transport is a custom `mimir/expandMacro` LSP request (mirrors rust-analyzer's `rust-analyzer/expandMacro`); the VS Code client opens the result via a `mimir-expand:` `TextDocumentContentProvider`. A small per-document cache keyed by `(uri, version, usage-range)` lets the hover footer and the panel command share one preprocessor run. **Expansion runs on a dedicated sidecar process/connection, separate from elaborate.** The wire protocol is single-flight, so one connection serialises all its callers behind the in-flight request; a real-project elaborate can hold its connection for many seconds (or hang outright). The slang `Client` therefore spawns *two* sidecars — one for `compile`/elaborate, one used only for `expandMacro` — so on-demand expansion never queues behind (or stalls on) a long/stuck compile. The expand sidecar only ever runs the preprocessor (it never builds the elaborated design), so its steady-state memory stays small. Before this, both the command and the hover footer shared the compile connection: a long or stuck elaborate made the command appear to "do nothing" (it blocked on the lock forever) and the hover footer silently vanish. The command still blocks on its (uncontended) expand connection so the user gets the result, with a VS Code progress notification while it computes; the **hover footer stays non-blocking** (`try_lock` on the expand connection) so a hover is never delayed even by a concurrent expansion. **Stale fallback**: the per-document expansion cache is *not* wiped on every edit — it retains the last-good expansion per usage range (capped per document). When a hover lands while the sidecar is busy/unresponsive and there's no fresh hit, the footer falls back to the cached expansion, clearly marked *“(cached — may be stale while slang re-elaborates)”*, so it stays visible through active editing instead of flickering away on every keystroke; the next idle hover replaces it with a fresh expansion. v1 limitations: returns null (friendly "not on a macro usage" message) when the cursor isn't on a `` `macro ``; the hover footer is gated by a cheap "backtick before identifier" check so ordinary hovers never trigger a preprocessor round-trip; for an include-member file the expansion reflects the file's on-disk content (the including TU reads it from disk), so unsaved edits to the macro arguments may lag until save.

- ✅ `textDocument/semanticTokens` ("LSP syntax highlighting") — pure tree-sitter classifier walking the cached parse tree. Supports `semanticTokens/full` and `semanticTokens/range` (the latter prunes whole subtrees outside the editor's viewport, so cold-open on a huge file scales with what's visible). Fixed legend advertised in `initialize`, ordinals pinned at the [`mimir_syntax::semantic_tokens::TokenType`](./crates/mimir-syntax/src/semantic_tokens.rs) enum: `keyword`, `type`, `class`, `interface`, `namespace`, `function`, `macro`, `parameter`, `variable`, `comment`, `string`, `number`, `regexp`. Modifiers: `declaration`, `readonly`. Classification rules (first match wins): comments / numbers / `$system_tf_identifier` emit one whole-node token and stop descent (LSP forbids overlapping tokens); string literals are split into alternating `string` and `regexp` sub-tokens, where `regexp` covers each `%`-format specifier (`%d`, `%0h`, `%8.0f`, …) so themes can colour them differently from the surrounding string body — disable with `[features] format_specs_in_strings = false` in `.mimir.toml` to revert to a single whole-string token per literal; `simple_identifier` is classified by its parent node kind — declaration names on `class_declaration` / `module_*_header` / `interface_declaration` / `package_declaration` / `program_declaration` / `function_body_declaration` / `task_body_declaration` / `param_assignment` get the matching specific type plus the `declaration` modifier; identifiers under `class_type` are `class` references; identifiers directly under `data_type` are `type` (user-defined typedef / enum / struct references — built-in types like `int` / `logic` appear as anonymous keyword nodes and are handled separately; package-scoped `pkg::MyType` colours `pkg` as `namespace`); the last `simple_identifier` in a `hierarchical_identifier` child of `tf_call` is `function` (covers both `foo()` bare calls and `obj.method()` method calls — the receiver chain stays `variable`); identifiers under `method_call_body` are `function` (`super.method()` / `this.method()` via `implicit_class_handle`); identifiers under `text_macro_usage` / `text_macro_definition` are `macro`; anonymous keyword leaves whose `kind()` matches the existing `KEYWORDS` table become `type` if their parent is a `data_type` / `integer_atom_type` / `integer_vector_type` / `net_type` container, otherwise `keyword`. Feature toggles (all default `true`): `[features] semantic_tokens = false` disables the entire feature so the client falls back to its TextMate grammar; `format_specs_in_strings = false` reverts to whole-string tokens; `keyword_hover = false` suppresses the keyword/system-task hover fallback. v1 deferrals / limitations: **syntactic classifier only** — no workspace symbol index, no slang. A reference like `my_class x;` colours `my_class` as `type` when it appears in a `data_type` position (covering UVM typedef aliases like `uvm_status_e`, `uvm_reg_data_t`) or `class` when it appears in a `class_type` node (e.g. `extends MyClass`); whether an identifier refers to a class vs. a typedef still requires slang for full accuracy. **Legend fixed at server-init** — no per-client legend negotiation; re-ordering the enum is a breaking wire-format change. **No `semanticTokens/full/delta`** — every refresh sends the full token stream. Acceptable for files under ~10k lines; revisit when benchmarks say otherwise. **No `operator` slot** — operators are anonymous tree-sitter tokens and classifying them is cheap-but-noisy; deferred. **No semantic-token modifiers beyond `declaration` and `readonly`** — no `static`, `abstract`, `deprecated`, `defaultLibrary`. **`variable` is the default catch-all** for `simple_identifier`s with no specific classification — that means many identifier references colour as `variable` even when they refer to functions / classes / etc.
- ✅ `textDocument/hover` — slang-first with tree-sitter fallback, and a keyword / system-task help fallback after both. When slang is configured the server looks up the cursor symbol in the cached `MimirAst` (produced by the sidecar's `compile` RPC) and routes through the same rich formatter the tree-sitter path uses. On miss or when the sidecar is unconfigured, falls through to the tree-sitter path (hover is a UX feature, not a correctness one — better to show *something* than nothing). Tree-sitter side: cursor on a class / module / interface / package / program / typedef / variable / port / parameter / field returns its declaration line as a `systemverilog` fenced markdown block; cursor on a function / task / method returns a synthesized signature rendered as rich markdown — declaration keywords bold (`**function**`, `**task**`, `**input**`), primitive types italic (`*int*`, `*logic*`, `*string*`), and identifiers as inline code (`` `build_phase` ``); cursor on a `` `MACRO `` reference returns the `` `define `` signature plus the trimmed multi-line body sliced from `Symbol::full_range`. Rich markdown is generated by `mimir_syntax::hover_format::format_sv_signature` (a word-by-word classifier — no extra parse, no code fence). Receiver-aware via the same chain inlay-hints use: `this.X` and `super.X` walk the enclosing `class_declaration` (and its `extends` chain) through `find_method_in_class` / `find_field_in_class`; `obj.X` resolves `obj`'s declared type via `find_variable_type_at`, normalizes type qualifiers, and looks the member up on that class. Keyword / system-task fallback (runs only after both slang and the workspace index miss, so it never shadows user-defined symbols): cursor on a documented SV keyword (`always_ff`, `covergroup`, `constraint`, …) or a `$system_task` (`$display`, `$cast`, `$urandom`, …) returns a popup with the keyword/name in a fenced block plus a one-line description and an IEEE 1800-2017 LRM `§` reference, sourced from the curated `KEYWORD_DOCS` / `SYSTEM_TASK_DOCS` tables in `crates/mimir-syntax/src/keywords.rs`. v1 deferrals on the keyword fallback: **curated coverage only** — ~110 keywords (the ones with non-obvious semantics; structural noise like `endmodule`, `endclass`, `endcase` is intentionally omitted) plus ~75 common verification system tasks, so reserved words / `$tasks` not in the tables still return no popup; **one-liner format** — short summary + LRM `§` reference, no examples, no parameter docs, no full LRM text; **IEEE 1800-2017 fixed** — no per-LRM-version awareness; vendor extensions (`$psprintf`, etc.) are only present if explicitly listed; UVM macros (`` `uvm_info ``, …) are not in this table and resolve through the symbol index as `\`define`s; **no context sensitivity** — `assert` returns the same docs whether used procedurally or as a concurrent assertion. Multi-hop chained member access (`a.b.c`, `this.ap.write`, `obj.get().field`) is supported on the tree-sitter path up to 2 intermediate hops via `chain_resolve`; deeper chains fall through to slang (when configured) or bare-identifier lookup. Other v1 deferrals: no hierarchical names (`u_dut.fsm.state`). Typedef expansion is now supported — see the `✅ Typedef expansion in hover` entry below.
- ✅ `textDocument/completion` — full pipeline: syntax candidates (same-file symbols, workspace-wide symbols, SV keywords) always on; MimirAst-backed routes when `MIMIR_SLANG_PATH` is configured and a successful `compile` RPC has produced a cached `MimirAst`: `obj.` member-access and `pkg::` package-scope completion (type-aware via the elaborated symbol table), scope-aware identifier completion (inner scopes shadow outer). Syntax fallback for all paths when slang is unavailable. For `.` triggers when the MimirAst is unavailable, falls back to AST-based member completion: `super.` enumerates the parent class's members via `extends` chain walk; `this.` enumerates the enclosing class's own members and inherited ones; `<ident>.` resolves the identifier's declared type via `find_variable_type_at` then enumerates its class members; multi-hop chains like `a.b.` walk each segment via the workspace index and enumerate the type at the end of the chain (up to 2 intermediate hops on the tree-sitter path) — unknown receivers (undeclared variables, deeper chains) still return empty to avoid workspace-dump noise. **Built-in method completion**: appended after any workspace-defined members (workspace wins on name collision). Three layers: (1) type-aware — `"string"` receiver appends all IEEE 1800-2017 §6.16.13 string methods; queue / dynamic-array / associative-array receivers (detected by dimension suffix `[$]` / `[]` / `[K]`) append the matching container method table; (2) dimension-suffix–aware builtin routing via `methods_for_suffix`; (3) universal — `rand_mode`, `constraint_mode`, `randomize` appended unconditionally for any class-typed receiver. Items are fuzzy-ranked (subsequence matching with prefix bonus) and top-200 are selected via `select_nth_unstable_by` (O(n) partial sort) before the final O(k log k) sort of only the returned items — avoids O(n log n) full sort on large workspaces; core SV constructs (`module`, `class`, `always_ff`, …) expand as snippets. Trigger characters: `.`, `` ` ``, `$`, `:` (the second colon of `pkg::`). The workspace symbol index follows `` `include`` directives, so listing `uvm.sv` in `.mimir.toml` is enough for tree-sitter completion to surface symbols defined in `uvm_pkg.sv`. All responses are returned as a `CompletionList` with `isIncomplete: true` so the editor re-queries the server on every edit — including deletion — rather than caching a "complete" list and filtering locally. Without this, editing back into a member-access prefix (`obj.a_some` → backspace → `obj.a_som`) would not re-pop the suggestion list; the server recomputes candidates from the buffer on each call, so the cursor always gets fresh, position-correct results.
- ✅ `completionItem/resolve` — lazily attaches the declaration line as a markdown documentation block when the user highlights a completion item. Reads from the open-doc store first; falls back to a disk read so cross-file items resolve even when the declaring file isn't open.
- ✅ `textDocument/signatureHelp` — tree-sitter based. Finds the enclosing call via `tf_call` / `system_tf_call` / `text_macro_usage` nodes, looks up the callee in the same-file and workspace symbol indices, and emits one `SignatureInformation` with parameter offsets for active-parameter highlighting. When neither index contains the callee, falls back to the built-in method table (catches `push_back(item)`, `substr(i, j)`, `rand_mode(on_off)`, etc.). Trigger characters: `(` and `,`. Method calls (`.method(...)`) are skipped for the workspace-index path — receiver type resolution for slang-backed method lookup is a future slice.
- ✅ `textDocument/documentSymbol` (flat, from the tree-sitter symbol index). Forward type declarations (`typedef class Foo;`, `typedef interface class Foo;`, bare `typedef Foo;`) are **not** indexed — they carry no definition and would otherwise shadow the real `class Foo … endclass` in name lookups (hover/goto/references would surface the forward decl instead of the implementation).
- ✅ `workspace/symbol` — fuzzy workspace-wide symbol picker (VS Code's `Ctrl+T`, Emacs `xref-find-apropos`). Reads the same workspace symbol index already populated for `definition` and `completion` (open docs + `.mimir.toml` filelist, following `` `include `` chains), fuzzy-ranks every candidate against the user's query via the completion scorer, and returns up to 200 `SymbolInformation` results ordered by score descending. Empty query returns every visible-kind entry up to the cap, matching IDE picker conventions. v1 limitations: source is tree-sitter only — no slang-backed semantic symbols (cross-package resolved generics, elaborated names) yet; the kinds `Variable`, `Port`, `Parameter`, and `EnumMember` are excluded from results so the picker stays usable on real UVM testbenches; `container_name` is populated only for class methods (free functions / modules carry `None`); no `workspaceSymbol/resolve` (lazy range filling — pointless until we migrate the response type to LSP 3.17's `WorkspaceSymbol`).
- ⬜ `workspaceSymbol/resolve` — lazy enrichment for workspace symbols; gated on a tower-lsp upgrade that exposes the LSP 3.17 `WorkspaceSymbol` response type.
- ✅ `textDocument/foldingRange` — pure tree-sitter walk. Emits one foldable line range per top-level construct (modules, classes, functions, tasks, packages, interfaces, programs, properties, sequences, covergroups) and per `begin...end` block (`seq_block`) inside `if`/`else`/`for`/`while`/`fork` statements. Nested folds are emitted (a class's methods fold inside the class's own fold). Single-line constructs are skipped. `kind: Region` in the LSP response. Compiler directives and standalone UVM macro calls (`` `ifdef ``, `` `uvm_fatal ``, etc.) are preprocessed before parsing so include-guard wrappers and UVM-heavy files produce correct structural folds. Comment folding is deferred.
- ✅ `textDocument/documentHighlight` — scope-aware intra-file highlighter built on tree-sitter. Uses `identifier_at` to grab the name under the cursor, climbs the parse tree to the narrowest enclosing scope (function/task/class/module/interface/program/package/begin-block/initial/always/generate) that *locally* declares that name, and collects only the `simple_identifier` / `system_tf_identifier` matches inside that scope. Nested scopes that re-declare the same name are pruned, so a `phase` parameter in `build_phase` no longer lights up the unrelated `phase` parameter in `connect_phase`, and a shadowed inner `int x;` doesn't pollute outer-scope highlights. Free-standing references whose declaration isn't visible (e.g. `super.x`) fall back to whole-file matching. Full-token equality (no prefix matches). Cursor on whitespace / keyword / non-identifier returns nothing. Cross-file scope resolution is future work atop slang.
- ✅ `textDocument/inlayHint` — slang-first with a tree-sitter fallback. Finds all call sites in the editor's visible viewport and places ghost-text labels before each argument. **Method-call resolution is slang-first**: when a cached `MimirAst` is available, the method-name token is resolved through the reference map (`MimirFile::references` via `ast_features::method_params_at`), and the callee's formal parameters are read from the resolved declaration. Because slang's name binder is authoritative, this resolves methods **inherited from base classes the tree-sitter workspace index never indexed** (e.g. UVM bases — the `m_obj.configure(...)` case), which the tree-sitter inheritance walk could not reach. On a miss (no cached AST, or a call shape the visitor doesn't cover) it falls back to the tree-sitter resolver below. **Label format is configurable** via `[inlay_hints] method_hint` in `.mimir.toml`: `"name"` (default) shows only the parameter name (`a`); `"type"` shows only the type (`int`, falls back to name when unknown); `"name+type"` shows both (`a: int`). Macro callsites always show bare param names (`ID`, `MSG`) regardless of the setting — macro args carry no SV type. Whole-line macro callsites are preserved past the preprocessor under the 0.3.1 grammar's compiler-directive allowlist, so AST-backed features see them as `text_macro_usage` nodes. The tree-sitter fallback covers four shapes via the AST: `this.X(...)` and `super.X(...)` use the enclosing `class_declaration` (and `extends` chain walked through `Symbol::parent_class_name` — `extern virtual` prototypes like UVM's `run_phase`/`build_phase` are indexed too); `obj.method(...)` uses `find_variable_type_at` to read `obj`'s declared type from any enclosing scope (class field, function arg, local) and normalizes type qualifiers (`virtual`, `pkg::`, `#(...)`, `[…]`, `.modport`) before looking up the method on the resolved class; `ap = new("ap", this)` (and `T x = new();`) reads the LHS's declared type and looks up the constructor. Chained access (`obj.field.method(...)`) is resolved on the tree-sitter path via the chain resolver (up to 2 intermediate hops); deeper chains and bare unattached `new(...)` fall back to slang (when configured) or skip with an explicit trace. Calls with more arguments than declared parameters are silently skipped (avoids wrong labels for variadic-style patterns). Named-argument syntax (`.name(value)`) is detected and suppresses the inline label for that argument — the user has already written the name at the call site, so an inlay would be redundant; before v0.7.17 the value would have been incorrectly prefixed with whichever positional param happened to share its slot.
- ✅ Keyword / system-task hover help — covered in the `textDocument/hover` entry above. Curated `KEYWORD_DOCS` + `SYSTEM_TASK_DOCS` tables in [`crates/mimir-syntax/src/keywords.rs`](./crates/mimir-syntax/src/keywords.rs).
- ✅ Built-in SV method hover — hover on LRM-defined methods that never appear in the workspace index (`push_back`, `pop_front`, `rand_mode`, `constraint_mode`, `randomize`, `len`, `toupper`, `tolower`, `substr`, `atoi`, `itoa`, `exists`, `first`, `last`, `sort`, `shuffle`, …) returns the IEEE 1800-2017 signature and a one-line description. Type-aware for `string` receivers (uses `find_variable_type_at`); dimension-suffix–aware for queues / dynamic arrays / associative arrays (uses `find_variable_type_info_at` which returns a `TypeInfo { base, suffix }` so `push_back` / `pop_front` appear for `int q[$]` and `exists` / `delete` appear for `int aa[string]`); universal-table–aware for any class receiver (`rand_mode`, `constraint_mode`, `randomize`). Curated tables (`STRING_METHODS`, `QUEUE_METHODS`, `DYNAMIC_ARRAY_METHODS`, `ASSOC_ARRAY_METHODS`, `UNIVERSAL_METHODS`) live in [`crates/mimir-syntax/src/builtin_methods.rs`](./crates/mimir-syntax/src/builtin_methods.rs).
- ✅ Typedef expansion in hover — when the cursor is on a `typedef` name (e.g. `addr_t`), the hover popup shows the full declaration line plus an **Expands to:** note with the underlying type extracted from that line (e.g. `logic [31:0]`). Implemented via `typedef_base_from_line` in `backend.rs` which parses the `typedef <base> <alias>;` pattern without a second tree-sitter traversal.
- ✅ Multi-hop chained member access in hover / go-to-definition / completion (`a.b.c`, `this.ap.write`, `obj.get().field`) — tree-sitter path supports up to 2 intermediate hops via `chain_resolve`; chains beyond that fall through to slang or bare-identifier lookup.
- ✅ Built-in method completion for queues / dynamic arrays / associative arrays — `find_variable_type_info_at` now returns a `TypeInfo { base: String, suffix: Option<String> }` carrying the dimension suffix (`[$]` for queues, `[]` for dynamic arrays, `[K]` for associative arrays). `syntax_member_completion` reads the suffix and appends the matching builtin table (`QUEUE_METHODS`, `DYNAMIC_ARRAY_METHODS`, `ASSOC_ARRAY_METHODS`) after any workspace-member items. The suffix is preserved through the chain resolver — intermediate hops that resolve to a field with a dimension suffix correctly offer container methods on dot-trigger.
- ✅ Built-in method completion for `rand_mode` / `constraint_mode` / `randomize` on any-typed receiver — `syntax_member_completion` now unconditionally appends `UNIVERSAL_METHODS` (via `mimir_syntax::builtin_methods::universal_methods()`) after workspace-member and dimension-based builtin passes, so `rand_mode` / `constraint_mode` / `randomize` appear in the completion list for any class-typed receiver regardless of whether the class has declared members in the index.
- ⬜ Built-in hover for `this.push_back` / `super.push_back`-style calls — the `this`/`super` receiver arm in `builtin_method_hover_at` only queries `UNIVERSAL_METHODS`; container methods (queue, array, assoc) are excluded because the class type is not checked and false positives would be worse than silence.
- ✅ `textDocument/selectionRange` — smart "expand selection" (VS Code `Shift+Alt+→`, Emacs `expand-region`-style). Pure tree-sitter: [`mimir_syntax::selection::selection_ranges_at`](./crates/mimir-syntax/src/selection.rs) walks the parse tree from the leaf node under the cursor up to the root, emitting each ancestor's range so each keypress grows the selection to the next enclosing construct (identifier → expression → statement → `begin…end` block → function/task → class → module → file). Consecutive ancestors that share the exact same span (tree-sitter wrapper nodes) are collapsed so no keypress is a visible no-op. Handles a batch of cursor positions per request (multi-cursor) and links each into the LSP `SelectionRange` parent-chain at the server boundary. No symbol table needed; works without slang.
- ✅ `textDocument/documentLink` — clickable `` `include "..." `` paths. Scans the document for include directives via [`includes::scan_includes_with_spans`](./crates/mimir-server/src/includes.rs) (same comment/string-aware text scan the workspace indexer uses, but span-returning), resolves each filename against the file's own directory then the project `include_dirs` (same order as slang's preprocessor), and returns a `DocumentLink` whose range underlines just the filename and whose target is the resolved file URL. `resolve_provider: false` — the target is filled eagerly. Unresolved includes (macro-derived paths, missing headers) yield no link rather than a dead one. Pure text scan — no slang round-trip; the project `include_dirs` come from `.mimir.toml` and are available even when the sidecar isn't configured.
- ✅ `textDocument/codeLens` — "▷ overrides Base::method" lens above each method that overrides an ancestor; clicking jumps to the overridden declaration. Tree-sitter only (no slang): the enclosing class's `extends` chain is walked through the workspace index for the nearest ancestor declaring the same method. Computed in one stage (the title needs the base class name), so `resolve_provider` is `false`. Scope is set by `[code_lens] overrides` in `.mimir.toml`: `"uvm"` (default — only UVM phase methods like `build_phase`/`run_phase`), `"all"` (every override), or `"none"` (disabled). The lens fires the `mimir.gotoLocation` client command (a tiny VS Code shim that opens + reveals the target). Lives in [`code_lens`](./crates/mimir-server/src/code_lens.rs).

### Navigation

- ✅ `textDocument/definition` — MimirAst-first (when `MIMIR_SLANG_PATH` is configured and a cached `MimirAst` is available from the sidecar's `compile` RPC), tree-sitter workspace-index fallback otherwise. The MimirAst path is **receiver-aware** via slang's resolved-reference table (`MimirFile::references`): each elaborated `NamedValueExpression`, `HierarchicalValueExpression`, `MemberAccessExpression`, and free `CallExpression` is emitted with a narrowed use-site range and its resolved declaration target. A cursor request binary-searches that table, so inherited class fields, typedef chains, and package-imported symbols all jump correctly without name disambiguation guesswork (the receiver-vs-method `configure` collision that broke v0.7.8 is the canonical case). Falls back to the legacy scope-walk name lookup when no ref is recorded at the cursor (e.g. macro identifiers the visitor doesn't yet cover) and to the tree-sitter workspace index when no MimirAst is cached. Set `MIMIR_SLANG_EMIT_REFS=0` in the sidecar's environment to disable the reference table for pathologically large compilations where the JSON payload becomes a hot spot — the consumer transparently falls back to the legacy paths.
- ✅ `textDocument/declaration` — MimirAst-first with tree-sitter workspace-index fallback, mirroring `textDocument/definition`. The MimirAst path resolves to the declaration site (the identifier token of the declaring construct). The tree-sitter fallback looks up the symbol name in the same-file index and workspace index, identical to `goto_definition`. v1 deferral: **prototype-vs-body distinction** — `extern function` / `pure virtual` prototypes and their external `function_body_declaration` counterparts are both indexed under the same name; a future slice will add `is_prototype` tracking so declaration jumps to the prototype while definition jumps to the body.
- ✅ `textDocument/typeDefinition` — MimirAst-based (requires `MIMIR_SLANG_PATH`). Cursor on a variable / port / parameter / class-field reference → jumps to the *type's* declaration (typedef, class, enum, struct, packed union) using the `type_str` from the cached `MimirAst`. No tree-sitter fallback (type resolution requires semantic analysis).
- ✅ `textDocument/implementation` — MimirAst-based (requires `MIMIR_SLANG_PATH`). Cursor on a class → all directly-derived subclasses across the compilation unit (using `parent_class` links in the cached `MimirAst`). Method-level virtual dispatch lookup is a future slice.
- ✅ `textDocument/references` — workspace-wide "Find All References" for the identifier under the cursor. Tree-sitter only (the slang sidecar doesn't yet expose a references RPC, so cross-package homonym disambiguation is a future slice). Three sources, merged and deduped by `(url, range)`: (1) **same file** is scope-aware via `occurrences_of_at` — a `phase` local in `build_phase` doesn't bleed into another `phase` in `connect_phase`; (2) **other open buffers and closed filelist-hydrated files** both use `occurrences_of_scoped` against their cached parse trees — scope-pruned (nested re-declarations of the searched name are excluded) and fast (no re-parse); (3) **files whose trees couldn't be cached** (parse failures at hydration) fall back to the workspace-index declaration site only. Closed filelist files are indexed at startup and kept up-to-date via the workspace state (updated on `didChange`, rehydrated on file-watcher events), so full usage sites — not just declarations — are returned for every file in the filelist whether or not it is currently open. An **identifier presence index** (`name → Set<Url>`) is maintained alongside the symbol index so the per-file occurrence scan only visits files that actually contain the identifier token, skipping the rest in O(1) — the common case on large workspaces is O(files_containing_name) rather than O(all_filelist_files). Honours `ReferenceContext::include_declaration` by stripping locations equal to any known declaration range when `false`. Caps at 1000 results (logged at `warn!` when truncated) so the editor's peek list stays usable on popular UVM macros. Remaining limitations: no slang-backed scope/type-aware resolution (so `pkg_a::foo` and `pkg_b::foo` are conflated by name); no hierarchical-name support (`u_dut.fsm.state`), matching the deferral on `definition`.
- ✅ `callHierarchy/*` — tree-sitter-only call graph. `prepareCallHierarchy` resolves the function or task under the cursor to a `CallHierarchyItem`. `incomingCalls` scans open buffers and all filelist-hydrated files for call sites matching the callee's name, groups them by the nearest enclosing callable (`find_enclosing_callable`), and returns one `CallHierarchyIncomingCall` per distinct caller (with multiple `from_ranges` when the same caller calls the target more than once). Pre-filtered by the identifier presence index so only files that actually contain the name are scanned. `outgoingCalls` uses `call_sites_in` to enumerate every call made within the callee's body range and resolves each unique callee name against the workspace index. Capped at 500 callers / callees. Limitation: no slang-backed type-aware resolution — calls to a function and a same-named method in a different class are not distinguished.
- ✅ `typeHierarchy/*` — class inheritance hierarchy. `prepareTypeHierarchy` resolves the class name under the cursor; when the cursor is on an instance/handle instead of a type, it resolves the variable's declared type via `ast_features::type_name_at` (stripping any `#(...)` parameterization) and opens the hierarchy on that class. `supertypes` walks the `parent_class` / `parent_class_name` chain one hop at a time (slang path via MimirAst, tree-sitter fallback via workspace index `Symbol::parent_class_name`). `subtypes` scans the workspace index for all `Class` entries whose `parent_class_name` matches the queried class. Capped at 500 items. Note: lsp-types 0.94.1 (pinned via tower-lsp 0.20) has no static `type_hierarchy_provider` capability field, so the capability is advertised via `client/registerCapability` (`textDocument/prepareTypeHierarchy`, scoped to the `systemverilog`/`verilog` languages) in the `initialized` handler. This is what makes the "Show Type Hierarchy" menu entry appear in VS Code; move it back to a static field once tower-lsp/lsp-types is bumped past the gap.
- ⬜ Hierarchical-name hover / navigation (`u_dut.fsm.state`) — slang-only future slice; today's tree-sitter resolution stops at one segment.

### Refactoring

- ✅ `textDocument/rename` + `textDocument/prepareRename` — workspace-wide rename using the same reference engine as `textDocument/references` (scope-aware within a file, workspace-wide across open buffers and filelist-hydrated files). `prepareRename` validates the cursor is on an identifier and returns its span so the editor can pre-fill the input box. One `TextEdit` per occurrence per file, returned as a `WorkspaceEdit`. v1 limitations: tree-sitter only — no slang-backed scope/type-aware resolution (`pkg_a::foo` and `pkg_b::foo` are conflated by name); no hierarchical-name support; capped at 1 000 occurrences matching the `references` limit.
- ⬜ `textDocument/codeAction` (quick-fixes)
- ✅ `textDocument/formatting` — whole-file via `verible-verilog-format` (see [docs/formatter.md](docs/formatter.md))
- ✅ `textDocument/rangeFormatting` — selection snapped to whole lines, same backend

### Verification-focused features (the actual product goals)

- ⬜ UVM class-tree navigation (component/object hierarchy)
- 🚧 UVM phase awareness — jump to an overridden phase via the `textDocument/codeLens` "overrides Base::method" lens (see above; default scope is UVM phase methods). Phase-graph ordering / phase-jump-by-name is still pending.
- ⬜ UVM factory registration validation (`uvm_object_utils`, `uvm_component_utils`)
- ⬜ UVM sequence ↔ sequencer ↔ driver navigation
- ⬜ SVA property/sequence index, hover-preview of expansion
- ⬜ Functional coverage: covergroup/coverpoint/cross structure view
- ⬜ Constraint blocks: list `rand` variables, navigate constraint references
- ⬜ Test/testbench discovery & runner integration
- ⬜ Waveform-aware hover (signal width, last-driven location)

### Project / build integration

- ✅ Filelist (`.f` / `-f`) parsing for compilation units (via `.mimir.toml`'s `slang.filelist`); inline `slang.files` list for per-workspace additions without a separate `.f`
- ✅ `+define+` / `+incdir+` macro & include path config
- ✅ Multi-file elaboration & cross-file symbol resolution. The sidecar's `compile` RPC elaborates the whole compilation unit and exports the full `MimirAst` (elaborated symbol table) in one shot. The server kicks off a workspace-wide compile on `initialize` (before the user opens any file) so semantic features are warm by the time the first request lands. Cross-file goto-definition, type-definition, and implementation are wired through the cached `MimirAst` (slang primary) with a tree-sitter workspace-index fallback. The server hashes the `ElaborateParams` inputs (file texts + include dirs + defines + `top`) and skips the sidecar round-trip when a subsequent `did_open` / `did_change` produces an identical hash — `did_open`-ing a file that was already part of the startup compile is a cache hit.
- ⬜ Integration with simulator-specific build files (Verilator, Xcelium, VCS, Questa)

---

## Engineering principles

1. **Small, well-tested units.** Every crate has its own test suite. Public
   functions get a `#[cfg(test)]` block in the same file.
2. **No silent failures.** Errors are typed (`thiserror`), bubble up, and end
   up logged with `tracing` at an appropriate level.
3. **Incremental everything.** Documents are stored as ropes, parsing uses
   `tree_sitter::InputEdit` for incremental reparse on `didChange` (reuses
   unchanged subtrees), and we never re-parse the world to answer a single request.
4. **Async by default.** Everything that touches I/O is `async`. CPU-heavy
   parsing runs on `tokio::task::spawn_blocking` so it doesn't stall the
   reactor.
5. **Verification first.** When choosing what to build next, the question is
   "does this help a verification engineer?" — not "does this look good in a
   feature comparison table."

---

## License

Dual-licensed under MIT or Apache-2.0, at your option.
