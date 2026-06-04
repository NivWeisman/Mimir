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
import tempfile
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
