"""Integration test for UVM-aware diagnostics (tree-sitter, no slang).

Drives `mimir-server` over stdio and verifies the "UVM phase override
forgets `super.<phase>()`" lint fires end-to-end: parse -> uvm check ->
publishDiagnostics. Hermetic — no example repo, no slang sidecar needed
(the check is purely syntactic and on by default).
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri


# build_phase without super.build_phase(phase) -> should be flagged.
# connect_phase *with* super -> must NOT be flagged.
_FIXTURE_SV = """\
class my_comp extends uvm_component;
  function void build_phase(uvm_phase phase);
    int x = 1;
  endfunction
  function void connect_phase(uvm_phase phase);
    super.connect_phase(phase);
  endfunction
endclass
"""


class UvmLintTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmpdir = tempfile.TemporaryDirectory()
        sv_path = pathlib.Path(cls._tmpdir.name) / "my_comp.sv"
        sv_path.write_text(_FIXTURE_SV)

        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=pathlib.Path(cls._tmpdir.name))
        cls.uri = file_uri(sv_path)
        cls.lsp.did_open(cls.uri, _FIXTURE_SV)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmpdir.cleanup()

    def test_missing_super_phase_is_flagged_once(self) -> None:
        params = self.lsp.wait_for_notification(
            "textDocument/publishDiagnostics", timeout=5.0
        )
        self.assertIsNotNone(params, "server never published diagnostics")
        diags = params.get("diagnostics", [])

        phase_diags = [
            d for d in diags
            if "super.build_phase" in d.get("message", "")
        ]
        self.assertEqual(
            len(phase_diags), 1,
            f"expected exactly one missing-super diagnostic, got {diags!r}",
        )
        # The diagnostic must point at the build_phase override on line 2
        # (0-indexed line 1), not the connect_phase one (which calls super).
        self.assertEqual(phase_diags[0]["range"]["start"]["line"], 1)

    def test_phase_with_super_is_not_flagged(self) -> None:
        params = self.lsp.wait_for_notification(
            "textDocument/publishDiagnostics", timeout=5.0
        )
        self.assertIsNotNone(params)
        diags = params.get("diagnostics", [])
        self.assertFalse(
            any("super.connect_phase" in d.get("message", "") for d in diags),
            f"connect_phase calls super; it must not be flagged: {diags!r}",
        )


if __name__ == "__main__":
    unittest.main()
