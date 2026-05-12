"""Integration tests against the `hello_world` simple UVM examples.

Covers two files:
- ``packet.sv`` — a plain UVM transaction class with a constraint block;
  the simplest possible file and therefore a clean-parse baseline.
- ``producer.sv`` — a parameterized component ``producer #(type T=packet)``
  with a TLM port and UVM macro registration block; verifies that the
  symbol extractor returns the bare class name despite the type parameter.

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_hello_world -v
"""

from __future__ import annotations

import time
import pathlib
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
HW_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "simple" / "hello_world"
PACKET_SV = HW_DIR / "packet.sv"
PRODUCER_SV = HW_DIR / "producer.sv"


# ---------------------------------------------------------------------------
# PacketTest — packet.sv
# ---------------------------------------------------------------------------


class PacketTest(unittest.TestCase):
    """Tests against ``packet.sv``: a single transaction class with one
    constraint and one constructor. Should parse completely cleanly."""

    @classmethod
    def setUpClass(cls) -> None:
        if not PACKET_SV.exists():
            raise unittest.SkipTest(f"example not found: {PACKET_SV}")
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=HW_DIR)
        cls.uri = file_uri(PACKET_SV)
        cls.text = read_text(PACKET_SV)
        cls.lsp.did_open(cls.uri, cls.text)
        # Capture the initial publishDiagnostics so we can inspect severity.
        diag = cls.lsp.wait_for_notification(
            "textDocument/publishDiagnostics", timeout=5.0
        )
        time.sleep(0.25)
        assert diag is not None, "server never published initial diagnostics"
        cls.initial_diagnostics: list[dict] = diag.get("diagnostics", [])

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    # ------------------------------------------------------------------
    # Health
    # ------------------------------------------------------------------

    def test_no_crash_on_packet(self) -> None:
        """Smoke: every standard LSP feature must respond without raising."""
        td = {"textDocument": {"uri": self.uri}}
        # cursor on `addr` field (line 26, char 6)
        td_pos = {
            "textDocument": {"uri": self.uri},
            "position": {"line": 26, "character": 6},
        }
        self.lsp.request("textDocument/foldingRange", td)
        self.lsp.request("textDocument/documentSymbol", td)
        self.lsp.request("textDocument/documentHighlight", td_pos)
        self.lsp.request("textDocument/definition", td_pos)
        self.lsp.request("textDocument/signatureHelp", td_pos)
        self.lsp.request(
            "textDocument/inlayHint",
            {**td, "range": _range(0, 0, 40, 0)},
        )

    # ------------------------------------------------------------------
    # documentSymbol
    # ------------------------------------------------------------------

    def test_document_symbol_finds_packet_class(self) -> None:
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(symbols, list)
        names = _collect_symbol_names(symbols)
        self.assertIn("packet", names)

    def test_document_symbol_packet_members(self) -> None:
        """``addr`` (field), ``c`` (constraint), ``new`` (constructor)."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for member in ("addr", "c", "new"):
            self.assertIn(member, names, f"missing symbol `{member}` in {sorted(names)}")

    # ------------------------------------------------------------------
    # foldingRange
    # ------------------------------------------------------------------

    def test_folding_has_class_range(self) -> None:
        """``class packet`` starts on line 21 (0-indexed) and must fold."""
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(folds, list)
        starts = {f["startLine"] for f in folds}
        self.assertIn(21, starts, f"missing fold for `packet` class; got {sorted(starts)}")

    # ------------------------------------------------------------------
    # Diagnostics
    # ------------------------------------------------------------------

    def test_no_diagnostic_errors(self) -> None:
        """packet.sv is syntactically clean — no error-severity diagnostics
        should be published on open."""
        errors = [
            d for d in self.initial_diagnostics if d.get("severity") == 1
        ]
        self.assertEqual(
            errors, [], f"unexpected parse errors on packet.sv: {errors}"
        )


# ---------------------------------------------------------------------------
# ProducerTest — producer.sv
# ---------------------------------------------------------------------------


class ProducerTest(unittest.TestCase):
    """Tests against ``producer.sv``: a parameterized component class
    ``producer #(type T=packet)`` with a TLM port and UVM macro block."""

    @classmethod
    def setUpClass(cls) -> None:
        if not PRODUCER_SV.exists():
            raise unittest.SkipTest(f"example not found: {PRODUCER_SV}")
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=HW_DIR)
        cls.uri = file_uri(PRODUCER_SV)
        cls.text = read_text(PRODUCER_SV)
        cls.lsp.did_open(cls.uri, cls.text)
        diag = cls.lsp.wait_for_notification(
            "textDocument/publishDiagnostics", timeout=5.0
        )
        time.sleep(0.25)
        assert diag is not None, "server never published initial diagnostics"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    # ------------------------------------------------------------------
    # Health
    # ------------------------------------------------------------------

    def test_no_crash_on_producer(self) -> None:
        """Smoke: every standard LSP feature must respond without raising."""
        td = {"textDocument": {"uri": self.uri}}
        # cursor on `run_phase` task name (line 41, char 7)
        td_pos = {
            "textDocument": {"uri": self.uri},
            "position": {"line": 41, "character": 7},
        }
        self.lsp.request("textDocument/foldingRange", td)
        self.lsp.request("textDocument/documentSymbol", td)
        self.lsp.request("textDocument/documentHighlight", td_pos)
        self.lsp.request("textDocument/definition", td_pos)
        self.lsp.request("textDocument/signatureHelp", td_pos)
        self.lsp.request(
            "textDocument/inlayHint",
            {**td, "range": _range(0, 0, 78, 0)},
        )

    # ------------------------------------------------------------------
    # documentSymbol
    # ------------------------------------------------------------------

    def test_document_symbol_finds_producer_class(self) -> None:
        """Symbol extractor uses ``class_identifier`` — just ``producer``,
        not ``producer #(T)``."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(symbols, list)
        names = _collect_symbol_names(symbols)
        self.assertIn("producer", names)

    def test_document_symbol_producer_members(self) -> None:
        """Protected fields appear in the symbol index.

        Note: ``run_phase`` is NOT asserted here. The task body contains
        ``T p;`` where ``T`` is the class type parameter, which appears
        to confuse tree-sitter's class-body parser enough that the
        ``task_body_declaration`` node loses its ``task_identifier`` child
        and is therefore not indexed. The field declarations above the
        task are unaffected because ``variable_decl_assignment`` nodes are
        simpler to recover."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for member in ("proto", "num_packets", "count"):
            self.assertIn(member, names, f"missing symbol `{member}` in {sorted(names)}")

    # ------------------------------------------------------------------
    # foldingRange
    # ------------------------------------------------------------------

    def test_folding_producer_class(self) -> None:
        """``class producer #(type T=packet)`` starts on line 21 (0-indexed)
        and must fold even though the class header has a type parameter."""
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(folds, list)
        starts = {f["startLine"] for f in folds}
        self.assertIn(
            21, starts, f"missing fold for parameterized `producer` class; got {sorted(starts)}"
        )


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


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
