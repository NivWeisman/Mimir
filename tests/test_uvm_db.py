"""Hermetic integration tests for the custom ``mimir/uvmDb`` request.

These drive the real ``mimir-server`` over LSP/stdio exactly like the VS
Code tree view does: initialize against a throwaway workspace with a
``.mimir.toml``, wait for the eager tree-sitter hydration pass, then ask
for the workspace-wide ``uvm_config_db`` / ``uvm_resource_db`` call
listing. No slang, no examples/ checkout — the scan is purely syntactic,
so the fixtures are two tiny UVM-shaped files written to a temp dir.

The two-file split is deliberate: ``dv_base_test.sv`` is *opened* via
``didOpen`` (exercising the open-document tree path) while ``dv_env.sv``
is only ever referenced by the filelist (exercising the closed-file
hydrated-tree path). Test 1 pins the union of the two AND the dedupe of
a file that is both open and hydrated.

Run:

    cargo build --release -p mimir-server   # ensure binary is current
    python3 -m unittest tests.test_uvm_db -v
"""

from __future__ import annotations

import pathlib
import tempfile
import unittest

from .lsp_client import MimirLspClient, file_uri

# Writers live here; this file is opened in the editor.
DV_BASE_TEST_SV = """\
class dv_base_test;
  function void build_phase();
    uvm_config_db#(int)::set(this, "env.agent", "cfg", 42);
    uvm_config_db#(int)::set(this, "env.*", "cfg", 7);
    uvm_resource_db#(bit)::set("regmodel", "en", 1, this);
  endfunction
endclass
"""

# Readers live here; this file is NEVER opened — the server only knows it
# through the .mimir.toml filelist hydration.
DV_ENV_SV = """\
class dv_env;
  int cfg;
  string mode;
  bit en;
  function void build_phase();
    if (!uvm_config_db#(int)::get(this, "", "cfg", cfg)) begin
    end
    void'(uvm_config_db#(string)::get(this, "", "mode", mode));
    void'(uvm_resource_db#(bit)::read_by_name("regmodel", "en", en));
  endfunction
endclass
"""

MIMIR_TOML = """\
[slang]
files = ["dv_base_test.sv", "dv_env.sv"]
"""

# The hydration pass logs this on completion (backend.rs); waiting on it
# guarantees the closed file's tree is in the workspace cache before we
# fire the first request.
HYDRATED_MARKER = "workspace index hydrated"


def _line_of(text: str, needle: str) -> int:
    """0-based line index of the first line containing ``needle``."""
    for i, line in enumerate(text.splitlines()):
        if needle in line:
            return i
    raise AssertionError(f"fixture does not contain {needle!r}")


class UvmDbViewerTest(unittest.TestCase):
    """One server, one open writer file, one closed reader file."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        (root / ".mimir.toml").write_text(MIMIR_TOML)
        base = root / "dv_base_test.sv"
        base.write_text(DV_BASE_TEST_SV)
        env = root / "dv_env.sv"
        env.write_text(DV_ENV_SV)
        cls.base_uri = file_uri(base)
        cls.env_uri = file_uri(env)

        # log_to_stderr gives us RUST_LOG so wait_for_log can see the
        # hydration marker.
        cls.lsp = MimirLspClient(log_to_stderr=True)
        cls.lsp.initialize(workspace_root=root)
        assert cls.lsp.wait_for_log(HYDRATED_MARKER, timeout=15.0), (
            "server never logged workspace-index hydration; stderr:\n"
            + cls.lsp.stderr_text
        )
        cls.lsp.did_open(cls.base_uri, DV_BASE_TEST_SV)
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

    def _uvm_db(self, params: object = None) -> dict:
        result = self.lsp.request("mimir/uvmDb", params if params is not None else {})
        self.assertIsInstance(result, dict, f"unexpected result: {result!r}")
        return result

    def _group(self, resp: dict, db: str, key: str) -> dict:
        for g in resp["groups"]:
            if g["db"] == db and g["key"] == key:
                return g
        self.fail(f"no ({db!r}, {key!r}) group in {resp['groups']!r}")

    # ------------------------------------------------------------------
    # tests
    # ------------------------------------------------------------------

    def test_open_and_closed_files_union_without_double_counting(self) -> None:
        """The "cfg" group pairs the open file's 2 sets with the closed
        file's 1 get — and the open file (which is also filelist-hydrated)
        is not scanned twice."""
        resp = self._uvm_db()
        g = self._group(resp, "config", "cfg")
        self.assertEqual(len(g["writers"]), 2, g["writers"])
        self.assertEqual(len(g["readers"]), 1, g["readers"])
        self.assertFalse(g["unwrittenRead"])
        self.assertFalse(g["typeMismatch"])
        self.assertEqual(g["typeParams"], ["int"])
        # Writers come from the open file; the reader from the closed one.
        for w in g["writers"]:
            self.assertEqual(w["uri"], self.base_uri)
        self.assertEqual(g["readers"][0]["uri"], self.env_uri)

    def test_get_without_set_is_flagged(self) -> None:
        """"mode" is only ever read — the group must carry unwrittenRead."""
        resp = self._uvm_db()
        g = self._group(resp, "config", "mode")
        self.assertEqual(len(g["writers"]), 0)
        self.assertEqual(len(g["readers"]), 1)
        self.assertTrue(g["unwrittenRead"])

    def test_resource_db_groups_separately(self) -> None:
        """resource_db "en" has its own group, distinct from config groups."""
        resp = self._uvm_db()
        g = self._group(resp, "resource", "en")
        self.assertEqual(len(g["writers"]), 1)
        self.assertEqual(len(g["readers"]), 1)
        self.assertEqual(g["writers"][0]["method"], "set")
        self.assertEqual(g["readers"][0]["method"], "read_by_name")
        self.assertEqual(g["writers"][0]["scope"], "regmodel")

    def test_entry_ranges_point_at_the_fixture_lines(self) -> None:
        """Jump targets are real: the reader's range sits on the line the
        fixture wrote the call on, and fileLine matches (1-based)."""
        resp = self._uvm_db()
        g = self._group(resp, "config", "cfg")
        reader = g["readers"][0]
        expected_line = _line_of(DV_ENV_SV, '"cfg"')
        self.assertEqual(reader["range"]["start"]["line"], expected_line)
        self.assertEqual(reader["fileLine"], f"dv_env.sv:{expected_line + 1}")
        # The label carries the method + inst_name qualifier.
        self.assertEqual(reader["label"], 'get  ""')

    def test_null_params_are_accepted(self) -> None:
        """Clients that send `"params": null` (or omit params) must not get
        an InvalidParams error — pins the Option<UvmDbParams> boundary."""
        result = self.lsp.request("mimir/uvmDb", None)
        self.assertIsInstance(result, dict)
        self.assertIn("groups", result)

    def test_nothing_truncated_and_nothing_ungrouped(self) -> None:
        """The tiny fixture stays under every cap and every key is a
        literal, so both escape hatches must be empty/false."""
        resp = self._uvm_db()
        self.assertFalse(resp["truncated"])
        self.assertEqual(resp["ungrouped"], [])


class UvmDbNoOpenDocsTest(unittest.TestCase):
    """The viewer must work before the user opens any file: hydrated
    closed-file trees alone feed the scan."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(cls._tmp.name)
        (root / ".mimir.toml").write_text(MIMIR_TOML)
        (root / "dv_base_test.sv").write_text(DV_BASE_TEST_SV)
        (root / "dv_env.sv").write_text(DV_ENV_SV)

        cls.lsp = MimirLspClient(log_to_stderr=True)
        cls.lsp.initialize(workspace_root=root)
        assert cls.lsp.wait_for_log(HYDRATED_MARKER, timeout=15.0), (
            "server never logged workspace-index hydration; stderr:\n"
            + cls.lsp.stderr_text
        )

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()
        cls._tmp.cleanup()

    def test_scan_works_with_zero_open_documents(self) -> None:
        resp = self.lsp.request("mimir/uvmDb", {})
        keys = {(g["db"], g["key"]) for g in resp["groups"]}
        self.assertEqual(
            keys,
            {("config", "cfg"), ("config", "mode"), ("resource", "en")},
        )


if __name__ == "__main__":
    unittest.main()
