"""Hermetic tests for the SVA property/sequence index + hover expansion.

Drives the real ``mimir-server`` over LSP/stdio with a throwaway temp-dir
workspace (no slang, no examples/): a module declaring one ``sequence``
and one ``property`` that are then referenced from ``assert property``,
``cover property``, and from inside the property expression itself.

Pins both halves of the feature:

* **index** — ``textDocument/documentSymbol`` lists the property and the
  sequence, and ``textDocument/definition`` on a use jumps to the
  declaration.
* **hover-preview of expansion** — hovering a property/sequence *use*
  (or its declaration name) shows the whole ``property … endproperty`` /
  ``sequence … endsequence`` body, not just the name line.

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_sva_hover -v
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri

SVA_SV = """\
module m(input logic clk, rst_n, req, ack);
  sequence s_handshake;
    req ##[1:3] ack;
  endsequence

  property p_req_ack;
    @(posedge clk) disable iff (!rst_n)
      req |-> s_handshake;
  endproperty

  assert property (p_req_ack) else $error("req/ack broke");
  cover property (p_req_ack);
endmodule
"""


def _pos_of(needle: str, occurrence: int = 0) -> tuple[int, int]:
    """(line, character) of the given occurrence of ``needle``; the
    character points 2 columns into the token so the cursor is inside it."""
    seen = 0
    for i, line in enumerate(SVA_SV.splitlines()):
        col = 0
        while (col := line.find(needle, col)) != -1:
            if seen == occurrence:
                return i, col + 2
            seen += 1
            col += len(needle)
    raise AssertionError(f"fixture lacks occurrence {occurrence} of {needle!r}")


class SvaHoverTest(unittest.TestCase):
    """One server, one SVA module, hover + symbols + definition."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        src = root / "sva.sv"
        src.write_text(SVA_SV)
        cls.uri = file_uri(src)

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=root)
        cls.lsp.did_open(cls.uri, SVA_SV)
        diag = cls.lsp.wait_for_notification(
            "textDocument/publishDiagnostics", timeout=5.0
        )
        assert diag is not None, "server never published initial diagnostics"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    # ------------------------------------------------------------------
    # helpers
    # ------------------------------------------------------------------

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

    # ------------------------------------------------------------------
    # hover-preview of expansion
    # ------------------------------------------------------------------

    def test_hover_on_assert_use_shows_property_body(self) -> None:
        """`assert property (p_req_ack)` → the full property block."""
        line, char = _pos_of("p_req_ack", occurrence=1)  # 0 is the decl
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on property use in assert")
        self.assertIn("property p_req_ack;", value)
        self.assertIn("req |-> s_handshake;", value)
        self.assertIn("endproperty", value)

    def test_hover_on_cover_use_shows_property_body(self) -> None:
        line, char = _pos_of("p_req_ack", occurrence=2)
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on property use in cover")
        self.assertIn("endproperty", value)

    def test_hover_on_declaration_name_shows_body_too(self) -> None:
        line, char = _pos_of("p_req_ack", occurrence=0)
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on property declaration name")
        self.assertIn("req |-> s_handshake;", value)

    def test_hover_on_sequence_use_inside_property_shows_sequence_body(self) -> None:
        """The `s_handshake` reference inside the property expression
        expands to the sequence body."""
        line, char = _pos_of("s_handshake", occurrence=1)  # 0 is the decl
        value = self._hover_value(line, char)
        self.assertIsNotNone(value, "no hover on sequence use")
        self.assertIn("sequence s_handshake;", value)
        self.assertIn("req ##[1:3] ack;", value)
        self.assertIn("endsequence", value)

    # ------------------------------------------------------------------
    # the index half
    # ------------------------------------------------------------------

    def test_document_symbols_include_property_and_sequence(self) -> None:
        result = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsNotNone(result)

        def names(nodes) -> set[str]:
            out = set()
            for n in nodes:
                out.add(n["name"])
                out |= names(n.get("children") or [])
            return out

        got = names(result)
        self.assertIn("p_req_ack", got)
        self.assertIn("s_handshake", got)

    def test_definition_on_use_jumps_to_declaration(self) -> None:
        use_line, use_char = _pos_of("p_req_ack", occurrence=1)
        decl_line, _ = _pos_of("p_req_ack", occurrence=0)
        result = self.lsp.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": use_line, "character": use_char},
            },
        )
        self.assertTrue(result, "no definition for property use")
        loc = result[0] if isinstance(result, list) else result
        self.assertEqual(loc["uri"], self.uri)
        self.assertEqual(loc["range"]["start"]["line"], decl_line)


if __name__ == "__main__":
    unittest.main()
