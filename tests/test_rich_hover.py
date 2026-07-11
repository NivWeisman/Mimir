"""Hermetic tests for rich hover: doc comments + provenance footer.

Drives the real ``mimir-server`` over LSP/stdio against throwaway temp-dir
workspaces (no slang, no examples/). Rich hover enriches every symbol
hover on the tree-sitter path with (1) the contiguous ``//`` comment block
written immediately above the declaration, rendered as prose below the
code block, and (2) a provenance footer ``*kind · file.sv:line*``.

Two workspaces:

* default (no ``.mimir.toml``) — rich hover is ON by default;
* ``[features] rich_hover = false`` — hovers are the bare declaration,
  byte-compatible with the pre-feature behaviour.

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_rich_hover -v
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri

COUNTER_SV = """\
// A byte-wide free-running counter.
// Wraps to zero on overflow.
module counter(
    input logic clk
);
    // Next value staged for the flop.
    logic [7:0] next_count;
endmodule
"""


def _pos_of(needle: str, occurrence: int = 0) -> tuple[int, int]:
    """(line, character) of an occurrence of ``needle``, 2 cols in."""
    seen = 0
    for i, line in enumerate(COUNTER_SV.splitlines()):
        col = 0
        while (col := line.find(needle, col)) != -1:
            if seen == occurrence:
                return i, col + 2
            seen += 1
            col += len(needle)
    raise AssertionError(f"fixture lacks occurrence {occurrence} of {needle!r}")


class _HoverClientMixin:
    """Shared hover plumbing for both workspace variants."""

    lsp: MimirLspClient
    uri: str

    @classmethod
    def _start(cls, root: pathlib.Path) -> None:
        src = root / "counter.sv"
        src.write_text(COUNTER_SV)
        cls.uri = file_uri(src)
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=root)
        cls.lsp.did_open(cls.uri, COUNTER_SV)
        diag = cls.lsp.wait_for_notification(
            "textDocument/publishDiagnostics", timeout=5.0
        )
        assert diag is not None, "server never published initial diagnostics"

    def _hover_value(self, line: int, character: int) -> str | None:
        result = self.lsp.request(
            "textDocument/hover",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": line, "character": character},
            },
        )
        if not result:
            return None
        contents = result.get("contents")
        if isinstance(contents, dict):
            return contents.get("value")
        return str(contents)


class RichHoverDefaultTest(_HoverClientMixin, unittest.TestCase):
    """No .mimir.toml — rich hover is on by default."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls._start(pathlib.Path(cls._tmp.name))

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def test_module_hover_has_doc_comment_and_footer(self) -> None:
        # Occurrence 0 of "counter" is inside the line-0 comment text;
        # occurrence 1 is the module name on the declaration line.
        line, char = _pos_of("counter", occurrence=1)
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on module name")
        self.assertIn("A byte-wide free-running counter.", value)
        self.assertIn("Wraps to zero on overflow.", value)
        self.assertIn("*module · counter.sv:3*", value)

    def test_variable_hover_has_its_own_doc_comment(self) -> None:
        line, char = _pos_of("next_count")
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on variable")
        self.assertIn("Next value staged for the flop.", value)
        self.assertIn("*variable · counter.sv:7*", value)
        # The module's doc must not bleed into the variable's hover.
        self.assertNotIn("free-running", value)

    def test_uncommented_symbol_gets_footer_only(self) -> None:
        line, char = _pos_of("clk")
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on port")
        self.assertIn("counter.sv:", value)  # footer present
        self.assertNotIn("free-running", value)  # no comment bleed


class RichHoverDisabledTest(_HoverClientMixin, unittest.TestCase):
    """[features] rich_hover = false — bare declaration hovers."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        (root / ".mimir.toml").write_text("[features]\nrich_hover = false\n")
        cls._start(root)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def test_hover_is_bare_declaration(self) -> None:
        line, char = _pos_of("counter", occurrence=1)
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover with rich_hover=false")
        self.assertIn("module counter", value)
        self.assertNotIn("Wraps to zero on overflow.", value)
        self.assertNotIn("counter.sv:3", value)
        self.assertNotIn("·", value)


if __name__ == "__main__":
    unittest.main()
