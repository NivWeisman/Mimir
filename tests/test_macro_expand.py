"""Hermetic end-to-end test for the `mimir/expandMacro` custom request.

Builds a throwaway project in a tempdir with a *nested* macro and asks the
server to expand it. Exercises the full path:

    VS-Code-style request  →  Backend::expand_macro  →  SlangService /
    SlangAdapter  →  sidecar `handle_expand_macro`  →  slang preprocessor.

Requires the slang sidecar (the expansion is computed entirely in C++).
Skips cleanly when `MIMIR_SLANG_PATH` is unset or points nowhere — the same
contract every other slang-dependent test in this suite follows.

Run manually:

    MIMIR_SLANG_PATH=slang-sidecar/build/mimir-slang-sidecar \\
        python3 -m unittest tests.test_macro_expand -v
"""

from __future__ import annotations

import os
import pathlib
import sys
import tempfile
import time
import unittest

from .lsp_client import MimirLspClient, file_uri


def _require_slang() -> str:
    path = os.environ.get("MIMIR_SLANG_PATH")
    if not path:
        raise unittest.SkipTest(
            "MIMIR_SLANG_PATH not set — macro expansion needs the slang sidecar"
        )
    if not pathlib.Path(path).is_file():
        raise unittest.SkipTest(f"MIMIR_SLANG_PATH points at {path} but no file is there")
    return path


# A nested macro: `A(x)` expands to `( `B(x) * 2)`, and `B(x)` to `((x)+1)`,
# so `A(k)` must fully expand to `(((k)+1)*2)`. The line with the usage is
# index 3 (0-based); `A` sits at character 11 (`  int y = ` is 10 chars,
# then the backtick at col 10, `A` at col 11).
_FIXTURE = """\
`define B(x) ((x)+1)
`define A(x) (`B(x)*2)
module m;
  int y = `A(k);
endmodule
"""
_USAGE_LINE = 3
_USAGE_CHAR = 11


class MacroExpandTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        slang_path = _require_slang()

        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        cls._sv = root / "top.sv"
        cls._sv.write_text(_FIXTURE)
        (root / "files.f").write_text("top.sv\n")
        (root / ".mimir.toml").write_text(
            "[slang]\nfilelist = \"files.f\"\n"
        )

        cls.lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": slang_path})
        cls.lsp.initialize(workspace_root=root)
        cls.uri = file_uri(cls._sv)
        cls.lsp.did_open(cls.uri, _FIXTURE)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def _expand(self, line: int, character: int):
        return self.lsp.request(
            "mimir/expandMacro",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": line, "character": character},
            },
            timeout=30.0,
        )

    def test_nested_macro_expands_recursively(self) -> None:
        result = self._expand(_USAGE_LINE, _USAGE_CHAR)
        self.assertIsNotNone(result, "expected an expansion for `A(k)")
        self.assertEqual(result["name"], "A")
        # Whitespace-insensitive check: the fully-recursive expansion must
        # contain the inner `B expansion with the argument substituted.
        compact = "".join(result["expansion"].split())
        self.assertEqual(compact, "(((k)+1)*2)", f"got: {result['expansion']!r}")
        self.assertGreaterEqual(result["lineCount"], 1)

    def test_cursor_not_on_macro_returns_null(self) -> None:
        # `module` keyword on line 2 — not a macro usage.
        result = self._expand(2, 0)
        self.assertIsNone(result)


# A *multi-line* nested macro (the UVM-style case): `RECORD` expands to a
# `FIELD` call plus a function, each macro body written with `\` line
# continuations. The fully-recursive expansion must stay multi-line — the
# preprocessor's per-token trivia carries the body's newlines — not collapse
# to one line.
_ML_FIXTURE = """\
`define FIELD(n) \\
  int n; \\
  bit n``_valid;
`define RECORD(r) \\
  `FIELD(r) \\
  function void show_``r(); \\
  endfunction
module m;
  `RECORD(data)
endmodule
"""
_ML_USAGE_LINE = 8
_ML_USAGE_CHAR = 5


class MacroExpandMultiLineTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        slang_path = _require_slang()
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        cls._sv = root / "top.sv"
        cls._sv.write_text(_ML_FIXTURE)
        (root / "files.f").write_text("top.sv\n")
        (root / ".mimir.toml").write_text("[slang]\nfilelist = \"files.f\"\n")
        cls.lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": slang_path})
        cls.lsp.initialize(workspace_root=root)
        cls.uri = file_uri(cls._sv)
        cls.lsp.did_open(cls.uri, _ML_FIXTURE)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def test_multiline_nested_macro_stays_multiline(self) -> None:
        result = self.lsp.request(
            "mimir/expandMacro",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": _ML_USAGE_LINE, "character": _ML_USAGE_CHAR},
            },
            timeout=30.0,
        )
        self.assertIsNotNone(result, "expected an expansion for `RECORD(data)")
        self.assertEqual(result["name"], "RECORD")
        expansion = result["expansion"]
        # Must be multi-line, not collapsed to one line.
        self.assertGreater(
            result["lineCount"], 1,
            f"multi-line macro collapsed to {result['lineCount']} line(s): {expansion!r}",
        )
        self.assertIn("\n", expansion, f"no newline in expansion: {expansion!r}")
        # Inner `FIELD was recursively expanded (with token-paste applied).
        compact = "".join(expansion.split())
        self.assertIn("intdata;", compact, f"inner FIELD not expanded: {expansion!r}")
        self.assertIn("bitdata_valid;", compact, f"token-paste lost: {expansion!r}")


# The common UVM layout: the macro is defined in a package file, which then
# `` `include ``s the component file. The component is NOT a filelist member —
# it's reached only via the include and relies on the package having defined
# the macro first. Opening the component and expanding the macro must work
# even though, preprocessed standalone, the macro would be undefined.
_PKG = """\
`define MK(T) \\
  typedef int T; \\
  function void T``_f(); \\
  endfunction
`include "comp.sv"
"""
_COMP = """\
module comp;
  `MK(widget)
endmodule
"""
_COMP_USAGE_LINE = 1
_COMP_USAGE_CHAR = 4


class MacroExpandIncludeMemberTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        slang_path = _require_slang()
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        cls._pkg = root / "pkg.sv"
        cls._comp = root / "comp.sv"
        cls._pkg.write_text(_PKG)
        cls._comp.write_text(_COMP)
        # Only pkg.sv is in the filelist; comp.sv is pulled in via `include.
        (root / "files.f").write_text("pkg.sv\n")
        (root / ".mimir.toml").write_text(
            "[slang]\nfilelist = \"files.f\"\ninclude_dirs = [\".\"]\n"
        )
        cls.lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": slang_path})
        cls.lsp.initialize(workspace_root=root)
        # Open the *include-member* component file, not the package.
        cls.uri = file_uri(cls._comp)
        cls.lsp.did_open(cls.uri, _COMP)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def test_expand_macro_in_include_member_file(self) -> None:
        result = self.lsp.request(
            "mimir/expandMacro",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": _COMP_USAGE_LINE, "character": _COMP_USAGE_CHAR},
            },
            timeout=30.0,
        )
        self.assertIsNotNone(
            result,
            "expand returned None for a macro in an `include-member file "
            "(the macro is defined by the package that includes it)",
        )
        self.assertEqual(result["name"], "MK")
        self.assertGreater(result["lineCount"], 1, f"got: {result['expansion']!r}")
        compact = "".join(result["expansion"].split())
        self.assertIn("typedefintwidget;", compact, f"got: {result['expansion']!r}")
        self.assertIn("widget_f", compact, f"token-paste lost: {result['expansion']!r}")


# Regression guard for the "Expand Macro does nothing / hover footer never
# appears" bug: expansion shares NO connection with the heavy elaborate, so a
# long (or stuck) background compile must not stall it. The project below has a
# tiny target file plus several large sibling files, so the elaborate holds its
# (separate) connection for many seconds — far longer than the bound asserted
# here. Before the dedicated expand sidecar, the command blocked on the compile
# connection for the *whole* elaborate (the user saw it "do nothing").
_BUSY_TARGET = "`define A(x) (`B(x)*2)\n`define B(x) ((x)+1)\nmodule top; int y = `A(k); endmodule\n"
_BUSY_USAGE_LINE = 2
_BUSY_USAGE_CHAR = 21  # on the `A identifier
# Big enough that the elaborate runs for well over _MAX_LATENCY_S even on fast
# hardware; the compile never finishes during the test (the client closes
# first), so test wall-time stays small regardless.
_BUSY_SIBLINGS = 8
_BUSY_MODULES_PER_SIBLING = 6000
_MAX_LATENCY_S = 8.0


# A macro defined TWO ways behind `ifdef — the UVM `ifdef UVM_EMPTY_MACROS
# pattern, where every `uvm_field_* / utility macro has a real body in one
# branch and an empty body in the other. Expansion must follow the active
# +define+ state, and a defined-but-empty branch must be reported as such
# (not silently treated as "cursor isn't on a macro").
_COND_MACROS = """\
`ifdef MY_EMPTY
`define util(T)
`else
`define util(T) function void name_of_``T(); endfunction
`endif
"""
_COND_TOP = """\
`include "macros.svh"
module m;
  `util(widget)
endmodule
"""
_COND_USAGE_LINE = 2
_COND_USAGE_CHAR = 4


class ConditionalMacroDefinitionTest(unittest.TestCase):
    """Conditional (`ifdef-guarded) macro definitions resolve to the branch
    the active +define+ state selects, and the empty branch is honest."""

    def _server(self, extra_toml: str):
        slang_path = _require_slang()
        tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(tmp.name)
        (root / "macros.svh").write_text(_COND_MACROS)
        (root / "top.sv").write_text(_COND_TOP)
        (root / "files.f").write_text("top.sv\n")
        (root / ".mimir.toml").write_text(
            '[slang]\nfilelist = "files.f"\ninclude_dirs = ["."]\n' + extra_toml
        )
        lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": slang_path})
        lsp.initialize(workspace_root=root)
        uri = file_uri(root / "top.sv")
        lsp.did_open(uri, _COND_TOP)
        return tmp, lsp, uri

    def _expand(self, lsp, uri):
        return lsp.request(
            "mimir/expandMacro",
            {
                "textDocument": {"uri": uri},
                "position": {"line": _COND_USAGE_LINE, "character": _COND_USAGE_CHAR},
            },
            timeout=30.0,
        )

    def test_full_branch_when_define_absent(self) -> None:
        tmp, lsp, uri = self._server("")
        try:
            r = self._expand(lsp, uri)
            self.assertIsNotNone(r, "expected the non-empty branch to expand")
            self.assertEqual(r["name"], "util")
            self.assertIn("name_of_widget", "".join(r["expansion"].split()))
            self.assertIsNone(r.get("error"))
        finally:
            lsp.close()
            tmp.cleanup()

    def test_empty_branch_reported_when_define_set(self) -> None:
        tmp, lsp, uri = self._server('defines = ["MY_EMPTY"]\n')
        try:
            r = self._expand(lsp, uri)
            self.assertIsNotNone(
                r, "a defined-but-empty macro must not be reported as 'not a macro'"
            )
            self.assertEqual(r["name"], "util")
            self.assertEqual(r["expansion"], "")
            self.assertEqual(r["lineCount"], 0)
            self.assertIn("expands to nothing", r.get("error") or "")
        finally:
            lsp.close()
            tmp.cleanup()


REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
RISCV_DV = REPO_ROOT / "examples" / "riscv-dv"
UVM_SRC = REPO_ROOT / "examples" / "uvm-1.2" / "src"
VECTOR_CFG = RISCV_DV / "src" / "riscv_vector_cfg.sv"
# `uvm_field_queue_int(legal_eew, UVM_DEFAULT)` — line 117 (1-based) in
# riscv_vector_cfg.sv, cursor inside the macro name.
_QUEUE_INT_LINE = 116
_QUEUE_INT_CHAR = 8
# A cached repeat must be a position lookup (milliseconds of sidecar time
# plus LSP overhead), nowhere near the multi-second cold preprocess. The
# bound is generous for slow CI but far below one re-preprocess.
_REPEAT_BUDGET_S = 3.0


class UvmFieldQueueIntTimingTest(unittest.TestCase):
    """Dedicated timing + caching regression test for the user-reported
    "`uvm_field_queue_int` takes minutes to expand" bug.

    Reproduces the real-world shape on real sources: riscv-dv's package
    files as the compilation unit (with UVM-1.2 on the include path), the
    macro usage inside riscv_vector_cfg.sv — a file reached only via
    `` `include ``, so the sidecar must preprocess the whole unit once.
    *Three* non-filelist files are open, which is what used to randomise
    the assembled file order (HashMap iteration), flip the input hash on
    every request, and force a full re-preprocess per expand.

    Asserts the cold expand completes and that repeat expansions with no
    change are answered from the cache.
    """

    @classmethod
    def setUpClass(cls) -> None:
        slang_path = _require_slang()
        if not VECTOR_CFG.is_file():
            raise unittest.SkipTest(f"riscv-dv not cloned at {RISCV_DV}")
        if not (UVM_SRC / "uvm_macros.svh").is_file():
            raise unittest.SkipTest(f"uvm-1.2 not present at {UVM_SRC}")

        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        cu_files = [
            RISCV_DV / "src" / "riscv_signature_pkg.sv",
            RISCV_DV / "src" / "riscv_instr_pkg.sv",
            RISCV_DV / "test" / "riscv_instr_test_pkg.sv",
            RISCV_DV / "test" / "riscv_instr_gen_tb_top.sv",
        ]
        (root / "files.f").write_text("".join(f"{p}\n" for p in cu_files))
        (root / ".mimir.toml").write_text(
            "[slang]\n"
            'filelist = "files.f"\n'
            f'include_dirs = ["{RISCV_DV}/src", "{RISCV_DV}/test", '
            f'"{RISCV_DV}/target/rv32imc", "{UVM_SRC}"]\n'
        )
        # Two scratch files; together with the target they are three open
        # docs outside the filelist (the order-instability trigger).
        scratches = []
        for name in ("scratch_a.sv", "scratch_b.sv"):
            p = root / name
            p.write_text(f"module {name.removesuffix('.sv')}; endmodule\n")
            scratches.append(p)

        cls.lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": slang_path})
        cls.lsp.initialize(workspace_root=root)
        for p in scratches:
            cls.lsp.did_open(file_uri(p), p.read_text())
        cls.uri = file_uri(VECTOR_CFG)
        cls.lsp.did_open(cls.uri, VECTOR_CFG.read_text())

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def _expand_timed(self) -> tuple[dict | None, float]:
        start = time.monotonic()
        result = self.lsp.request(
            "mimir/expandMacro",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": _QUEUE_INT_LINE, "character": _QUEUE_INT_CHAR},
            },
            timeout=120.0,
        )
        return result, time.monotonic() - start

    def test_cold_expand_then_cached_repeats(self) -> None:
        result, cold = self._expand_timed()
        self.assertIsNotNone(result, "no expansion for `uvm_field_queue_int")
        self.assertIn(
            "legal_eew", result["expansion"],
            "expansion does not contain the queue field name",
        )
        self.assertGreater(result["lineCount"], 10)

        repeats = []
        for _ in range(5):
            r, dt = self._expand_timed()
            self.assertIsNotNone(r, "repeat expansion went missing")
            repeats.append(dt)

        worst = max(repeats)
        print(
            f"\n[uvm_field_queue_int] cold={cold:.2f}s "
            f"repeats={['%.3f' % t for t in repeats]} worst={worst:.3f}s",
            file=sys.stderr,
        )
        self.assertLess(
            worst, _REPEAT_BUDGET_S,
            f"repeat expand took {worst:.2f}s (cold was {cold:.2f}s) — the "
            "expand cache is not being hit for consecutive unchanged requests",
        )


class MacroExpandDuringCompileTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        slang_path = _require_slang()
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        (root / "top.sv").write_text(_BUSY_TARGET)
        files = ["top.sv"]
        for n in range(_BUSY_SIBLINGS):
            body = "".join(
                f"module big{n}_{i}; int z{i}; endmodule\n"
                for i in range(_BUSY_MODULES_PER_SIBLING)
            )
            (root / f"big{n}.sv").write_text(body)
            files.append(f"big{n}.sv")
        (root / "files.f").write_text("\n".join(files) + "\n")
        # debounce_ms = 0 so the elaborate starts immediately on did_open.
        (root / ".mimir.toml").write_text(
            '[slang]\nfilelist = "files.f"\ndebounce_ms = 0\n'
        )
        cls.lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": slang_path})
        cls.lsp.initialize(workspace_root=root)
        cls.uri = file_uri(root / "top.sv")
        cls.lsp.did_open(cls.uri, _BUSY_TARGET)
        # Give the background elaborate a moment to acquire its connection.
        time.sleep(0.3)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def test_command_expands_while_elaborate_is_in_flight(self) -> None:
        worst = 0.0
        for _ in range(5):
            start = time.monotonic()
            result = self.lsp.request(
                "mimir/expandMacro",
                {
                    "textDocument": {"uri": self.uri},
                    "position": {"line": _BUSY_USAGE_LINE, "character": _BUSY_USAGE_CHAR},
                },
                timeout=30.0,
            )
            worst = max(worst, time.monotonic() - start)
            self.assertIsNotNone(
                result, "Expand Macro returned nothing while a compile was running"
            )
            self.assertEqual(result["name"], "A")
            self.assertEqual("".join(result["expansion"].split()), "(((k)+1)*2)")
        # If expansion still shared the compile connection it would have blocked
        # for the whole multi-second elaborate; the dedicated connection keeps it
        # well under the bound.
        self.assertLess(
            worst,
            _MAX_LATENCY_S,
            f"expand blocked {worst:.1f}s — is it still queuing behind elaborate?",
        )

    def test_hover_footer_appears_while_elaborate_is_in_flight(self) -> None:
        # The opportunistic hover footer must also reach the (uncontended) expand
        # connection, not silently drop because the compile connection is busy.
        result = self.lsp.request(
            "textDocument/hover",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": _BUSY_USAGE_LINE, "character": _BUSY_USAGE_CHAR},
            },
            timeout=30.0,
        )
        self.assertIsNotNone(result)
        self.assertIn(
            "expands to",
            str(result),
            "hover footer missing during compile (expand connection contended?)",
        )
