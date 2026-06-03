"""Integration test for `textDocument/codeLens` override lenses.

Drives `mimir-server` over stdio and verifies the "overrides Base::method"
CodeLens fires for a UVM phase override (default `[code_lens] overrides =
"uvm"`). Hermetic — no example repo, no slang sidecar (CodeLens is
tree-sitter only).
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri


# derived.build_phase overrides base.build_phase -> one lens, pointing at
# base's declaration (line 1, 0-indexed). build_phase is a UVM phase, so it
# qualifies under the default "uvm" scope.
_FIXTURE_SV = """\
class base extends uvm_component;
  function void build_phase(uvm_phase phase);
    super.build_phase(phase);
  endfunction
endclass
class derived extends base;
  function void build_phase(uvm_phase phase);
    super.build_phase(phase);
  endfunction
endclass
"""


class CodeLensTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmpdir = tempfile.TemporaryDirectory()
        sv_path = pathlib.Path(cls._tmpdir.name) / "comp.sv"
        sv_path.write_text(_FIXTURE_SV)

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=pathlib.Path(cls._tmpdir.name))
        cls.uri = file_uri(sv_path)
        cls.lsp.did_open(cls.uri, _FIXTURE_SV)
        # Wait for the parse so the workspace index is populated before the
        # codeLens request (the handler reads it for override resolution).
        cls.lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmpdir.cleanup()

    def test_override_lens_for_phase_method(self) -> None:
        lenses = self.lsp.request(
            "textDocument/codeLens",
            {"textDocument": {"uri": self.uri}},
        )
        self.assertIsInstance(lenses, list)
        # Default "uvm" scope: only the derived build_phase override (base's
        # build_phase has no ancestor declaring it).
        override = [
            l for l in lenses
            if l.get("command", {}).get("title", "").startswith("▷ overrides")
        ]
        self.assertEqual(len(override), 1, f"expected one override lens, got {lenses!r}")

        lens = override[0]
        cmd = lens["command"]
        self.assertIn("overrides base::build_phase", cmd["title"])
        self.assertEqual(cmd["command"], "mimir.gotoLocation")
        # The lens sits on derived.build_phase (line 6, 0-indexed).
        self.assertEqual(lens["range"]["start"]["line"], 6)
        # Its target is base.build_phase (line 1, 0-indexed).
        args = cmd["arguments"]
        self.assertEqual(args[1]["line"], 1)


if __name__ == "__main__":
    unittest.main()
