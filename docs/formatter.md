# Mimir formatter configuration

Mimir delegates LSP document formatting to
[`verible-verilog-format`](https://github.com/chipsalliance/verible), a
production-grade SystemVerilog formatter from Google. Both
`textDocument/formatting` (whole file) and `textDocument/rangeFormatting`
(selection, snapped to whole lines) are supported.

## Quick start

Install Verible (see [Installing Verible](#installing-verible)), then add a
`[formatter]` table to your `.mimir.toml`:

```toml
[formatter]
column_limit       = 100
indentation_spaces = 2
```

That's it — the editor's "Format Document" and "Format Selection" commands now
route through Verible.

## How it works

On every formatting request mimir:

1. Reads the document text from its in-memory rope.
2. Spawns `verible-verilog-format` with the flags derived from `[formatter]`.
3. For range requests, appends `--lines START-END` (1-based, inclusive). Verible
   still outputs the full file; only the requested lines are changed.
4. Pipes the document to the process stdin and reads stdout.
5. Returns a single `TextEdit` replacing the entire document with the formatted
   result.

The formatter runs with a hard 5-second timeout. If Verible isn't installed or
fails, mimir logs the error to its stderr and returns no edits (the document is
left unchanged).

## Configuration reference

All fields live under `[formatter]` in `.mimir.toml`. Every field except
`binary` is optional — omitting it tells Verible to use its own built-in
default, listed in the table below.

| TOML field | Verible flag | Type | Verible default | What it controls |
|---|---|---|---|---|
| `binary` | _(binary path)_ | string | `"verible-verilog-format"` | Path or name of the formatter executable. Resolved via `PATH` when no `/` is present. |
| `column_limit` | `--column_limit` | integer | `100` | Maximum line length (columns) before wrapping is applied. |
| `indentation_spaces` | `--indentation_spaces` | integer | `2` | Number of spaces per indentation level. |
| `wrap_spaces` | `--wrap_spaces` | integer | `4` | Extra indentation spaces for lines that are wrapped as continuations. |
| `try_wrap_long_lines` | `--try_wrap_long_lines` | bool | `false` | When `true`, actively break lines that exceed `column_limit`. When `false`, Verible wraps only where grammar allows. |
| `port_declarations_alignment` | `--port_declarations_alignment` | string | `"flush-left"` | Column alignment for port declaration lists (`input logic clk, …`). |
| `assignment_statement_alignment` | `--assignment_statement_alignment` | string | `"flush-left"` | Column alignment for `=` and `<=` in procedural blocks. |
| `named_parameter_alignment` | `--named_parameter_alignment` | string | `"flush-left"` | Column alignment for named parameter connections (`.PARAM(value)`). |
| `named_port_alignment` | `--named_port_alignment` | string | `"flush-left"` | Column alignment for named port connections (`.port(wire)`). |
| `module_net_variable_alignment` | `--module_net_variable_alignment` | string | `"flush-left"` | Column alignment for net/variable declarations inside modules. |
| `formal_parameters_alignment` | `--formal_parameters_alignment` | string | `"flush-left"` | Column alignment for formal parameter lists (`#(parameter int W = 8)`). |
| `class_member_variable_alignment` | `--class_member_variable_alignment` | string | `"flush-left"` | Column alignment for class member variable declarations. |
| `struct_union_members_alignment` | `--struct_union_members_alignment` | string | `"flush-left"` | Column alignment for `struct` and `union` member declarations. |
| `extra_args` | _(pass-through)_ | string array | `[]` | Raw flags appended verbatim to every Verible invocation. Use for any flag not listed above. |
| `wrap_ifdefs` | _(mimir only)_ | bool | `true` | When `true`, mimir wraps `` `ifdef ``/`` `ifndef `` blocks with `/* verilog_format: off/on */` pragmas before invoking Verible. This lets Verible reformat surrounding code even when preprocessor guards span statement boundaries (common in UVM and simulator-specific blocks). Set to `false` to pass source text unmodified. |

### `wrap_ifdefs` details

Verible exits 0 but returns the file unchanged when it encounters `` `ifdef ``
blocks that span statement boundaries (e.g. simulator guards like `` `ifdef VCS
if (!triggered) `endif``). With `wrap_ifdefs = true` (default) mimir
automatically detects these blocks and inserts temporary `/* verilog_format:
off/on */` markers around them so the rest of the file is still reformatted.
The markers are stripped from Verible's output before the edit is applied.

**Header guards** (a top-level `` `ifndef X `` / `` `define X `` … `` `endif ``
spanning the whole file) are handled specially: only the three guard lines
themselves are frozen; the file body is still formatted normally, and nested
`` `ifdef `` blocks inside the body are individually wrapped.

`` `else `` and `` `elsif `` branches are transparent — they live inside an
already-frozen block and do not need separate wrapping.

Set `wrap_ifdefs = false` if you manage format-off pragmas yourself or find the
automatic wrapping interferes with your workflow:

```toml
[formatter]
wrap_ifdefs = false
```

### Alignment values

All `*_alignment` fields accept one of three values:

| Value | Effect |
|---|---|
| `"flush-left"` | Each item starts at its natural indentation column. No extra alignment applied. |
| `"align"` | Items in a block are padded so their names/values line up vertically. |
| `"preserve"` | Leave the existing whitespace as-is; Verible does not reformat this construct. |

**Example — `"align"` on port declarations:**

```systemverilog
// flush-left (default)
module dut (
  input  logic       clk,
  input  logic [7:0] data,
  output logic       valid
);

// align
module dut (
  input  logic       clk,
  input  logic [7:0] data,
  output logic       valid
);
```

```systemverilog
// align on named port connections
dut u_dut (
  .clk  (clk),
  .data (in_data),
  .valid(out_valid)
);
```

### `extra_args` examples

Pass any flag `verible-verilog-format` supports that isn't listed above:

```toml
[formatter]
extra_args = [
  "--expand_coverpoints",          # expand coverpoints to a single long line
  "--failsafe_success=false",      # propagate Verible's exit code on error
]
```

Run `verible-verilog-format --helpfull` for the complete flag reference.

## Disabling formatting

To stop mimir from advertising `textDocument/formatting` and
`textDocument/rangeFormatting` — for example, if you run Verible through a
separate pre-commit hook or a different editor plugin — set:

```toml
[features]
formatting = false
```

The capability disappears from `ServerCapabilities` and the editor will not
offer "Format Document" via mimir.

## Installing Verible

### Pre-built binary (recommended)

Download a static binary for your platform from the
[Verible releases page](https://github.com/chipsalliance/verible/releases).
Place `verible-verilog-format` somewhere on your `PATH`, or point mimir at
it directly:

```toml
[formatter]
binary = "/opt/verible/bin/verible-verilog-format"
```

### Using `make verible` (local development)

The Mimir repository ships a Makefile target that downloads a pinned Linux
static binary into `tools/verible/` (listed in `.gitignore`):

```bash
make verible
# binary lands at tools/verible/bin/verible-verilog-format
```

To run the formatter integration tests after downloading:

```bash
make verible
VERIBLE_BIN=$(pwd)/tools/verible/bin/verible-verilog-format \
  cargo test -p mimir-server -- --include-ignored format
```

Override the pinned version or platform:

```bash
make verible \
  VERIBLE_VERSION=v0.0-4053-g89d4d98a \
  VERIBLE_PLATFORM=macOS
```

### Package managers

| Platform | Command |
|---|---|
| Ubuntu / Debian | `sudo apt install verible` (may lag behind upstream) |
| macOS (Homebrew) | `brew install verible` |
| From source | See the [Verible build guide](https://github.com/chipsalliance/verible#build) |
