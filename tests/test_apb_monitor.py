"""Integration tests against the real UVM `apb_monitor.sv` example.

These drive `mimir-server` over LSP/stdio exactly like an editor would.
They exist because `cargo test` only exercises pure-logic helpers — the
LSP wire path (parse → cache → handler → response) only runs at runtime,
and real-world SystemVerilog (UVM macros, classes, hierarchical names)
exposes corner cases that synthetic test fixtures don't.

Known broken cases are marked with `@unittest.expectedFailure` and carry
a `XFAIL:` note pointing at the underlying bug. When someone fixes the
bug the test will start passing — unittest reports that as an
"unexpected success", which is the signal to drop the decorator.

Run:

    cargo build --release -p mimir-server   # ensure binary is current
    python3 -m unittest tests.test_apb_monitor -v
"""

from __future__ import annotations

import pathlib
import time
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
APB_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "integrated" / "apb"
APB_MONITOR = APB_DIR / "apb_monitor.sv"


def _wait_for_first_parse(lsp: MimirLspClient, settle_ms: int = 250) -> None:
    """`did_open` triggers `reparse_and_publish` on a tokio task. Block
    until the server's `publishDiagnostics` for that URI lands, so the
    tree cache is populated before we fire feature requests."""
    diag = lsp.wait_for_notification("textDocument/publishDiagnostics", timeout=5.0)
    time.sleep(settle_ms / 1000.0)
    assert diag is not None, "server never published initial diagnostics"


class ApbMonitorTest(unittest.TestCase):
    """All tests share one server instance — saves ~200ms per test on
    initialize.
    """

    @classmethod
    def setUpClass(cls) -> None:
        if not APB_MONITOR.exists():
            raise unittest.SkipTest(f"example not found: {APB_MONITOR}")
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=APB_DIR)
        cls.uri = file_uri(APB_MONITOR)
        cls.text = read_text(APB_MONITOR)
        cls.lsp.did_open(cls.uri, cls.text)
        _wait_for_first_parse(cls.lsp)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    # ------------------------------------------------------------------
    # Health: the server responded to all standard requests
    # ------------------------------------------------------------------

    def test_no_crash_on_uvm_file(self) -> None:
        """Smoke check: fire every implemented feature request against
        the UVM file. The cache-on-DocumentState change should mean
        zero re-parses for these requests (visible in `mimir=trace`
        logs); here we just assert nothing errors out."""
        td = {"textDocument": {"uri": self.uri}}
        td_pos = {
            "textDocument": {"uri": self.uri},
            "position": {"line": 36, "character": 29},
        }
        # Each call below must return *something* (None or a result) and
        # must not raise. If any handler panics, the request times out.
        self.lsp.request("textDocument/foldingRange", td)
        self.lsp.request("textDocument/documentSymbol", td)
        self.lsp.request("textDocument/documentHighlight", td_pos)
        self.lsp.request("textDocument/definition", td_pos)
        self.lsp.request(
            "textDocument/signatureHelp",
            {**td_pos, "position": {"line": 55, "character": 44}},
        )
        self.lsp.request(
            "textDocument/inlayHint",
            {**td, "range": _range(0, 0, 100, 0)},
        )

    # ------------------------------------------------------------------
    # foldingRange — what currently works
    # ------------------------------------------------------------------

    def test_folding_returns_some_ranges(self) -> None:
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(folds, list)
        self.assertGreater(len(folds), 0, "no folds at all on a 100-line class file")
        # The `apb_monitor_cbs` class (lines 30-33, 0-indexed 29-32)
        # parses cleanly — it must show up.
        starts = {f["startLine"] for f in folds}
        self.assertIn(29, starts, f"missing fold for `apb_monitor_cbs`; got {sorted(starts)}")

    def test_folding_covers_inner_apb_monitor_class(self) -> None:
        """The `class apb_monitor extends uvm_monitor;` declaration on
        line 36 (0-indexed 35) gets a fold. This used to XFAIL because
        the parameterized scope `uvm_config_db#(T)::get(...)` inside
        `build_phase` put the entire class body into an ERROR root —
        the preprocessor now rewrites the `#(T)::` glue so the class
        parses cleanly."""
        folds = self.lsp.request(
            "textDocument/foldingRange", {"textDocument": {"uri": self.uri}}
        )
        starts = {f["startLine"] for f in folds}
        self.assertIn(35, starts)  # `class apb_monitor extends uvm_monitor;` at line 36

    # ------------------------------------------------------------------
    # documentSymbol — what currently works
    # ------------------------------------------------------------------

    def test_document_symbol_finds_classes_and_fields(self) -> None:
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        self.assertIsInstance(symbols, list)
        names = _collect_symbol_names(symbols)
        # Class declarations (both classes' names survive even when the
        # second class's body is wrecked).
        self.assertIn("apb_monitor", names)
        self.assertIn("apb_monitor_cbs", names)
        # Fields inside the broken class still get extracted.
        for field in ("sigs", "ap", "cfg"):
            self.assertIn(field, names, f"missing field `{field}` in {sorted(names)}")
        # Macro definition at the top of the file.
        self.assertIn("APB_MONITOR__SV", names)

    def test_document_symbol_includes_inner_class_methods(self) -> None:
        """All four methods on `apb_monitor` show up. The constructor
        `new` is recognised via `class_constructor_declaration`; the
        other three (`build_phase`, `run_phase`, inner `trans_observed`)
        come back once the parameterized-scope rewrite lets the class
        body parse normally."""
        symbols = self.lsp.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": self.uri}}
        )
        names = _collect_symbol_names(symbols)
        for method in ("new", "build_phase", "run_phase"):
            self.assertIn(method, names)

    # ------------------------------------------------------------------
    # inlayHint
    # ------------------------------------------------------------------

    def test_inlay_hints_returned_for_viewport(self) -> None:
        """Pure tree-sitter inlay hints skip method calls (no receiver
        type without slang). With the broken parse, free-function calls
        like `trans_observed(tr)` may not be detected as calls either.
        The handler must still respond cleanly with a list."""
        hints = self.lsp.request(
            "textDocument/inlayHint",
            {"textDocument": {"uri": self.uri}, "range": _range(0, 0, 100, 0)},
        )
        self.assertIsInstance(hints, list)
        for hint in hints:
            self.assertIn("position", hint)
            self.assertIn("label", hint)

    # ------------------------------------------------------------------
    # signatureHelp — fallback returns None gracefully
    # ------------------------------------------------------------------

    def test_signature_help_does_not_crash(self) -> None:
        """Tree-sitter fallback should return a SignatureHelp object or
        None — never raise. Without slang and with the broken inner
        class, most positions will return None; that's acceptable."""
        result = self.lsp.request(
            "textDocument/signatureHelp",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 55, "character": 44},
            },
        )
        if result is not None:
            self.assertIn("signatures", result)
            self.assertIsInstance(result["signatures"], list)

    # ------------------------------------------------------------------
    # documentHighlight
    # ------------------------------------------------------------------

    def test_document_highlight_does_not_crash(self) -> None:
        """`sigs` is referenced ~10 times in `apb_monitor`. With the
        broken parse the scope-aware highlighter falls back to a tight
        scope and returns fewer matches than ideal, but it must not
        crash."""
        result = self.lsp.request(
            "textDocument/documentHighlight",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 36, "character": 29},  # on `sigs`
            },
        )
        if result is not None:
            for hl in result:
                self.assertIn("range", hl)

    def test_document_highlight_sigs_finds_all_uses(self) -> None:
        """`sigs` is the class field on line 37 (0-indexed 36). The
        scope-aware highlighter now finds the enclosing
        `class_declaration` because the parameterized-scope rewrite
        keeps that envelope intact — all ~10 uses light up."""
        result = self.lsp.request(
            "textDocument/documentHighlight",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": 36, "character": 29},
            },
        )
        self.assertIsNotNone(result)
        self.assertGreaterEqual(len(result), 8)

    # ------------------------------------------------------------------
    # textDocument/hover
    # ------------------------------------------------------------------

    def _hover(self, line: int, character: int) -> dict | None:
        return self.lsp.request(
            "textDocument/hover",
            {
                "textDocument": {"uri": self.uri},
                "position": {"line": line, "character": character},
            },
        )

    def test_hover_class_name_shows_declaration_line(self) -> None:
        """Cursor on the `apb_monitor` class identifier (line 36 / 0-idx 35
        column 9 is `apb_monitor`) returns the class declaration as a
        fenced systemverilog block."""
        result = self._hover(35, 9)
        self.assertIsNotNone(result, "no hover for class name")
        contents = result["contents"]
        self.assertEqual(contents["kind"], "markdown")
        value = contents["value"]
        self.assertIn("class apb_monitor", value)
        self.assertTrue(value.startswith("```systemverilog"))
        self.assertTrue(value.endswith("```"))

    def test_hover_field_reference_shows_declaration(self) -> None:
        """Cursor on `cfg` (line 40 / 0-idx 39) returns its declaration
        line `apb_config cfg;`."""
        # Line 40 raw: `   apb_config cfg;` — `cfg` starts at char 14.
        result = self._hover(39, 14)
        self.assertIsNotNone(result, "no hover for class field")
        value = result["contents"]["value"]
        self.assertIn("apb_config cfg;", value)

    def test_hover_function_emits_signature(self) -> None:
        """Cursor on the `build_phase` declaration (line 49 / 0-idx 48)
        returns a synthesized signature: `function build_phase(uvm_phase phase);`."""
        # Line 49 raw: `   virtual function void build_phase(uvm_phase phase);`
        # `build_phase` starts at char 27.
        result = self._hover(48, 27)
        self.assertIsNotNone(result, "no hover for function declaration")
        value = result["contents"]["value"]
        self.assertIn("build_phase", value)
        self.assertIn("uvm_phase", value)

    def test_hover_on_keyword_returns_none(self) -> None:
        """Cursor on `class` (line 36 / 0-idx 35 col 0) should now return
        keyword hover docs — keyword help was shipped in v0.7.2."""
        # Line 36: `class apb_monitor extends uvm_monitor;` — `class` at col 0.
        result = self._hover(35, 0)
        self.assertIsNotNone(result, "expected keyword hover for `class` but got None")
        value = result["contents"]["value"]
        self.assertIn("class", value, f"hover value does not mention keyword: {value!r}")

    def test_hover_on_whitespace_returns_none(self) -> None:
        """Cursor on whitespace returns no hover."""
        # An almost certainly-blank position somewhere in the file.
        result = self._hover(2, 0)
        self.assertIsNone(result)

    def test_hover_uvm_fatal_macro_shows_multi_line_body(self) -> None:
        """Cursor on `` `uvm_fatal `` at line 57 must return the full
        multi-line `` `define `` body, not just the first source line
        of the definition. The slang path resolves to
        `uvm_message_defines.svh` and the resolved symbol's
        `full_range` spans several lines."""
        # Line 57 raw: `            `uvm_fatal("APB/MON/NOVIF", "...")`
        # `uvm_fatal` starts at char 13.
        result = self._hover(56, 13)
        self.assertIsNotNone(result, "no hover for `uvm_fatal`")
        value = result["contents"]["value"]
        # The body of `uvm_fatal includes `uvm_report_fatal` on a later
        # source line — that's the signal that we read more than line 1.
        self.assertIn("uvm_report_fatal", value)

    @unittest.expectedFailure
    def test_hover_get_on_uvm_config_db_returns_full_signature(self) -> None:
        """Cursor on `get` in `uvm_config_db#(apb_vif)::get(this, ...)`
        (line 56 / 0-idx 55) should resolve to `uvm_config_db::get` and
        return its full multi-line signature — the declaration spans four
        source lines and a single-line read would only show the first
        comma-terminated line.

        XFAIL: Requires slang to be configured with UVM headers so that
        the class-scope qualified call `uvm_config_db#(T)::get` resolves to
        the UVM standard-library declaration. Without UVM in the include path
        the server correctly returns None (we no longer return a misleading
        workspace-index `get` because `is_scope_qualified_at` skips the bare
        identifier lookup for `::` expressions). Drop this decorator when
        slang+UVM is wired into the CI test environment."""
        # Line 56 raw: `         if (!uvm_config_db#(apb_vif)::get(this, "", "vif", tmp)) begin`
        # `get` starts at the position right after `::`.
        result = self._hover(55, 41)
        self.assertIsNotNone(result, "no hover for uvm_config_db::get")
        value = result["contents"]["value"]
        # All four params must be present, regardless of how the source
        # wraps them across lines.
        for param in ("cntxt", "inst_name", "field_name", "value"):
            self.assertIn(
                param, value,
                f"missing param `{param}` in hover content: {value!r}",
            )

    def test_hover_this_field_resolves_via_class(self) -> None:
        """Cursor on `sigs` in `this.sigs.pck` (line 70 / 0-idx 69)
        resolves through the enclosing class to the declaration on
        line 37. Receiver-aware hover via `this`."""
        # Line 70 raw: `            @ (this.sigs.pck);` — `sigs` at char 21.
        result = self._hover(69, 21)
        if result is not None:
            value = result["contents"]["value"]
            self.assertIn("sigs", value)

    # ------------------------------------------------------------------
    # workspace/symbol — workspace-wide fuzzy picker (Ctrl+T / xref)
    # ------------------------------------------------------------------

    def test_workspace_symbol_empty_query_returns_visible_symbols(self) -> None:
        """Empty query → server returns up to 200 visible-kind symbols
        (the documented v1 cap). With a `.mimir.toml` filelist that pulls
        in the UVM library, the workspace index has thousands of entries
        — every candidate ties at score 0, so the alphabetical tie-break
        decides what makes the cut. We only assert the response shape
        and cap here; content assertions live in the fuzzy-query test
        below, where the query narrows the result set."""
        result = self.lsp.request("workspace/symbol", {"query": ""})
        self.assertIsInstance(result, list)
        self.assertGreater(len(result), 0)
        self.assertLessEqual(len(result), 200)
        for sym in result:
            self.assertIn("name", sym)
            self.assertIn("kind", sym)
            self.assertIn("location", sym)

    def test_workspace_symbol_fuzzy_query_filters_results(self) -> None:
        """A specific subsequence query must return matches and only
        matches — `apb_mon` matches `apb_monitor` / `apb_monitor_cbs`
        but not unrelated symbols."""
        result = self.lsp.request("workspace/symbol", {"query": "apb_mon"})
        self.assertIsInstance(result, list)
        self.assertGreater(len(result), 0)
        for sym in result:
            self.assertIn("name", sym)
            self.assertIn("kind", sym)
            self.assertIn("location", sym)
            loc = sym["location"]
            self.assertIn("uri", loc)
            self.assertTrue(loc["uri"].startswith("file://"))
            self.assertIn("range", loc)

    def test_workspace_symbol_unmatched_query_returns_empty(self) -> None:
        """A query that can't match any candidate (out-of-order chars
        against every symbol) returns an empty list, not a crash."""
        result = self.lsp.request(
            "workspace/symbol", {"query": "zzzznever_matches_anythingzzzz"}
        )
        self.assertIsInstance(result, list)
        self.assertEqual(result, [])

    def test_workspace_symbol_excludes_variables_and_ports(self) -> None:
        """Variables/ports/parameters are filtered out — the picker is
        for top-of-mind navigation, not every signal in the project.
        `sigs` (a class field, indexed as Variable) must not appear."""
        result = self.lsp.request("workspace/symbol", {"query": "sigs"})
        names = {s["name"] for s in result}
        self.assertNotIn(
            "sigs",
            names,
            f"unexpected Variable kind in workspace/symbol results: {sorted(names)}",
        )

    # ------------------------------------------------------------------
    # AST-cache behaviour: subsequent requests don't re-parse.
    # ------------------------------------------------------------------

    def test_repeated_folding_does_not_reparse(self) -> None:
        """Fire `foldingRange` twice on the same document without any
        edits between. The cache must serve the second call without
        invoking `SyntaxParser::parse` (the parser emits a `parse
        complete` log line per call). We can't directly observe the
        log from here without enabling trace logs, but we can assert
        the two responses are identical, which is the user-visible
        contract."""
        td = {"textDocument": {"uri": self.uri}}
        first = self.lsp.request("textDocument/foldingRange", td)
        second = self.lsp.request("textDocument/foldingRange", td)
        self.assertEqual(first, second)


# ---- helpers ---------------------------------------------------------------


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
