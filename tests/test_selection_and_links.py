"""Hermetic tests for `textDocument/selectionRange` and
`textDocument/documentLink`.

Both are pure tree-sitter features — no slang sidecar required — so these run
on any checkout. A throwaway project is built in a tempdir so the document-
link resolution has real files (and a real include dir) to point at.

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_selection_and_links -v
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri


_TOP = """\
`include "defs.svh"
module m;
  initial begin
    x = a + b;
  end
endmodule
"""
_DEFS = "`define WIDTH 8\n"


class SelectionAndLinksTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        (root / "inc").mkdir()
        cls._top = root / "top.sv"
        cls._top.write_text(_TOP)
        # The included header lives in an include dir, not next to top.sv,
        # so resolution must consult the project include_dirs.
        (root / "inc" / "defs.svh").write_text(_DEFS)
        (root / "files.f").write_text("top.sv\n")
        (root / ".mimir.toml").write_text(
            "[slang]\nfilelist = \"files.f\"\ninclude_dirs = [\"inc\"]\n"
        )

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=root)
        cls.uri = file_uri(cls._top)
        cls.lsp.did_open(cls.uri, _TOP)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    # ---- selectionRange ------------------------------------------------

    def test_selection_range_nests_outward(self) -> None:
        # Cursor on `a` in `    x = a + b;` (line 3): 4 spaces, x=4, '='=6, a=8.
        result = self.lsp.request(
            "textDocument/selectionRange",
            {
                "textDocument": {"uri": self.uri},
                "positions": [{"line": 3, "character": 8}],
            },
        )
        self.assertIsInstance(result, list)
        self.assertEqual(len(result), 1, "one SelectionRange per position")

        # Walk the parent chain; each parent must contain its child and the
        # chain must reach the module (line 1) at the top.
        node = result[0]
        depth = 0
        outermost = node
        while node is not None:
            r = node["range"]
            parent = node.get("parent")
            if parent is not None:
                pr = parent["range"]
                # parent contains child
                self.assertLessEqual(
                    (pr["start"]["line"], pr["start"]["character"]),
                    (r["start"]["line"], r["start"]["character"]),
                )
                self.assertGreaterEqual(
                    (pr["end"]["line"], pr["end"]["character"]),
                    (r["end"]["line"], r["end"]["character"]),
                )
            outermost = node
            node = parent
            depth += 1
        self.assertGreaterEqual(depth, 2, "expected a real nesting chain")
        # Outermost should span from the module/file start (line 0 or 1).
        self.assertLessEqual(outermost["range"]["start"]["line"], 1)

    # ---- documentLink --------------------------------------------------

    def test_document_link_for_include(self) -> None:
        links = self.lsp.request(
            "textDocument/documentLink",
            {"textDocument": {"uri": self.uri}},
        )
        self.assertIsInstance(links, list)
        self.assertEqual(len(links), 1, f"expected one include link, got {links}")
        link = links[0]

        # The link target resolves to inc/defs.svh.
        self.assertTrue(link["target"].endswith("inc/defs.svh"), link["target"])

        # The range underlines just the filename on line 0.
        rng = link["range"]
        self.assertEqual(rng["start"]["line"], 0)
        # `include "defs.svh" — the filename starts after `include "` (10 chars).
        self.assertEqual(rng["start"]["character"], 10)
        self.assertEqual(rng["end"]["character"], 10 + len("defs.svh"))
