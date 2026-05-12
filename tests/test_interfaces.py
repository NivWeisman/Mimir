"""Integration tests against interface and multi-construct UVM example files.

Covers two files:

- ``simple/interfaces/interface.sv`` — a single file that contains an
  interface (``pin_if``) with modports, two packages (``top_pkg``,
  ``user_pkg``) each with nested classes, plus three modules (``dut``,
  ``clkgen``, ``top``).  Exercises the server's ability to index all
  top-level construct kinds from one document.

- ``integrated/apb/apb_if.sv`` — an interface with include-guard macros,
  three clocking blocks, and sequence declarations inside clocking blocks.
  Exercises clocking-block parsing and include-guard handling.

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_interfaces -v
"""

from __future__ import annotations

import time
import pathlib
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
IFACE_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "simple" / "interfaces"
IFACE_SV = IFACE_DIR / "interface.sv"

APB_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "integrated" / "apb"
APB_IF_SV = APB_DIR / "apb_if.sv"


# ---------------------------------------------------------------------------
# InterfaceFileTest — interfaces/interface.sv
# ---------------------------------------------------------------------------


class InterfaceFileTest(unittest.TestCase):
    """Tests against ``interface.sv``: a single file that mixes an
    interface declaration, two packages (with class bodies inside), and
    three module declarations."""

    @classmethod
    def setUpClass(cls) -> None:
        if not IFACE_SV.exists():
            raise unittest.SkipTest(f"example not found: {IFACE_SV}")
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=IFACE_DIR)
        cls.uri = file_uri(IFACE_SV)
        cls.text = read_text(IFACE_SV)
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

    def test_no_crash_on_interface_file(self) -> None:
        """Smoke: every standard LSP feature must respond without raising."""
        td = {"textDocument": {"uri": self.uri}}
        # cursor on `pin_if` interface name (line 36, char 12)
        td_pos = {
            "textDocument": {"uri": self.uri},
            "position": {"line": 36, "character": 12},
        }
        self.lsp.request("textDocument/foldingRange", td)
        self.lsp.request("textDocument/documentSymbol", td)
        self.lsp.request("textDocument/documentHighlight", td_pos)
        self.lsp.request("textDocument/definition", td_pos)
        self.lsp.request("textDocument/signatureHelp", td_pos)
        self.lsp.request(
            "textDocument/inlayHint",
            {**td, "range": _range(0, 0, 191, 0)},
        )

    # ------------------------------------------------------------------
    # documentSymbol — top-level construct kinds
    # ------------------------------------------------------------------

    def test_document_symbol_finds_interface(self) -> None:
        """`pin_if` interface declaration at line 36 (0-indexed) is indexed."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(symbols, list)
        names = _collect_symbol_names(symbols)
        self.assertIn("pin_if", names)

    def test_document_symbol_finds_packages(self) -> None:
        """Both ``top_pkg`` and ``user_pkg`` are indexed as Package symbols."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for pkg in ("top_pkg", "user_pkg"):
            self.assertIn(pkg, names, f"missing package `{pkg}` in {sorted(names)}")

    def test_document_symbol_finds_modules(self) -> None:
        """All three modules — ``dut``, ``clkgen``, ``top`` — are indexed."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for mod in ("dut", "clkgen", "top"):
            self.assertIn(mod, names, f"missing module `{mod}` in {sorted(names)}")

    def test_document_symbol_finds_classes_in_package(self) -> None:
        """``driver`` and ``env`` are declared inside ``user_pkg`` — the
        indexer must descend into packages to find them."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for cls_name in ("driver", "env"):
            self.assertIn(
                cls_name, names, f"missing class `{cls_name}` in {sorted(names)}"
            )

    # ------------------------------------------------------------------
    # foldingRange
    # ------------------------------------------------------------------

    def test_folding_interface_has_range(self) -> None:
        """``interface pin_if`` starts on line 36 (0-indexed) and must fold."""
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(folds, list)
        self.assertGreater(len(folds), 0, "no folds returned for multi-construct file")
        starts = {f["startLine"] for f in folds}
        self.assertIn(
            36, starts, f"missing fold for `pin_if` interface; got {sorted(starts)}"
        )


# ---------------------------------------------------------------------------
# ApbIfTest — integrated/apb/apb_if.sv
# ---------------------------------------------------------------------------


class ApbIfTest(unittest.TestCase):
    """Tests against ``apb_if.sv``: an interface that uses include guards,
    three clocking blocks, and sequence declarations inside clocking blocks."""

    @classmethod
    def setUpClass(cls) -> None:
        if not APB_IF_SV.exists():
            raise unittest.SkipTest(f"example not found: {APB_IF_SV}")
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=APB_DIR)
        cls.uri = file_uri(APB_IF_SV)
        cls.text = read_text(APB_IF_SV)
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

    def test_no_crash_on_apb_if(self) -> None:
        """Smoke: every standard LSP feature must respond without raising.
        The include guard (``ifndef``/``define``/``endif``) and sequences
        inside clocking blocks must not cause a handler panic."""
        td = {"textDocument": {"uri": self.uri}}
        # cursor on `apb_if` interface name (line 29, char 12)
        td_pos = {
            "textDocument": {"uri": self.uri},
            "position": {"line": 29, "character": 12},
        }
        self.lsp.request("textDocument/foldingRange", td)
        self.lsp.request("textDocument/documentSymbol", td)
        self.lsp.request("textDocument/documentHighlight", td_pos)
        self.lsp.request("textDocument/definition", td_pos)
        self.lsp.request("textDocument/signatureHelp", td_pos)
        self.lsp.request(
            "textDocument/inlayHint",
            {**td, "range": _range(0, 0, 67, 0)},
        )

    # ------------------------------------------------------------------
    # documentSymbol
    # ------------------------------------------------------------------

    def test_document_symbol_finds_apb_if_interface(self) -> None:
        """``apb_if`` interface is indexed even though the file body is
        wrapped in an include guard."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(symbols, list)
        names = _collect_symbol_names(symbols)
        self.assertIn("apb_if", names, f"missing `apb_if` in {sorted(names)}")

    # ------------------------------------------------------------------
    # foldingRange
    # ------------------------------------------------------------------

    def test_folding_apb_if_has_range(self) -> None:
        """``interface apb_if`` starts on line 29 (0-indexed) and must fold."""
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(folds, list)
        self.assertGreater(len(folds), 0, "no folds returned for apb_if.sv")
        starts = {f["startLine"] for f in folds}
        self.assertIn(
            29, starts, f"missing fold for `apb_if` interface; got {sorted(starts)}"
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
