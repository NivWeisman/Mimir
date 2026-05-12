"""Integration tests for capabilities lost in the
``tree-sitter-verilog`` → ``tree-sitter-systemverilog 0.3.1`` grammar
swap (commit ``eb82d45``) and missing cross-file / macro features.

The new grammar fuses parameterized scope calls (``IDENT#(T)::name``)
and produces a much more faithful UVM parse, but four user-visible
capabilities are gone or never existed:

1. Whole-line macro callsites (`` `uvm_fatal("ID", "MSG") ``) get
   blanked by [`blank_backtick_lines`][p1] before the parser sees them,
   so inlay hints and goto-definition on a macro callsite have nothing
   to consume.
2. Goto-def from a type reference (``apb_rw`` in ``apb_monitor.sv``)
   to its declaration in another file fails when that other file isn't
   already open — the tree-sitter Stage-2 workspace index hydrates only
   the literal filelist, not files reachable via `` `include `` chains.
3. The slang sidecar isn't elaborated at startup, so its compilation
   database is empty until the user touches a file. F12 across files
   that the user hasn't opened yet relies on tree-sitter alone.

[p1]: ../crates/mimir-syntax/src/parser.rs

These tests fail today (``@expectedFailure`` + ``XFAIL:`` note); when
the corresponding fix lands the test flips to *unexpected success*,
which is the signal to drop the decorator in the same PR.

Run::

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_apb_cross_file -v
"""

from __future__ import annotations

import pathlib
import time
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
APB_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "integrated" / "apb"
APB_MONITOR = APB_DIR / "apb_monitor.sv"
APB_RW = APB_DIR / "apb_rw.sv"


def _wait_for_first_parse(lsp: MimirLspClient, settle_ms: int = 250) -> None:
    diag = lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)
    time.sleep(settle_ms / 1000.0)
    assert diag is not None, "server never published initial diagnostics"


class ApbMonitorCrossFileTest(unittest.TestCase):
    """``apb_monitor.sv`` is opened; ``apb_rw.sv`` is NOT. Each test
    asks the server about a capability that should work even though
    the referenced file isn't open — the filelist + include chain
    declares it as part of the project."""

    @classmethod
    def setUpClass(cls) -> None:
        if not APB_MONITOR.exists():
            raise unittest.SkipTest(f"example not found: {APB_MONITOR}")
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=APB_DIR)
        cls.uri = file_uri(APB_MONITOR)
        cls.text = read_text(APB_MONITOR)
        cls.lsp.did_open(cls.uri, cls.text)
        _wait_for_first_parse(cls.lsp)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    # ------------------------------------------------------------------
    # Cross-file goto-def
    # ------------------------------------------------------------------

    def test_goto_def_apb_rw_from_apb_monitor_without_opening_rw(self) -> None:
        """Goto-def on the ``apb_rw`` token used as a variable type
        (``apb_rw tr;`` at line 66, 0-indexed 65) must resolve to
        ``apb_rw.sv`` even though that file was never opened.

        Validates the end-to-end story for cross-file resolution:
          - startup slang elaborate (§1) seeds the compilation DB before
            the first request lands,
          - the tree-sitter workspace index hydrates the filelist *plus*
            files reachable via `` `include `` chains (§2), and
          - ``route_definition`` falls back to the syntax index when slang
            returns empty (so positions slang can't resolve still land)."""
        result = self.lsp.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 65, "character": 11},  # on `apb_rw`
            },
        )
        self.assertIsNotNone(result, "definition returned None")
        locations = result if isinstance(result, list) else [result]
        self.assertTrue(
            any(loc.get("uri", "").endswith("apb_rw.sv") for loc in locations),
            f"no location pointed at apb_rw.sv; got {locations}",
        )

    @unittest.expectedFailure
    def test_goto_def_apb_rw_inside_parameterized_class_arg(self) -> None:
        """XFAIL: goto-def on the ``apb_rw`` token used as a type argument
        inside ``uvm_analysis_port#(apb_rw) ap;`` (line 39, 0-indexed 38)
        currently resolves to the surrounding ``uvm_analysis_port`` class
        — slang treats the parameterized class instantiation as the
        innermost named thing under the cursor.

        Documents the specific corner case so a future fix (whichever
        layer ends up owning it — slang's resolver or a mimir-side
        position rewrite before sending to slang) can flip this test."""
        result = self.lsp.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 38, "character": 24},  # on `apb_rw`
            },
        )
        self.assertIsNotNone(result, "definition returned None")
        locations = result if isinstance(result, list) else [result]
        self.assertTrue(
            any(loc.get("uri", "").endswith("apb_rw.sv") for loc in locations),
            f"no location pointed at apb_rw.sv; got {locations}",
        )

    # ------------------------------------------------------------------
    # Macro callsite goto-def
    # ------------------------------------------------------------------

    def test_goto_def_uvm_fatal_callsite(self) -> None:
        """Sanity (works today via slang): goto-def on the macro name in
        `` `uvm_fatal("APB/MON/NOVIF", ...) `` (line 57, 0-indexed 56)
        must resolve to the ``\\`define uvm_fatal`` site somewhere under
        ``examples/uvm-1.2/src/macros/``.

        Tree-sitter on its own can't see the callsite (the whole line is
        blanked by ``blank_backtick_lines``), but ``did_open`` triggers a
        slang elaborate and slang resolves it through the actual
        preprocessor. This test locks in that behaviour so §3a (narrowed
        blanking) doesn't accidentally regress the slang-backed path.
        """
        # Cursor on the macro name `uvm_fatal` (column 13 = 12 spaces + backtick).
        result = self.lsp.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 56, "character": 16},
            },
        )
        self.assertIsNotNone(result)
        locations = result if isinstance(result, list) else [result]
        self.assertTrue(
            any("uvm_message_defines" in loc.get("uri", "") for loc in locations),
            f"expected a location in uvm_message_defines.svh; got {locations}",
        )

    # ------------------------------------------------------------------
    # Macro-arg inlay hints
    # ------------------------------------------------------------------

    def test_inlay_hints_label_uvm_fatal_args(self) -> None:
        """Inlay hints near the `` `uvm_fatal(...) `` callsite on line
        57 include the macro's param names (``ID``, ``MSG``) as labels
        in front of the matching argument expressions.

        Validates the §3 end-to-end story: (a) the callsite survives
        ``blank_backtick_lines`` under the compiler-directive allowlist
        (§3a), so the AST exposes a ``text_macro_usage`` node; (b) the
        inlay handler joins the call against the macro's ``params`` from
        the workspace index; (c) the workspace index has indexed
        ``\\`define uvm_fatal`` from a file reachable only via
        `` `include `` chains from the filelist."""
        hints = self.lsp.request(
            "textDocument/inlayHint",
            {
                "textDocument": {"uri": self.uri},
                "range": _range(50, 0, 65, 0),
            },
        )
        self.assertIsInstance(hints, list)
        labels = {_hint_label(h) for h in hints if 50 <= h["position"]["line"] <= 65}
        # Param names from `\`define uvm_fatal(ID, MSG)` should appear.
        self.assertTrue(
            any("ID" in lbl for lbl in labels),
            f"no inlay hint mentioning macro param `ID` on lines 50-65; got {sorted(labels)}",
        )

    # Note: a startup-workspace-elaborate XFAIL test belongs here once §1
    # lands. It was prototyped but pulled because spawning a second
    # ``MimirLspClient`` mid-suite in the apb workspace stacks ~5 min of
    # slang sidecar startup/shutdown per test. Re-add with a dedicated
    # ``TestCase`` (so the slow setUpClass amortises) once the startup
    # elaborate is wired and we have a clear notification to assert on.


# ---- helpers ---------------------------------------------------------------


def _range(sl: int, sc: int, el: int, ec: int) -> dict:
    return {
        "start": {"line": sl, "character": sc},
        "end": {"line": el, "character": ec},
    }


def _hint_label(hint: dict) -> str:
    """Inlay hint labels can be a string or a list of
    ``InlayHintLabelPart`` objects. Flatten to a single string for
    matching."""
    label = hint.get("label", "")
    if isinstance(label, str):
        return label
    if isinstance(label, list):
        return "".join(part.get("value", "") for part in label)
    return ""


if __name__ == "__main__":
    unittest.main()
