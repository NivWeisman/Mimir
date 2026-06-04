"""Integration test: completion responses are marked `isIncomplete`.

The bug this guards against: completion only fired the instant a trigger
character (`.`/`::`) was typed. Because responses were sent as a bare array
(`isIncomplete: false`), VS Code cached them and stopped re-querying the
server, so editing back into a member prefix (`obj.a_some` -> backspace ->
`obj.a_som`) never re-popped the list.

The fix marks every completion response `isIncomplete: true`, so the client
re-queries on each edit. Here we assert (a) the response carries
`isIncomplete: true`, and (b) a request at a *mid-member* position (what a
re-query after deletion looks like) still returns the receiver's members.
Hermetic — no slang; member type is resolved via tree-sitter.
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri


_FIXTURE_SV = """\
class packet;
  int data;
  int addr;
endclass
class env;
  packet pkt;
  function void go();
    pkt.data = 0;
  endfunction
endclass
"""


class CompletionRetriggerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmpdir = tempfile.TemporaryDirectory()
        sv_path = pathlib.Path(cls._tmpdir.name) / "env.sv"
        sv_path.write_text(_FIXTURE_SV)

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=pathlib.Path(cls._tmpdir.name))
        cls.uri = file_uri(sv_path)
        cls.lsp.did_open(cls.uri, _FIXTURE_SV)
        cls.lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmpdir.cleanup()

    def _complete(self, line: int, character: int):
        return self.lsp.request(
            "textDocument/completion",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": line, "character": character},
            },
        )

    def _assert_incomplete(self, result) -> list[str]:
        self.assertIsNotNone(result)
        # A CompletionList serializes as an object with isIncomplete; a bare
        # array would be the old (broken) shape that stops the client
        # re-querying.
        self.assertIsInstance(
            result, dict,
            f"expected a CompletionList object, got {type(result).__name__}: {result!r}",
        )
        self.assertTrue(
            result.get("isIncomplete"),
            f"completion must be isIncomplete:true so the client re-queries; got {result!r}",
        )
        return [i["label"] for i in result.get("items", [])]

    def test_member_completion_at_dot_lists_all_members(self) -> None:
        # Line 7 is "    pkt.data = 0;" — cursor right after the dot
        # (character 8): empty prefix, so every member is offered.
        labels = self._assert_incomplete(self._complete(7, 8))
        self.assertIn("data", labels, f"member 'data' missing: {labels}")
        self.assertIn("addr", labels, f"member 'addr' missing: {labels}")

    def test_member_completion_mid_prefix_requery(self) -> None:
        # Cursor after "pkt.da" (character 10) — what a re-query looks like
        # after typing then deleting a char. The matching member is still
        # returned (filtered to the "da" prefix), proving completion stays
        # live mid-member rather than only firing on the dot keystroke.
        labels = self._assert_incomplete(self._complete(7, 10))
        self.assertIn("data", labels, f"member 'data' missing on re-query: {labels}")


if __name__ == "__main__":
    unittest.main()
