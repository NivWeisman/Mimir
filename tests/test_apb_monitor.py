"""Integration tests against the real UVM `apb_monitor.sv` example.

These drive `mimir-server` over LSP/stdio exactly like an editor would.
They exist because `cargo test` only exercises pure-logic helpers — the
LSP wire path (parse → cache → handler → response) only runs at runtime,
and real-world SystemVerilog (UVM macros, classes, hierarchical names)
exposes corner cases that synthetic test fixtures don't.

Known broken cases are marked with `@unittest.expectedFailure` and carry
a `XFAIL:` note pointing at the underlying bug. When someone fixes the
bug the test will start passing — unittest reports that as an
"unexpected success", which is the signal to drop the decorator.

Run:

    cargo build --release -p mimir-server   # ensure binary is current
    python3 -m unittest tests.test_apb_monitor -v
"""

from __future__ import annotations

import pathlib
import time
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
APB_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "integrated" / "apb"
APB_MONITOR = APB_DIR / "apb_monitor.sv"


def _wait_for_first_parse(lsp: MimirLspClient, settle_ms: int = 250) -> None:
    """`did_open` triggers `reparse_and_publish` on a tokio task. Block
    until the server's `publishDiagnostics` for that URI lands, so the
    tree cache is populated before we fire feature requests."""
    diag = lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)
    time.sleep(settle_ms / 1000.0)
    assert diag is not None, "server never published initial diagnostics"


class ApbMonitorTest(unittest.TestCase):
    """All tests share one server instance — saves ~200ms per test on
    initialize.
    """

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
    # Health: the server responded to all standard requests
    # ------------------------------------------------------------------

    def test_no_crash_on_uvm_file(self) -> None:
        """Smoke check: fire every implemented feature request against
        the UVM file. The cache-on-DocumentState change should mean
        zero re-parses for these requests (visible in `mimir=trace`
        logs); here we just assert nothing errors out."""
        td = {"textDocument": {"uri": self.uri}}
        td_pos = {
            "textDocument": {"uri": self.uri},
            "position": {"line": 36, "character": 29},
        }
        # Each call below must return *something* (None or a result) and
        # must not raise. If any handler panics, the request times out.
        self.lsp.request("textDocument/foldingRange", td)
        self.lsp.request("textDocument/documentSymbol", td)
        self.lsp.request("textDocument/documentHighlight", td_pos)
        self.lsp.request("textDocument/definition", td_pos)
        self.lsp.request(
            "textDocument/signatureHelp",
            {**td_pos, "position": {"line": 55, "character": 44}},
        )
        self.lsp.request(
            "textDocument/inlayHint",
            {**td, "range": _range(0, 0, 100, 0)},
        )

    # ------------------------------------------------------------------
    # foldingRange — what currently works
    # ------------------------------------------------------------------

    def test_folding_returns_some_ranges(self) -> None:
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(folds, list)
        self.assertGreater(len(folds), 0, "no folds at all on a 100-line class file")
        # The `apb_monitor_cbs` class (lines 30-33, 0-indexed 29-32)
        # parses cleanly — it must show up.
        starts = {f["startLine"] for f in folds}
        self.assertIn(29, starts, f"missing fold for `apb_monitor_cbs`; got {sorted(starts)}")

    @unittest.expectedFailure
    def test_folding_covers_inner_apb_monitor_class(self) -> None:
        """XFAIL: tree-sitter-verilog can't recover the `class_declaration`
        wrapper for `apb_monitor` because the parameterized scope access
        `uvm_config_db#(apb_vif)::get(...)` inside `build_phase` puts the
        tree into an ERROR root. The body fragments survive as loose
        `class_item` nodes, but the class envelope is gone, so the folder
        doesn't emit a fold for it. Same root cause as the missing class
        methods in `test_document_symbol_includes_inner_class_methods`."""
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        starts = {f["startLine"] for f in folds}
        self.assertIn(35, starts)  # `class apb_monitor extends uvm_monitor;` at line 36

    # ------------------------------------------------------------------
    # documentSymbol — what currently works
    # ------------------------------------------------------------------

    def test_document_symbol_finds_classes_and_fields(self) -> None:
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(symbols, list)
        names = _collect_symbol_names(symbols)
        # Class declarations (both classes' names survive even when the
        # second class's body is wrecked).
        self.assertIn("apb_monitor", names)
        self.assertIn("apb_monitor_cbs", names)
        # Fields inside the broken class still get extracted.
        for field in ("sigs", "ap", "cfg"):
            self.assertIn(field, names, f"missing field `{field}` in {sorted(names)}")
        # Macro definition at the top of the file.
        self.assertIn("APB_MONITOR__SV", names)

    @unittest.expectedFailure
    def test_document_symbol_includes_inner_class_methods(self) -> None:
        """XFAIL: methods inside `apb_monitor` (`new`, `build_phase`,
        `run_phase`, `trans_observed`) are not indexed because the
        enclosing `class_declaration` parse fails (see comment on
        `test_folding_covers_inner_apb_monitor_class`). The constructor
        `new` survives as a free-standing `class_constructor_declaration`
        but our `SymbolKind::from_node_kind` doesn't recognize that
        node kind, and the other methods lose their
        `function_body_declaration`/`task_body_declaration` wrapping in
        the ERROR-recovery tree shape."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for method in ("new", "build_phase", "run_phase"):
            self.assertIn(method, names)

    # ------------------------------------------------------------------
    # inlayHint
    # ------------------------------------------------------------------

    def test_inlay_hints_returned_for_viewport(self) -> None:
        """Pure tree-sitter inlay hints skip method calls (no receiver
        type without slang). With the broken parse, free-function calls
        like `trans_observed(tr)` may not be detected as calls either.
        The handler must still respond cleanly with a list."""
        hints = self.lsp.request(
            "textDocument/inlayHint",
            {"textDocument": {"uri": self.uri}, "range": _range(0, 0, 100, 0)},
        )
        self.assertIsInstance(hints, list)
        for hint in hints:
            self.assertIn("position", hint)
            self.assertIn("label", hint)

    # ------------------------------------------------------------------
    # signatureHelp — fallback returns None gracefully
    # ------------------------------------------------------------------

    def test_signature_help_does_not_crash(self) -> None:
        """Tree-sitter fallback should return a SignatureHelp object or
        None — never raise. Without slang and with the broken inner
        class, most positions will return None; that's acceptable."""
        result = self.lsp.request(
            "textDocument/signatureHelp",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 55, "character": 44},
            },
        )
        if result is not None:
            self.assertIn("signatures", result)
            self.assertIsInstance(result["signatures"], list)

    # ------------------------------------------------------------------
    # documentHighlight
    # ------------------------------------------------------------------

    def test_document_highlight_does_not_crash(self) -> None:
        """`sigs` is referenced ~10 times in `apb_monitor`. With the
        broken parse the scope-aware highlighter falls back to a tight
        scope and returns fewer matches than ideal, but it must not
        crash."""
        result = self.lsp.request(
            "textDocument/documentHighlight",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 36, "character": 29},  # on `sigs`
            },
        )
        if result is not None:
            for hl in result:
                self.assertIn("range", hl)

    @unittest.expectedFailure
    def test_document_highlight_sigs_finds_all_uses(self) -> None:
        """XFAIL: same root cause — `sigs` is the class field, and the
        scope walk can't find the enclosing `class_declaration`. Of the
        ~10 actual uses, only 2-3 surface."""
        result = self.lsp.request(
            "textDocument/documentHighlight",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 36, "character": 29},
            },
        )
        self.assertIsNotNone(result)
        self.assertGreaterEqual(len(result), 8)

    # ------------------------------------------------------------------
    # AST-cache behaviour: subsequent requests don't re-parse.
    # ------------------------------------------------------------------

    def test_repeated_folding_does_not_reparse(self) -> None:
        """Fire `foldingRange` twice on the same document without any
        edits between. The cache must serve the second call without
        invoking `SyntaxParser::parse` (the parser emits a `parse
        complete` log line per call). We can't directly observe the
        log from here without enabling trace logs, but we can assert
        the two responses are identical, which is the user-visible
        contract."""
        td = {"textDocument": {"uri": self.uri}}
        first = self.lsp.request("textDocument/foldingRange", td)
        second = self.lsp.request("textDocument/foldingRange", td)
        self.assertEqual(first, second)


# ---- helpers ---------------------------------------------------------------


def _range(sl: int, sc: int, el: int, ec: int) -> dict:
    return {
        "start": {"line": sl, "character": sc},
        "end": {"line": el, "character": ec},
    }


def _collect_symbol_names(syms: list[dict]) -> set[str]:
    out: set[str] = set()
    for s in syms:
        if "name" in s:
            out.add(s["name"])
        for child in s.get("children", []) or []:
            out |= _collect_symbol_names([child])
    return out


if __name__ == "__main__":
    unittest.main()
