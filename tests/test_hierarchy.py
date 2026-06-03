"""Integration tests for ``callHierarchy/*`` and ``typeHierarchy/*`` LSP features.

Uses a self-contained fixture written to a temporary directory so the tests
are hermetic and do not depend on UVM library paths.

Call hierarchy works in tree-sitter-only mode.  Type hierarchy uses the
tree-sitter workspace index for supertypes/subtypes (slang path is a bonus).

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_hierarchy -v

Fixture layout (0-indexed line numbers):

    0: class base;
    1:   function int compute(int x);
    2:     return x + 1;
    3:   endfunction
    4: endclass
    5: (blank)
    6: class derived extends base;
    7:   function int run(int y);
    8:     int r = compute(y);      <- expression-context call; tree-sitter tf_call
    9:     return r;
   10:   endfunction
   11: endclass
   12: (blank)
   13: class derived2 extends base;
   14:   function void noop();
   15:   endfunction
   16: endclass
"""

from __future__ import annotations

import pathlib
import tempfile
import time
import unittest

from .lsp_client import MimirLspClient, file_uri


# ---------------------------------------------------------------------------
# Shared fixture
# ---------------------------------------------------------------------------

_FIXTURE_SV = """\
class base;
  function int compute(int x);
    return x + 1;
  endfunction
endclass

class derived extends base;
  function int run(int y);
    int r = compute(y);
    return r;
  endfunction
endclass

class derived2 extends base;
  function void noop();
  endfunction
endclass
"""


def _pos(line: int, char: int) -> dict:
    return {"line": line, "character": char}


# ---------------------------------------------------------------------------
# Call hierarchy
# ---------------------------------------------------------------------------


class CallHierarchyTest(unittest.TestCase):
    """``callHierarchy/*`` against a small synthetic SV fixture.

    Exercises all three methods of the two-phase call hierarchy protocol:
    ``textDocument/prepareCallHierarchy``, ``callHierarchy/incomingCalls``,
    and ``callHierarchy/outgoingCalls``.
    """

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmpdir = tempfile.TemporaryDirectory()
        sv_path = pathlib.Path(cls._tmpdir.name) / "fixture.sv"
        sv_path.write_text(_FIXTURE_SV)

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=pathlib.Path(cls._tmpdir.name))
        cls.uri = file_uri(sv_path)
        cls.lsp.did_open(cls.uri, _FIXTURE_SV)
        # Wait for initial indexing to complete.
        cls.lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)
        time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmpdir.cleanup()

    # --- prepareCallHierarchy ---

    def test_prepare_on_function_name_returns_item(self) -> None:
        """Cursor on 'run' (line 7, char 15) resolves to a ``CallHierarchyItem``."""
        result = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(7, 15),  # 'r' in 'run'
            },
        )
        self.assertIsInstance(result, list)
        self.assertTrue(
            len(result) >= 1, f"expected at least one item, got {result!r}"
        )
        self.assertEqual(result[0]["name"], "run")

    def test_prepare_on_callee_name_returns_item(self) -> None:
        """Cursor on 'compute' (line 1, char 15) resolves to a ``CallHierarchyItem``."""
        result = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(1, 15),  # 'c' in 'compute'
            },
        )
        self.assertIsInstance(result, list)
        self.assertTrue(
            len(result) >= 1, f"expected at least one item, got {result!r}"
        )
        self.assertEqual(result[0]["name"], "compute")

    def test_prepare_on_blank_line_returns_null(self) -> None:
        """Cursor on a blank line returns ``null`` (no callable under cursor)."""
        result = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(5, 0),  # blank line between the two classes
            },
        )
        # tower-lsp Ok(None) → JSON null → Python None; Ok(Some([])) → [].
        # Both are falsy and both are valid "not found" answers.
        self.assertFalse(
            result, f"expected null/empty for non-callable position, got {result!r}"
        )

    # --- callHierarchy/outgoingCalls ---

    def test_outgoing_calls_finds_callee(self) -> None:
        """``outgoingCalls`` from 'run' lists 'compute' (called in expr context)."""
        prepare = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(7, 15),
            },
        )
        self.assertIsInstance(prepare, list)
        self.assertTrue(len(prepare) >= 1, f"prepare returned {prepare!r}")
        item = prepare[0]

        result = self.lsp.request("callHierarchy/outgoingCalls", {"item": item})
        self.assertIsInstance(result, list)
        callee_names = [c["to"]["name"] for c in result]
        self.assertIn(
            "compute",
            callee_names,
            f"expected 'compute' in outgoing calls, got {callee_names}",
        )

    def test_outgoing_calls_includes_from_ranges(self) -> None:
        """Each outgoing call entry carries at least one ``fromRanges`` entry."""
        prepare = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(7, 15),
            },
        )
        item = prepare[0] if prepare else None
        self.assertIsNotNone(item)

        result = self.lsp.request("callHierarchy/outgoingCalls", {"item": item})
        for call in result:
            self.assertIn(
                "fromRanges",
                call,
                f"missing fromRanges key in {call!r}",
            )
            self.assertTrue(
                len(call["fromRanges"]) >= 1,
                f"fromRanges is empty in {call!r}",
            )

    # --- callHierarchy/incomingCalls ---

    def test_incoming_calls_finds_caller(self) -> None:
        """``incomingCalls`` for 'compute' lists 'run' as the enclosing caller."""
        prepare = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(1, 15),  # 'c' in 'compute'
            },
        )
        self.assertIsInstance(prepare, list)
        self.assertTrue(len(prepare) >= 1, f"prepare returned {prepare!r}")
        item = prepare[0]

        result = self.lsp.request("callHierarchy/incomingCalls", {"item": item})
        self.assertIsInstance(result, list)
        caller_names = [c["from"]["name"] for c in result]
        self.assertIn(
            "run",
            caller_names,
            f"expected 'run' in incoming calls, got {caller_names}",
        )

    def test_incoming_calls_includes_from_ranges(self) -> None:
        """Each incoming call carries at least one call-site range."""
        prepare = self.lsp.request(
            "textDocument/prepareCallHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(1, 15),
            },
        )
        item = prepare[0] if prepare else None
        self.assertIsNotNone(item)

        result = self.lsp.request("callHierarchy/incomingCalls", {"item": item})
        for call in result:
            self.assertIn("fromRanges", call, f"missing fromRanges in {call!r}")
            self.assertTrue(len(call["fromRanges"]) >= 1)


# ---------------------------------------------------------------------------
# Type hierarchy
# ---------------------------------------------------------------------------


class TypeHierarchyTest(unittest.TestCase):
    """``typeHierarchy/*`` against the same synthetic SV fixture.

    Exercises all three methods: ``textDocument/prepareTypeHierarchy``,
    ``typeHierarchy/supertypes``, and ``typeHierarchy/subtypes``.
    """

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmpdir = tempfile.TemporaryDirectory()
        sv_path = pathlib.Path(cls._tmpdir.name) / "fixture.sv"
        sv_path.write_text(_FIXTURE_SV)

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=pathlib.Path(cls._tmpdir.name))
        cls.uri = file_uri(sv_path)
        cls.lsp.did_open(cls.uri, _FIXTURE_SV)
        cls.lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)
        time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmpdir.cleanup()

    # --- prepareTypeHierarchy ---

    def test_prepare_on_class_name_returns_item(self) -> None:
        """Cursor on 'derived' (line 6, char 6) resolves to a ``TypeHierarchyItem``."""
        result = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(6, 6),  # 'd' in 'derived'
            },
        )
        self.assertIsInstance(result, list)
        self.assertTrue(
            len(result) >= 1, f"expected at least one item, got {result!r}"
        )
        self.assertEqual(result[0]["name"], "derived")

    def test_prepare_on_root_class_returns_item(self) -> None:
        """Cursor on 'base' (line 0, char 6) resolves correctly."""
        result = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(0, 6),  # 'b' in 'base'
            },
        )
        self.assertIsInstance(result, list)
        self.assertTrue(len(result) >= 1, f"got {result!r}")
        self.assertEqual(result[0]["name"], "base")

    def test_prepare_on_blank_line_returns_null(self) -> None:
        """Cursor on a blank line returns ``null`` (no class under cursor)."""
        result = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(5, 0),
            },
        )
        self.assertFalse(
            result, f"expected null/empty for non-class position, got {result!r}"
        )

    # --- typeHierarchy/supertypes ---

    def test_supertypes_finds_parent_class(self) -> None:
        """``supertypes`` for 'derived' returns 'base'."""
        prepare = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(6, 6),
            },
        )
        self.assertIsInstance(prepare, list)
        self.assertTrue(len(prepare) >= 1)
        item = prepare[0]

        result = self.lsp.request("typeHierarchy/supertypes", {"item": item})
        self.assertIsInstance(result, list)
        parent_names = [i["name"] for i in result]
        self.assertIn(
            "base",
            parent_names,
            f"expected 'base' in supertypes, got {parent_names}",
        )

    def test_supertypes_for_root_class_is_empty(self) -> None:
        """``supertypes`` for 'base' is empty — no parent in this workspace."""
        prepare = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(0, 6),
            },
        )
        self.assertIsInstance(prepare, list)
        self.assertTrue(len(prepare) >= 1)
        item = prepare[0]

        result = self.lsp.request("typeHierarchy/supertypes", {"item": item})
        self.assertIsInstance(result, list)
        self.assertEqual(
            result,
            [],
            f"expected empty supertypes for root class 'base', got {result}",
        )

    # --- typeHierarchy/subtypes ---

    def test_subtypes_finds_both_subclasses(self) -> None:
        """``subtypes`` for 'base' lists both 'derived' and 'derived2'."""
        prepare = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(0, 6),  # 'b' in 'base'
            },
        )
        self.assertIsInstance(prepare, list)
        self.assertTrue(len(prepare) >= 1)
        item = prepare[0]

        result = self.lsp.request("typeHierarchy/subtypes", {"item": item})
        self.assertIsInstance(result, list)
        child_names = [i["name"] for i in result]
        self.assertIn(
            "derived",
            child_names,
            f"expected 'derived' in subtypes, got {child_names}",
        )
        self.assertIn(
            "derived2",
            child_names,
            f"expected 'derived2' in subtypes, got {child_names}",
        )

    def test_subtypes_for_leaf_class_is_empty(self) -> None:
        """``subtypes`` for 'derived' is empty — nothing extends it."""
        prepare = self.lsp.request(
            "textDocument/prepareTypeHierarchy",
            {
                "textDocument": {"uri": self.uri},
                "position": _pos(6, 6),
            },
        )
        self.assertIsInstance(prepare, list)
        self.assertTrue(len(prepare) >= 1)
        item = prepare[0]

        result = self.lsp.request("typeHierarchy/subtypes", {"item": item})
        self.assertIsInstance(result, list)
        self.assertEqual(
            result,
            [],
            f"expected empty subtypes for leaf class 'derived', got {result}",
        )

    # --- capability advertisement ---

    def test_capability_registered_dynamically(self) -> None:
        """The server advertises type hierarchy via
        ``client/registerCapability`` (lsp-types 0.94.1 has no static
        ``type_hierarchy_provider`` field, so dynamic registration is what
        makes VS Code show the "Show Type Hierarchy" entry).

        The registration is sent during ``initialized`` — captured by the
        client during the setup handshake/diagnostics pump or pumped here.
        """
        found = self.lsp.wait_for_registration(
            "textDocument/prepareTypeHierarchy", timeout=3.0
        )
        self.assertTrue(
            found,
            "server never registered textDocument/prepareTypeHierarchy via "
            "client/registerCapability",
        )


if __name__ == "__main__":
    unittest.main()
