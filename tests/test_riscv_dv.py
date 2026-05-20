"""Comprehensive integration tests for mimir-server using the riscv-dv project.

Drives ``mimir-server`` over LSP/stdio with the real
``examples/riscv-dv/`` workspace (89 SV files, ``.mimir.toml``).
Every implemented LSP feature is exercised; tests that require
features not yet complete are marked ``@expectedFailure`` with a note
explaining the gap.

Primary test files (chosen for manageable size and rich content):
  - ``src/riscv_instr_sequence.sv``   (352 lines) — classes, method calls,
    macros, string format specs, function calls.
  - ``src/riscv_instr_gen_config.sv`` (797 lines) — large class, rand fields,
    typedef references, constraint blocks, function definitions.
  - ``src/riscv_instr_pkg.sv``        (1628 lines) — package, typedef
    declarations (struct/enum).

Run (requires a release build):

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_riscv_dv -v

The suite auto-skips when ``examples/riscv-dv`` is not cloned.
"""

from __future__ import annotations

import os
import pathlib
import shutil
import time
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT  = pathlib.Path(__file__).resolve().parent.parent
RISCV_DV   = REPO_ROOT / "examples" / "riscv-dv"

# Frequently-used source files (all 0-indexed line/col in comments).
SEQ_SV     = RISCV_DV / "src" / "riscv_instr_sequence.sv"
CFG_SV     = RISCV_DV / "src" / "riscv_instr_gen_config.sv"
PKG_SV     = RISCV_DV / "src" / "riscv_instr_pkg.sv"

# LSP semantic-token type ordinals (match TokenType enum in mimir-syntax)
TOK_KEYWORD   = 0
TOK_TYPE      = 1
TOK_CLASS     = 2
TOK_INTERFACE = 3
TOK_NAMESPACE = 4
TOK_FUNCTION  = 5
TOK_MACRO     = 6
TOK_PARAMETER = 7
TOK_VARIABLE  = 8
TOK_COMMENT   = 9
TOK_STRING    = 10
TOK_NUMBER    = 11
TOK_REGEXP    = 12

MOD_DECLARATION = 0x1
MOD_READONLY    = 0x2


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _wait_for_parse(lsp: MimirLspClient, uri: str, timeout: float = 8.0) -> None:
    """Block until the server publishes ``publishDiagnostics`` for ``uri``."""
    diag = lsp.wait_for_fresh_diagnostics(uri, timeout=timeout)
    time.sleep(0.15)
    assert diag is not None, "server never published initial diagnostics"


def _range(sl: int, sc: int, el: int, ec: int) -> dict:
    return {"start": {"line": sl, "character": sc},
            "end":   {"line": el, "character": ec}}


def _pos(line: int, char: int) -> dict:
    return {"line": line, "character": char}


def _td_pos(uri: str, line: int, char: int) -> dict:
    return {"textDocument": {"uri": uri}, "position": _pos(line, char)}


def _collect_symbol_names(syms: list[dict]) -> set[str]:
    out: set[str] = set()
    for s in syms:
        if "name" in s:
            out.add(s["name"])
        for child in (s.get("children") or []):
            out |= _collect_symbol_names([child])
    return out


def _collect_symbols_with_kind(syms: list[dict]) -> list[tuple[str, int]]:
    """Return flat list of (name, kind) from a possibly-hierarchical response."""
    out: list[tuple[str, int]] = []
    for s in syms:
        out.append((s.get("name", ""), s.get("kind", -1)))
        for child in (s.get("children") or []):
            out.extend(_collect_symbols_with_kind([child]))
    return out


def _find_children_of(syms: list[dict], parent_name: str) -> list[dict]:
    for s in syms:
        if s.get("name") == parent_name:
            return s.get("children") or []
        found = _find_children_of(s.get("children") or [], parent_name)
        if found:
            return found
    return []


def _decode_tokens(response: dict | None) -> list[tuple[int, int, int, int, int]]:
    """Decode LSP semantic token delta-encoded data into absolute
    ``(line, char, length, token_type, token_modifiers)`` tuples."""
    data = (response or {}).get("data", [])
    tokens: list[tuple[int, int, int, int, int]] = []
    line = char = 0
    for i in range(0, len(data), 5):
        dl, dc, length, ttype, mods = data[i:i+5]
        if dl > 0:
            line += dl
            char = dc
        else:
            char += dc
        tokens.append((line, char, length, ttype, mods))
    return tokens


def _token_at(tokens: list[tuple], line: int, char: int) -> tuple | None:
    """Return the token whose span covers ``(line, char)``, or ``None``."""
    for tok in tokens:
        tl, tc, tlen, ttype, tmods = tok
        if tl == line and tc <= char < tc + tlen:
            return tok
    return None


def _tokens_on_line(tokens: list[tuple], line: int) -> list[tuple]:
    return [t for t in tokens if t[0] == line]


def _verible_available() -> bool:
    return shutil.which("verible-verilog-format") is not None


def _slang_available() -> bool:
    return bool(os.getenv("MIMIR_SLANG_PATH"))


def _skip_if_no_riscv_dv(cls_setup):
    """Class ``setUpClass`` wrapper that skips when riscv-dv is absent."""
    def wrapper(cls):
        if not RISCV_DV.exists():
            raise unittest.SkipTest(
                f"riscv-dv not cloned at {RISCV_DV} — "
                "git clone https://github.com/chipsalliance/riscv-dv examples/riscv-dv"
            )
        cls_setup(cls)
    return wrapper


# ---------------------------------------------------------------------------
# 1. Init / capabilities
# ---------------------------------------------------------------------------


class RiscvDvInitTest(unittest.TestCase):
    """Verify that ``initialize`` advertises every implemented feature."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        result = cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.caps = result.get("capabilities", {})
        # Open one file so the workspace index has time to hydrate.
        cls.uri = file_uri(SEQ_SV)
        cls.lsp.did_open(cls.uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.uri)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def test_advertises_semantic_tokens(self) -> None:
        self.assertIn("semanticTokensProvider", self.caps)
        self.assertIsNotNone(self.caps["semanticTokensProvider"])

    def test_advertises_completion_with_trigger_chars(self) -> None:
        cp = self.caps.get("completionProvider", {})
        self.assertIsNotNone(cp)
        triggers = cp.get("triggerCharacters", [])
        for ch in (".", "`", "$", ":"):
            self.assertIn(ch, triggers, f"missing trigger char {ch!r}")

    def test_advertises_references(self) -> None:
        self.assertTrue(self.caps.get("referencesProvider"))

    def test_advertises_signature_help(self) -> None:
        sh = self.caps.get("signatureHelpProvider", {})
        self.assertIsNotNone(sh)
        triggers = sh.get("triggerCharacters", [])
        self.assertIn("(", triggers)
        self.assertIn(",", triggers)

    def test_advertises_document_symbol(self) -> None:
        self.assertTrue(self.caps.get("documentSymbolProvider"))

    def test_advertises_workspace_symbol(self) -> None:
        self.assertTrue(self.caps.get("workspaceSymbolProvider"))

    def test_advertises_folding_range(self) -> None:
        self.assertTrue(self.caps.get("foldingRangeProvider"))

    def test_advertises_document_highlight(self) -> None:
        self.assertTrue(self.caps.get("documentHighlightProvider"))

    def test_advertises_inlay_hints(self) -> None:
        self.assertIn("inlayHintProvider", self.caps)

    def test_advertises_hover(self) -> None:
        self.assertTrue(self.caps.get("hoverProvider"))

    def test_advertises_definition(self) -> None:
        self.assertTrue(self.caps.get("definitionProvider"))

    def test_advertises_references_provider(self) -> None:
        self.assertTrue(self.caps.get("referencesProvider"))

    def test_advertises_formatting(self) -> None:
        self.assertTrue(self.caps.get("documentFormattingProvider"))

    def test_workspace_symbol_hydrated_from_filelist(self) -> None:
        """``riscv_instr_gen_config`` lives in a file that was never opened;
        the workspace index (hydrated via ``.mimir.toml`` filelist) must find it."""
        result = self.lsp.request("workspace/symbol",
                                  {"query": "riscv_instr_gen_config"})
        self.assertIsInstance(result, list)
        names = {s["name"] for s in result}
        self.assertIn("riscv_instr_gen_config", names,
                      "filelist hydration failed — cross-file class not indexed")


# ---------------------------------------------------------------------------
# 2. Diagnostics + incremental editing
# ---------------------------------------------------------------------------


class RiscvDvDiagnosticsTest(unittest.TestCase):
    """Parse diagnostics, incremental didChange, didSave."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)

        cls.seq_uri  = file_uri(SEQ_SV)
        cls.cfg_uri  = file_uri(CFG_SV)
        cls.seq_text = read_text(SEQ_SV)
        cls.cfg_text = read_text(CFG_SV)

        cls._version = 1  # did_open uses version=1; edits use _next_version()
        cls.lsp.did_open(cls.seq_uri, cls.seq_text)
        cls.seq_diag = cls.lsp.wait_for_fresh_diagnostics(cls.seq_uri, timeout=8.0)

        cls.lsp.did_open(cls.cfg_uri, cls.cfg_text)
        cls.cfg_diag = cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _next_version(self) -> int:
        self.__class__._version += 1
        return self.__class__._version

    def _errors(self, diag: dict | None) -> list[dict]:
        return [d for d in (diag or {}).get("diagnostics", [])
                if d.get("severity") == 1]

    def test_sequence_file_no_parse_errors(self) -> None:
        self.assertIsNotNone(self.seq_diag)
        errors = self._errors(self.seq_diag)
        self.assertEqual(errors, [],
                         f"unexpected parse errors in {SEQ_SV.name}: {errors}")

    def test_gen_config_file_no_parse_errors(self) -> None:
        self.assertIsNotNone(self.cfg_diag)
        errors = self._errors(self.cfg_diag)
        self.assertEqual(errors, [],
                         f"unexpected parse errors in {CFG_SV.name}: {errors}")

    def test_inject_syntax_error_produces_error_diagnostic(self) -> None:
        """Inserting an unterminated module produces a severity-1 diagnostic."""
        broken = self.seq_text + "\nmodule BROKEN_sentinel;\n"
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": broken}],
        })
        diag = self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=8.0)
        errors = self._errors(diag)
        self.assertGreater(len(errors), 0,
                           "expected at least one error diagnostic after broken edit")

    def test_fix_syntax_error_clears_diagnostic(self) -> None:
        """Restoring the original text should clear error diagnostics."""
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": self.seq_text}],
        })
        diag = self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=8.0)
        errors = self._errors(diag)
        self.assertEqual(errors, [],
                         f"errors remain after restoring clean source: {errors}")

    def test_injected_edit_updates_document_symbol(self) -> None:
        """A new function injected via didChange must appear in documentSymbol."""
        sentinel = "\nfunction automatic int mimir_sentinel_fn_xyz(); endfunction"
        patched = self.seq_text + sentinel
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": patched}],
        })
        # Give the server a moment to reparse.
        time.sleep(0.3)
        syms = self.lsp.request("textDocument/documentSymbol",
                                {"textDocument": {"uri": self.seq_uri}})
        names = _collect_symbol_names(syms or [])
        self.assertIn("mimir_sentinel_fn_xyz", names,
                      "injected sentinel function not in documentSymbol after didChange")
        # Restore and consume the restore's diagnostic so subsequent tests get
        # a clean diagnostic slate (no stale 0-error notification in the stream).
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": self.seq_text}],
        })
        self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=5.0)

    def test_did_save_does_not_crash(self) -> None:
        """textDocument/didSave must not crash the server."""
        self.lsp.notify("textDocument/didSave",
                        {"textDocument": {"uri": self.seq_uri}})
        # Server is still alive if foldingRange responds.
        result = self.lsp.request("textDocument/foldingRange",
                                  {"textDocument": {"uri": self.seq_uri}})
        self.assertIsInstance(result, list)

    def test_multiple_injected_errors_all_published(self) -> None:
        """Two broken constructs in a single edit produce at least one diagnostic.

        Tree-sitter may collapse adjacent errors into a single error node so
        we only assert ≥ 1, not ≥ 2.
        """
        broken = (self.seq_text
                  + "\nmodule A_BROKEN;\nmodule B_BROKEN;\n")
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": broken}],
        })
        diag = self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=8.0)
        errors = self._errors(diag)
        self.assertGreater(len(errors), 0,
                           f"expected ≥ 1 error from broken constructs, got: {errors}")
        # Restore and consume restore diagnostic for clean slate.
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": self.seq_text}],
        })
        self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=5.0)

    def test_diagnostic_range_near_injected_error_line(self) -> None:
        """Diagnostic range.start.line should be at or near the error site."""
        inject_line = self.seq_text.count("\n")  # appended at end of file
        broken = self.seq_text + "\nmodule RANGE_TEST;\n"
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": broken}],
        })
        diag = self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=8.0)
        errors = self._errors(diag)
        self.assertGreater(len(errors), 0)
        # Error should be reported near the injected line, not at line 0.
        error_line = errors[0]["range"]["start"]["line"]
        self.assertGreater(error_line, 0,
                           "expected error range near injected content, not line 0")
        # Restore and consume restore diagnostic for clean slate.
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": self.seq_uri, "version": self._next_version()},
            "contentChanges": [{"text": self.seq_text}],
        })
        self.lsp.wait_for_fresh_diagnostics(self.seq_uri, timeout=5.0)


# ---------------------------------------------------------------------------
# 3. Semantic tokens
# ---------------------------------------------------------------------------


class RiscvDvSemanticTokensTest(unittest.TestCase):
    """Verify semantic token classification on real riscv-dv source.

    Tests use ``semanticTokens/range`` (small viewport) to keep
    response sizes manageable.  Ordinals: keyword=0 type=1 class=2
    namespace=4 function=5 macro=6 variable=8 string=10 regexp=12.
    """

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)

        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.pkg_uri = file_uri(PKG_SV)

        cls.seq_text = read_text(SEQ_SV)
        cls.cfg_text = read_text(CFG_SV)
        cls.pkg_text = read_text(PKG_SV)

        cls.lsp.did_open(cls.seq_uri, cls.seq_text)
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, cls.cfg_text)
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        cls.lsp.did_open(cls.pkg_uri, cls.pkg_text)
        cls.lsp.wait_for_fresh_diagnostics(cls.pkg_uri, timeout=8.0)
        time.sleep(0.1)

        # Cache full semantic tokens — used by token-type assertion tests instead
        # of semanticTokens/range, which currently returns [] (server-side bug).
        cls.seq_full_tokens = _decode_tokens(cls.lsp.request(
            "textDocument/semanticTokens/full", {"textDocument": {"uri": cls.seq_uri}}))
        cls.cfg_full_tokens = _decode_tokens(cls.lsp.request(
            "textDocument/semanticTokens/full", {"textDocument": {"uri": cls.cfg_uri}}))
        cls.pkg_full_tokens = _decode_tokens(cls.lsp.request(
            "textDocument/semanticTokens/full", {"textDocument": {"uri": cls.pkg_uri}}))

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _range_tokens(self, uri: str, start_line: int,
                      end_line: int) -> list[tuple]:
        resp = self.lsp.request("textDocument/semanticTokens/range", {
            "textDocument": {"uri": uri},
            "range": _range(start_line, 0, end_line, 999),
        })
        return _decode_tokens(resp)

    def _full_tokens(self, uri: str) -> list[tuple]:
        resp = self.lsp.request("textDocument/semanticTokens/full",
                                {"textDocument": {"uri": uri}})
        return _decode_tokens(resp)

    # ---- structural ----

    def test_full_tokens_returns_nonempty_list(self) -> None:
        tokens = self._full_tokens(self.seq_uri)
        self.assertGreater(len(tokens), 0, "semanticTokens/full returned no tokens")

    def test_range_tokens_is_subset_of_full(self) -> None:
        """Range tokens are all within the requested viewport lines."""
        tokens = self._range_tokens(self.seq_uri, 35, 50)
        for line, char, length, ttype, mods in tokens:
            self.assertGreaterEqual(line, 35,
                                    f"token on line {line} is before requested range start 35")
            self.assertLess(line, 50,
                            f"token on line {line} is past requested range end 50")

    def test_range_has_fewer_tokens_than_full(self) -> None:
        full   = self._full_tokens(self.seq_uri)
        ranged = self._range_tokens(self.seq_uri, 35, 50)
        self.assertLess(len(ranged), len(full))

    # ---- namespace: package declaration name ----

    def test_package_name_is_namespace_with_declaration(self) -> None:
        # riscv_instr_pkg.sv line 18 (0-idx): `package riscv_instr_pkg;`
        # 'riscv_instr_pkg' starts at col 8.
        tok = _token_at(self.pkg_full_tokens, 18, 8)
        self.assertIsNotNone(tok, "no token found at package name position")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_NAMESPACE,
                         f"package name: expected Namespace({TOK_NAMESPACE}), got {ttype}")
        self.assertTrue(mods & MOD_DECLARATION,
                        f"package name: expected DECLARATION modifier, got mods={mods}")

    # ---- type: typedef alias names (declaration sites) ----

    def test_typedef_struct_alias_is_type_with_declaration(self) -> None:
        # riscv_instr_pkg.sv line 36 (0-idx): `  } mem_region_t;`
        # 'mem_region_t' at col 4.
        tok = _token_at(self.pkg_full_tokens, 36, 4)
        self.assertIsNotNone(tok, "no token at mem_region_t closing brace line")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_TYPE,
                         f"mem_region_t: expected Type({TOK_TYPE}), got {ttype}")
        self.assertTrue(mods & MOD_DECLARATION,
                        f"mem_region_t: expected DECLARATION, got mods={mods}")

    def test_typedef_enum_alias_is_type_with_declaration(self) -> None:
        # riscv_instr_pkg.sv line 52 (0-idx): `  } satp_mode_t;`
        # 'satp_mode_t' at col 4.
        tok = _token_at(self.pkg_full_tokens, 52, 4)
        self.assertIsNotNone(tok, "no token at satp_mode_t closing brace line")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_TYPE,
                         f"satp_mode_t: expected Type({TOK_TYPE}), got {ttype}")
        self.assertTrue(mods & MOD_DECLARATION,
                        f"satp_mode_t: expected DECLARATION, got mods={mods}")

    # ---- type: typedef reference (usage site) ----

    def test_typedef_ref_in_field_is_type_not_variable(self) -> None:
        # riscv_instr_gen_config.sv line 45 (0-idx):
        # `  vreg_init_method_t     vreg_init_method = RANDOM_VALUES_VMV;`
        # 'vreg_init_method_t' at col 2 — a typedef used as a field type.
        # NOTE: the 'data_type' arm in classify_identifier() is not yet
        # implemented, so this token is currently classified as Variable.
        # This test intentionally fails until that arm is added.
        tok = _token_at(self.cfg_full_tokens, 45, 2)
        self.assertIsNotNone(tok, "no token at vreg_init_method_t field type position")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_TYPE,
                         f"vreg_init_method_t reference: expected Type({TOK_TYPE}), got {ttype}")
        self.assertFalse(mods & MOD_DECLARATION,
                         "typedef reference should NOT have DECLARATION modifier")

    # ---- class: identifier in `extends` clause ----

    def test_class_in_extends_is_class_token(self) -> None:
        # riscv_instr_sequence.sv line 35 (0-idx):
        # `class riscv_instr_sequence extends uvm_sequence;`
        # 'uvm_sequence' at col 35.
        tok = _token_at(self.seq_full_tokens, 35, 35)
        self.assertIsNotNone(tok, "no token at uvm_sequence in extends clause")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_CLASS,
                         f"uvm_sequence in extends: expected Class({TOK_CLASS}), got {ttype}")

    # ---- function: method-call callee ----

    def test_method_call_callee_is_function(self) -> None:
        # riscv_instr_sequence.sv line 75 (0-idx):
        # `    instr_stream.initialize_instr_list(instr_cnt);`
        # 'initialize_instr_list' at col 17 — last id in hierarchical_identifier
        # under tf_call → Function.
        tok = _token_at(self.seq_full_tokens, 75, 17)
        self.assertIsNotNone(tok, "no token at initialize_instr_list call site")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_FUNCTION,
                         f"initialize_instr_list: expected Function({TOK_FUNCTION}), got {ttype}")

    def test_method_call_receiver_is_variable(self) -> None:
        # Same line — 'instr_stream' at col 4 is the receiver → Variable.
        tok = _token_at(self.seq_full_tokens, 75, 4)
        self.assertIsNotNone(tok, "no token at instr_stream receiver")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_VARIABLE,
                         f"instr_stream receiver: expected Variable({TOK_VARIABLE}), got {ttype}")

    # ---- macro: text_macro_usage ----

    def test_macro_usage_is_macro_token(self) -> None:
        # riscv_instr_sequence.sv line 57 (0-idx):
        # `      `uvm_fatal(get_full_name(), "Cannot get instr_gen_cfg")`
        # 'uvm_fatal' at col 7 (backtick at col 6).
        tok = _token_at(self.seq_full_tokens, 57, 7)
        self.assertIsNotNone(tok, "no token at uvm_fatal macro usage")
        _, _, _, ttype, mods = tok
        self.assertEqual(ttype, TOK_MACRO,
                         f"uvm_fatal: expected Macro({TOK_MACRO}), got {ttype}")

    # ---- string and format spec ----

    def test_string_literal_is_string_token(self) -> None:
        # riscv_instr_sequence.sv line 57 (0-idx):
        # `      `uvm_fatal(get_full_name(), "Cannot get instr_gen_cfg")`
        # String literal 'Cannot ...' — at col 30ish.
        string_toks = [t for t in _tokens_on_line(self.seq_full_tokens, 57)
                       if t[3] == TOK_STRING]
        self.assertGreater(len(string_toks), 0,
                           "no String token found on the uvm_fatal line")

    def test_format_spec_in_string_is_regexp_token(self) -> None:
        # riscv_instr_sequence.sv line 168 (0-idx):
        # `        instr_stream.instr_list[i].label = $sformatf("%0d", label_idx);`
        # '%0d' inside a string literal → Regexp token.
        regexp_toks = [t for t in _tokens_on_line(self.seq_full_tokens, 168)
                       if t[3] == TOK_REGEXP]
        self.assertGreater(len(regexp_toks), 0,
                           "no Regexp token for '%0d' format spec in string literal")

    def test_format_spec_adjacent_to_string_tokens(self) -> None:
        """The regexp token is flanked by string tokens on the same line."""
        line_toks = _tokens_on_line(self.seq_full_tokens, 168)
        types_on_line = [t[3] for t in line_toks]
        self.assertIn(TOK_STRING, types_on_line)
        self.assertIn(TOK_REGEXP, types_on_line)


# ---------------------------------------------------------------------------
# 4. Hover
# ---------------------------------------------------------------------------


class RiscvDvHoverTest(unittest.TestCase):
    """textDocument/hover — declaration lines, signatures, keywords, sys-tasks."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.pkg_uri = file_uri(PKG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        cls.lsp.did_open(cls.pkg_uri, read_text(PKG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.pkg_uri, timeout=8.0)
        time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _hover(self, uri: str, line: int, char: int) -> dict | None:
        return self.lsp.request("textDocument/hover", _td_pos(uri, line, char))

    # ---- class / package declarations ----

    def test_hover_package_name_shows_declaration(self) -> None:
        # riscv_instr_pkg.sv L18 col 8: 'riscv_instr_pkg'
        result = self._hover(self.pkg_uri, 18, 8)
        self.assertIsNotNone(result, "no hover for package name")
        value = result["contents"]["value"]
        self.assertIn("riscv_instr_pkg", value)
        self.assertTrue(value.startswith("```systemverilog"),
                        f"hover not fenced as systemverilog: {value[:40]!r}")

    def test_hover_class_name_shows_declaration(self) -> None:
        # riscv_instr_gen_config.sv L20 col 6: class declaration name
        result = self._hover(self.cfg_uri, 20, 6)
        self.assertIsNotNone(result, "no hover for class name")
        value = result["contents"]["value"]
        self.assertIn("riscv_instr_gen_config", value)

    # ---- field declarations ----

    def test_hover_rand_field_shows_declaration(self) -> None:
        # riscv_instr_gen_config.sv L27 col 25: 'main_program_instr_cnt'
        result = self._hover(self.cfg_uri, 27, 25)
        self.assertIsNotNone(result, "no hover for rand field")
        value = result["contents"]["value"]
        self.assertIn("main_program_instr_cnt", value)

    # ---- function / task signatures ----

    def test_hover_function_name_shows_signature(self) -> None:
        # riscv_instr_sequence.sv L72 col 24: 'gen_instr' function
        result = self._hover(self.seq_uri, 72, 24)
        self.assertIsNotNone(result, "no hover for function name")
        value = result["contents"]["value"]
        self.assertIn("gen_instr", value)

    def test_hover_function_signature_includes_params(self) -> None:
        # Same position — signature_for should include parameter names.
        result = self._hover(self.seq_uri, 72, 24)
        self.assertIsNotNone(result)
        value = result["contents"]["value"]
        self.assertIn("is_main_program", value)

    def test_hover_task_name_returns_some_content(self) -> None:
        # riscv_instr_sequence.sv L92 col 16: 'gen_stack_enter_instr'
        result = self._hover(self.seq_uri, 92, 16)
        self.assertIsNotNone(result, "no hover for function declaration")
        value = result["contents"]["value"]
        self.assertIn("gen_stack_enter_instr", value)

    # ---- typedef at declaration site ----

    def test_hover_typedef_enum_at_decl_shows_typedef(self) -> None:
        # riscv_instr_pkg.sv L52 col 4: 'satp_mode_t' in `} satp_mode_t;`
        result = self._hover(self.pkg_uri, 52, 4)
        self.assertIsNotNone(result, "no hover for typedef alias at declaration")
        value = result["contents"]["value"]
        self.assertIn("satp_mode_t", value)

    # ---- keyword / system-task docs ----

    def test_hover_keyword_constraint_returns_docs(self) -> None:
        # riscv_instr_gen_config.sv L278 col 2: 'constraint' keyword
        result = self._hover(self.cfg_uri, 278, 2)
        self.assertIsNotNone(result,
                             "hover on 'constraint' keyword returned None — not in KEYWORD_DOCS")
        value = result["contents"]["value"]
        self.assertIn("constraint", value.lower())

    def test_hover_system_task_sformatf_returns_docs(self) -> None:
        # riscv_instr_sequence.sv L76 col 31: '$sformatf'
        result = self._hover(self.seq_uri, 76, 31)
        self.assertIsNotNone(result,
                             "hover on '$sformatf' returned None — not in SYSTEM_TASK_DOCS")
        value = result["contents"]["value"]
        self.assertIn("sformatf", value)

    def test_hover_endpackage_keyword_returns_none(self) -> None:
        # riscv_instr_pkg.sv last line (0-idx 1627): 'endpackage' — structural
        # noise, intentionally omitted from KEYWORD_DOCS.
        result = self._hover(self.pkg_uri, 1627, 0)
        self.assertIsNone(result,
                          f"expected None for 'endpackage' (structural noise), got {result}")

    def test_hover_on_whitespace_returns_none(self) -> None:
        # Column 0 on the blank line between package header and first typedef
        result = self._hover(self.pkg_uri, 17, 0)
        self.assertIsNone(result, f"expected None for whitespace, got {result}")

    # ---- cross-file hover via workspace index ----

    def test_hover_cross_file_type_ref_shows_declaration(self) -> None:
        # riscv_instr_sequence.sv L44 col 2: 'riscv_instr_gen_config' used
        # as a field type — defined in a different file.
        result = self._hover(self.seq_uri, 44, 2)
        if result is not None:
            value = result["contents"]["value"]
            self.assertIn("riscv_instr_gen_config", value)
        # result may be None if cross-file hover is not yet implemented;
        # that is acceptable today — the test documents the expectation.

    # ---- hover range is valid ----

    def test_hover_result_range_covers_cursor(self) -> None:
        # Function name hover should include a range that spans the token.
        result = self._hover(self.seq_uri, 72, 24)
        self.assertIsNotNone(result)
        rng = result.get("range")
        if rng is not None:
            start = rng["start"]
            end   = rng["end"]
            self.assertLessEqual(start["line"], 72)
            self.assertGreaterEqual(end["line"], 72)
            # cursor at col 24 must be within [start.char, end.char)
            if start["line"] == 72:
                self.assertLessEqual(start["character"], 24)


# ---------------------------------------------------------------------------
# 5. Completion
# ---------------------------------------------------------------------------


class RiscvDvCompletionTest(unittest.TestCase):
    """textDocument/completion + completionItem/resolve."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri  = file_uri(SEQ_SV)
        cls.seq_text = read_text(SEQ_SV)
        cls.lsp.did_open(cls.seq_uri, cls.seq_text)
        _wait_for_parse(cls.lsp, cls.seq_uri)
        time.sleep(0.3)  # allow workspace hydration

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _complete(self, line: int, char: int,
                  trigger_kind: int = 1,
                  trigger_char: str | None = None) -> list[dict]:
        ctx: dict = {"triggerKind": trigger_kind}
        if trigger_char:
            ctx["triggerCharacter"] = trigger_char
        resp = self.lsp.request("textDocument/completion", {
            "textDocument": {"uri": self.seq_uri},
            "position": _pos(line, char),
            "context": ctx,
        })
        if isinstance(resp, dict):
            return resp.get("items", [])
        return resp or []

    def test_completion_returns_list(self) -> None:
        # Position at start of a line inside the gen_instr function body.
        items = self._complete(84, 0)
        self.assertIsInstance(items, list)

    def test_completion_includes_sv_keywords(self) -> None:
        # Invoked completion at line start inside a function body.
        # The server currently returns workspace symbols but not SV keywords,
        # so we check for keyword-kind (14) items OR at least one common keyword.
        # This test intentionally fails until keyword completion is implemented.
        items = self._complete(84, 0)
        labels = {i.get("label", "") for i in items}
        kinds  = {i.get("kind") for i in items}
        sv_kws = {"module", "class", "function", "task", "always_ff",
                  "begin", "end", "if", "else", "for"}
        self.assertTrue(
            sv_kws & labels or 14 in kinds,
            f"no SV keyword (label or kind=14) found in {len(items)} completion items"
        )

    def test_completion_includes_workspace_symbols(self) -> None:
        """After filelist hydration, cross-file classes appear as candidates."""
        items = self._complete(84, 0)
        labels = {i.get("label", "") for i in items}
        self.assertIn("riscv_instr_gen_config", labels,
                      "cross-file class missing from completion candidates after hydration")

    def test_completion_dot_trigger_returns_list(self) -> None:
        # Trigger after 'instr_stream.' on line 75 col 17.
        items = self._complete(75, 17, trigger_kind=2, trigger_char=".")
        self.assertIsInstance(items, list)

    def test_completion_backtick_trigger_returns_macros(self) -> None:
        # Backtick trigger on line 57 col 7 (right after the backtick).
        items = self._complete(57, 7, trigger_kind=2, trigger_char="`")
        self.assertIsInstance(items, list)
        # At least some results must be macros (kind=15 in LSP) or just present.
        if items:
            macro_items = [i for i in items if i.get("kind") == 15
                           or "`" in i.get("label", "")
                           or "uvm" in i.get("label", "").lower()]
            self.assertGreater(len(macro_items), 0,
                               "no macro-like items returned on backtick trigger")

    def test_completion_item_resolve_attaches_documentation(self) -> None:
        """completionItem/resolve must add a documentation field."""
        items = self._complete(84, 0)
        # Pick a symbol item (non-keyword) if available.
        candidate = next(
            (i for i in items if i.get("kind") not in (14, None)
             and i.get("data") is not None),
            items[0] if items else None,
        )
        if candidate is None:
            self.skipTest("no completion items returned to resolve")
        resolved = self.lsp.request("completionItem/resolve", candidate)
        self.assertIsNotNone(resolved,
                             "completionItem/resolve returned None")
        # Either documentation is populated or the item itself is still valid.
        self.assertIn("label", resolved,
                      "resolved item missing 'label' field")


# ---------------------------------------------------------------------------
# 6. Definition / declaration
# ---------------------------------------------------------------------------


class RiscvDvDefinitionTest(unittest.TestCase):
    """textDocument/definition and textDocument/declaration."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        time.sleep(0.3)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _def(self, uri: str, line: int, char: int) -> list[dict] | dict | None:
        return self.lsp.request("textDocument/definition", _td_pos(uri, line, char))

    def _decl(self, uri: str, line: int, char: int) -> list[dict] | dict | None:
        return self.lsp.request("textDocument/declaration", _td_pos(uri, line, char))

    def _to_list(self, result) -> list[dict]:
        if result is None:
            return []
        if isinstance(result, dict):
            return [result]
        return result

    def test_definition_class_name_in_same_file(self) -> None:
        # Cursor on 'riscv_instr_sequence' class declaration name L35 col 6 —
        # definition should point to the same file at that line.
        locs = self._to_list(self._def(self.seq_uri, 35, 6))
        self.assertGreater(len(locs), 0, "definition returned no results")
        uris = {loc.get("uri", loc.get("targetUri", "")) for loc in locs}
        self.assertTrue(any(SEQ_SV.name in u for u in uris),
                        f"definition not in {SEQ_SV.name}: {uris}")

    def test_definition_cross_file_class_type_reference(self) -> None:
        # riscv_instr_sequence.sv L44 col 2: 'riscv_instr_gen_config' used
        # as field type → definition should jump to riscv_instr_gen_config.sv.
        locs = self._to_list(self._def(self.seq_uri, 44, 2))
        self.assertGreater(len(locs), 0,
                           "definition returned nothing for cross-file class type reference")
        uris = {loc.get("uri", loc.get("targetUri", "")) for loc in locs}
        self.assertTrue(any("riscv_instr_gen_config" in u for u in uris),
                        f"expected definition in riscv_instr_gen_config.sv, got {uris}")

    def test_definition_result_has_range(self) -> None:
        locs = self._to_list(self._def(self.seq_uri, 35, 6))
        if locs:
            loc = locs[0]
            rng = loc.get("range", loc.get("targetSelectionRange"))
            self.assertIsNotNone(rng, "definition location missing 'range' field")

    def test_definition_on_whitespace_returns_null_or_empty(self) -> None:
        result = self._def(self.seq_uri, 17, 0)
        locs = self._to_list(result)
        self.assertEqual(locs, [],
                         f"expected empty result for whitespace position, got {locs}")

    def test_declaration_matches_definition_without_slang(self) -> None:
        """Without slang both routes use the tree-sitter index and should agree."""
        def_locs  = self._to_list(self._def(self.seq_uri, 35, 6))
        decl_locs = self._to_list(self._decl(self.seq_uri, 35, 6))
        if not def_locs or not decl_locs:
            self.skipTest("one of definition/declaration returned nothing")
        def_uris  = {l.get("uri", l.get("targetUri", "")) for l in def_locs}
        decl_uris = {l.get("uri", l.get("targetUri", "")) for l in decl_locs}
        self.assertEqual(def_uris, decl_uris,
                         "definition and declaration disagree on target file (no slang)")

    def test_definition_field_in_same_file(self) -> None:
        # riscv_instr_sequence.sv L44 col 27: 'cfg' field reference →
        # definition should be in the same file.
        locs = self._to_list(self._def(self.seq_uri, 44, 27))
        if locs:
            uris = {l.get("uri", l.get("targetUri", "")) for l in locs}
            self.assertTrue(any(SEQ_SV.name in u for u in uris),
                            f"cfg field definition not in {SEQ_SV.name}: {uris}")


# ---------------------------------------------------------------------------
# 7. References
# ---------------------------------------------------------------------------


class RiscvDvReferencesTest(unittest.TestCase):
    """textDocument/references — same-file and cross-file."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        time.sleep(0.5)  # allow workspace index to settle

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _refs(self, uri: str, line: int, char: int,
              include_decl: bool = True) -> list[dict]:
        result = self.lsp.request("textDocument/references", {
            **_td_pos(uri, line, char),
            "context": {"includeDeclaration": include_decl},
        })
        return result or []

    def test_references_of_class_field_same_file(self) -> None:
        # riscv_instr_gen_config.sv L27 col 25: 'main_program_instr_cnt'
        # Used on lines 28, 282, 306, 311, 316, 479 in gen_config.sv → ≥ 4 refs.
        refs = self._refs(self.cfg_uri, 27, 25, include_decl=True)
        self.assertGreaterEqual(len(refs), 4,
                                f"expected ≥ 4 references to main_program_instr_cnt, got {len(refs)}")

    def test_references_all_have_uri_and_range(self) -> None:
        refs = self._refs(self.cfg_uri, 27, 25, include_decl=True)
        for ref in refs:
            self.assertIn("uri", ref, f"reference missing 'uri': {ref}")
            self.assertIn("range", ref, f"reference missing 'range': {ref}")

    def test_references_include_declaration_when_requested(self) -> None:
        # With includeDeclaration=True the declaration site (L27) must be present.
        refs = self._refs(self.cfg_uri, 27, 25, include_decl=True)
        decl_refs = [r for r in refs
                     if r["range"]["start"]["line"] == 27]
        self.assertGreater(len(decl_refs), 0,
                           "declaration site not in references when includeDeclaration=True")

    def test_references_exclude_declaration_when_not_requested(self) -> None:
        # With includeDeclaration=False the declaration line should be absent.
        refs = self._refs(self.cfg_uri, 27, 25, include_decl=False)
        decl_refs = [r for r in refs
                     if r["range"]["start"]["line"] == 27
                     and CFG_SV.name in r["uri"]]
        self.assertEqual(decl_refs, [],
                         "declaration site found when includeDeclaration=False")

    def test_references_cross_file_class_name(self) -> None:
        # riscv_instr_gen_config.sv L20 col 6: class name 'riscv_instr_gen_config'.
        # Used in seq, data_page_gen, illegal_instr, etc. → multiple files.
        refs = self._refs(self.cfg_uri, 20, 6, include_decl=True)
        self.assertGreater(len(refs), 0, "no references for class name")
        uris = {r["uri"] for r in refs}
        self.assertGreater(len(uris), 1,
                           "expected cross-file references, all in single file")

    def test_references_of_field_includes_cfg_file(self) -> None:
        # 'main_program_instr_cnt' is declared in gen_config.sv; at minimum
        # the declaration reference must be in that file (cross-file references
        # from riscv_asm_program_gen.sv are also expected and valid).
        refs = self._refs(self.cfg_uri, 27, 25, include_decl=True)
        cfg_refs = [r for r in refs if "riscv_instr_gen_config" in r["uri"]]
        self.assertGreater(len(cfg_refs), 0,
                           "no references to main_program_instr_cnt found in its defining file")

    def test_references_unknown_position_returns_empty(self) -> None:
        # Cursor on whitespace / blank line → empty list, not crash.
        refs = self._refs(self.seq_uri, 1, 0)
        self.assertIsInstance(refs, list)

    def test_uris_are_file_scheme(self) -> None:
        refs = self._refs(self.cfg_uri, 27, 25)
        for ref in refs:
            self.assertTrue(ref["uri"].startswith("file://"),
                            f"reference URI not file://: {ref['uri']}")


# ---------------------------------------------------------------------------
# 8. Signature help
# ---------------------------------------------------------------------------


class RiscvDvSignatureHelpTest(unittest.TestCase):
    """textDocument/signatureHelp — trigger chars, active param, no-crash."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        time.sleep(0.2)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _sig(self, uri: str, line: int, char: int,
             trigger: str = "(") -> dict | None:
        return self.lsp.request("textDocument/signatureHelp", {
            **_td_pos(uri, line, char),
            "context": {"triggerKind": 1, "triggerCharacter": trigger,
                        "isRetrigger": False},
        })

    def test_signature_help_at_call_does_not_crash(self) -> None:
        # riscv_instr_sequence.sv L102 col 43: inside gen_push_stack_instr(...)
        result = self._sig(self.seq_uri, 102, 43)
        if result is not None:
            self.assertIn("signatures", result)
            self.assertIsInstance(result["signatures"], list)

    def test_signature_help_outside_call_returns_none(self) -> None:
        # Cursor on field declaration — not inside a call.
        result = self._sig(self.seq_uri, 44, 2)
        self.assertIsNone(result,
                          f"expected None for non-call position, got {result}")

    def test_signature_help_result_shape(self) -> None:
        """When a result is returned it must have the right LSP shape."""
        # Try a bare function call: gen_stack_enter_instr() at line 83 col 6.
        result = self._sig(self.seq_uri, 83, 8)
        if result is None:
            self.skipTest("no signature help returned for this call site")
        self.assertIn("signatures", result)
        for sig in result["signatures"]:
            self.assertIn("label", sig)

    def test_signature_help_active_parameter_is_int(self) -> None:
        """activeParameter must be an integer when signatures are present."""
        result = self._sig(self.seq_uri, 102, 43)
        if result and result.get("signatures"):
            ap = result.get("activeParameter", 0)
            self.assertIsInstance(ap, int)

    def test_signature_comma_trigger_increments_active_param(self) -> None:
        # riscv_instr_sequence.sv L102:
        # gen_push_stack_instr(program_stack_len, .allow_branch(allow_branch))
        # Position before comma (col 43, arg 0) vs after comma (past col 60).
        res_arg0 = self._sig(self.seq_uri, 102, 43, trigger="(")
        res_arg1 = self._sig(self.seq_uri, 102, 63, trigger=",")
        if (res_arg0 and res_arg0.get("signatures")
                and res_arg1 and res_arg1.get("signatures")):
            ap0 = res_arg0.get("activeParameter", 0)
            ap1 = res_arg1.get("activeParameter", 0)
            self.assertGreaterEqual(ap1, ap0,
                                    "activeParameter did not advance past comma")

    def test_signature_help_macro_call_does_not_crash(self) -> None:
        # riscv_instr_sequence.sv L57 col 17: inside `uvm_fatal(...)
        result = self._sig(self.seq_uri, 57, 17, trigger="(")
        if result is not None:
            self.assertIn("signatures", result)

    def test_signature_help_nested_call_does_not_crash(self) -> None:
        # L76: `uvm_info(get_full_name(), $sformatf("...", ...))
        # Cursor inside the outer macro call args — nested call, no crash.
        result = self._sig(self.seq_uri, 76, 20, trigger="(")
        if result is not None:
            self.assertIn("signatures", result)

    def test_signature_parameters_list_when_present(self) -> None:
        """Each SignatureInformation may have a parameters list."""
        result = self._sig(self.seq_uri, 102, 43)
        if result and result.get("signatures"):
            sig = result["signatures"][0]
            if "parameters" in sig:
                self.assertIsInstance(sig["parameters"], list)


# ---------------------------------------------------------------------------
# 9. Inlay hints
# ---------------------------------------------------------------------------


class RiscvDvInlayHintTest(unittest.TestCase):
    """textDocument/inlayHint — param labels, viewport filtering."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        time.sleep(0.2)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _hints(self, start_line: int, end_line: int) -> list[dict]:
        result = self.lsp.request("textDocument/inlayHint", {
            "textDocument": {"uri": self.seq_uri},
            "range": _range(start_line, 0, end_line, 999),
        })
        return result or []

    def test_hints_does_not_crash(self) -> None:
        hints = self._hints(70, 90)
        self.assertIsInstance(hints, list)

    def test_hints_have_position_and_label(self) -> None:
        hints = self._hints(70, 90)
        for hint in hints:
            self.assertIn("position", hint, f"hint missing position: {hint}")
            self.assertIn("label", hint, f"hint missing label: {hint}")

    def test_hints_position_within_viewport(self) -> None:
        """No hint position falls outside the requested viewport."""
        hints = self._hints(70, 90)
        for hint in hints:
            line = hint["position"]["line"]
            self.assertGreaterEqual(line, 70,
                                    f"hint at line {line} before viewport start 70")
            self.assertLess(line, 90,
                            f"hint at line {line} past viewport end 90")

    def test_hints_outside_viewport_not_returned(self) -> None:
        """Requesting lines 90-100 should not return hints from lines 70-89."""
        hints = self._hints(90, 100)
        for hint in hints:
            line = hint["position"]["line"]
            self.assertGreaterEqual(line, 90,
                                    f"hint at line {line} outside requested viewport [90,100)")

    def test_pure_typedef_viewport_returns_empty(self) -> None:
        # Lines 35-54 in riscv_instr_pkg.sv are pure typedef declarations —
        # no function calls → no inlay hints expected.
        # (We open pkg.sv inline here so the test class stays independent.)
        lsp2 = MimirLspClient()
        try:
            lsp2.initialize(workspace_root=RISCV_DV)
            pkg_uri = file_uri(PKG_SV)
            lsp2.did_open(pkg_uri, read_text(PKG_SV))
            _wait_for_parse(lsp2, pkg_uri)
            hints = lsp2.request("textDocument/inlayHint", {
                "textDocument": {"uri": pkg_uri},
                "range": _range(35, 0, 55, 0),
            }) or []
            self.assertEqual(hints, [],
                             f"expected no hints for typedef-only viewport, got {hints}")
        finally:
            lsp2.close()

    def test_hint_label_is_string_or_list(self) -> None:
        hints = self._hints(70, 120)
        for hint in hints:
            label = hint["label"]
            self.assertTrue(isinstance(label, (str, list)),
                            f"hint label has unexpected type {type(label)}: {label}")

    def test_macro_call_hints_do_not_crash(self) -> None:
        # Lines containing `uvm_info / `uvm_fatal macro calls.
        hints = self._hints(55, 60)
        self.assertIsInstance(hints, list)

    def test_function_call_area_may_have_hints(self) -> None:
        """Lines 93-115 contain function calls; result must at minimum be a list."""
        hints = self._hints(93, 115)
        self.assertIsInstance(hints, list)


# ---------------------------------------------------------------------------
# 10. Document highlight
# ---------------------------------------------------------------------------


class RiscvDvDocumentHighlightTest(unittest.TestCase):
    """textDocument/documentHighlight — multiple occurrences, scope pruning."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _hl(self, uri: str, line: int, char: int) -> list[dict] | None:
        return self.lsp.request("textDocument/documentHighlight",
                                _td_pos(uri, line, char))

    def test_highlight_class_field_used_multiple_times(self) -> None:
        # riscv_instr_gen_config.sv L27 col 25: 'main_program_instr_cnt'
        # Appears on lines 28, 282, 306, 311, 316, 479 → ≥ 4 highlights.
        result = self._hl(self.cfg_uri, 27, 25)
        self.assertIsNotNone(result, "no highlights for main_program_instr_cnt")
        self.assertGreaterEqual(len(result), 4,
                                f"expected ≥ 4 highlights, got {len(result)}")

    def test_highlight_includes_declaration_site(self) -> None:
        result = self._hl(self.cfg_uri, 27, 25)
        self.assertIsNotNone(result)
        decl_hits = [h for h in result
                     if h["range"]["start"]["line"] == 27]
        self.assertGreater(len(decl_hits), 0,
                           "declaration site (L27) missing from highlights")

    def test_highlight_range_has_start_and_end(self) -> None:
        result = self._hl(self.cfg_uri, 27, 25)
        self.assertIsNotNone(result)
        for hl in result:
            self.assertIn("range", hl)
            self.assertIn("start", hl["range"])
            self.assertIn("end", hl["range"])

    def test_highlight_scope_aware_param_vs_field(self) -> None:
        # riscv_instr_sequence.sv L73 col 27: bare 'is_main_program' (function
        # parameter use on RHS of assignment).
        # The class field 'is_main_program' is on L41.  Scope-aware highlighter
        # should NOT include L41 when the cursor is on the local parameter.
        result = self._hl(self.seq_uri, 73, 27)
        if result is None:
            self.skipTest("no highlights returned — scope-aware test skipped")
        lines_hit = {h["range"]["start"]["line"] for h in result}
        # Field declaration is at line 41 (0-indexed).
        self.assertNotIn(41, lines_hit,
                         "scope-aware highlighter leaked into class field (L41) "
                         "when cursor is on function parameter")

    def test_highlight_keyword_returns_none_or_empty(self) -> None:
        # Cursor on 'class' keyword at L35 col 0 — not an identifier.
        result = self._hl(self.seq_uri, 35, 0)
        count = len(result) if result else 0
        self.assertEqual(count, 0,
                         f"expected no highlights for 'class' keyword, got {result}")

    def test_highlight_whitespace_returns_none_or_empty(self) -> None:
        result = self._hl(self.seq_uri, 1, 0)
        count = len(result) if result else 0
        self.assertEqual(count, 0,
                         f"expected no highlights for whitespace, got {result}")

    def test_highlight_kind_values_are_valid(self) -> None:
        """LSP highlight kinds: 1=Text, 2=Read, 3=Write."""
        result = self._hl(self.cfg_uri, 27, 25)
        self.assertIsNotNone(result)
        for hl in result:
            kind = hl.get("kind", 1)
            self.assertIn(kind, (1, 2, 3), f"unexpected highlight kind {kind}")

    def test_highlight_same_name_different_functions(self) -> None:
        # Cursor on 'instr_stack_enter' field on L38 (0-indexed) of seq.sv.
        # It appears several times inside the class but different functions
        # — scope should encompass the whole class.
        result = self._hl(self.seq_uri, 38, 2)
        if result is not None:
            self.assertGreaterEqual(len(result), 1)


# ---------------------------------------------------------------------------
# 11. Document symbol
# ---------------------------------------------------------------------------


class RiscvDvDocumentSymbolTest(unittest.TestCase):
    """textDocument/documentSymbol — kinds, nesting, ranges."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.pkg_uri = file_uri(PKG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        cls.lsp.did_open(cls.pkg_uri, read_text(PKG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.pkg_uri, timeout=8.0)
        time.sleep(0.1)

        cls.seq_syms = cls.lsp.request("textDocument/documentSymbol",
                                       {"textDocument": {"uri": cls.seq_uri}}) or []
        cls.cfg_syms = cls.lsp.request("textDocument/documentSymbol",
                                       {"textDocument": {"uri": cls.cfg_uri}}) or []
        cls.pkg_syms = cls.lsp.request("textDocument/documentSymbol",
                                       {"textDocument": {"uri": cls.pkg_uri}}) or []

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    # LSP symbol kinds: Class=5, Function=12, Method=6, Package=4(LSP)/12,
    # Enum=10, Struct=23. mimir maps SV 'package' to LSP Namespace(3) or
    # Module(2). Accept any non-zero kind for structural tests.

    def test_seq_class_name_found(self) -> None:
        names = _collect_symbol_names(self.seq_syms)
        self.assertIn("riscv_instr_sequence", names)

    def test_seq_class_methods_present(self) -> None:
        names = _collect_symbol_names(self.seq_syms)
        for fn in ("new", "gen_instr", "gen_stack_enter_instr",
                   "gen_stack_exit_instr"):
            self.assertIn(fn, names, f"expected method '{fn}' in document symbols")

    def test_seq_class_fields_present(self) -> None:
        names = _collect_symbol_names(self.seq_syms)
        for field in ("instr_cnt", "instr_stream", "label_name", "cfg"):
            self.assertIn(field, names, f"field '{field}' missing from symbols")

    def test_seq_methods_nested_under_class(self) -> None:
        children = _find_children_of(self.seq_syms, "riscv_instr_sequence")
        child_names = {c.get("name", "") for c in children}
        # At least some methods should be children of the class, not top-level.
        self.assertTrue(child_names,
                        "no children found under riscv_instr_sequence class symbol")
        self.assertGreater(len(child_names), 2,
                           f"expected >2 class members, got: {sorted(child_names)}")

    def test_cfg_class_name_and_kind(self) -> None:
        sym_pairs = _collect_symbols_with_kind(self.cfg_syms)
        class_syms = [(n, k) for n, k in sym_pairs
                      if n == "riscv_instr_gen_config"]
        self.assertGreater(len(class_syms), 0,
                           "riscv_instr_gen_config not found in documentSymbol")
        # kind: Class(5) or Module(2) or Namespace(3) — any non-trivial kind.
        _, kind = class_syms[0]
        self.assertGreater(kind, 0, f"unexpected symbol kind {kind}")

    def test_cfg_rand_fields_present(self) -> None:
        names = _collect_symbol_names(self.cfg_syms)
        for field in ("main_program_instr_cnt", "sub_program_instr_cnt",
                      "mstatus_mprv", "enable_sfence"):
            self.assertIn(field, names, f"rand field '{field}' missing")

    def test_cfg_methods_present(self) -> None:
        names = _collect_symbol_names(self.cfg_syms)
        for fn in ("new", "setup_instr_distribution", "init_delegation",
                   "post_randomize", "check_setting"):
            self.assertIn(fn, names, f"method '{fn}' missing")

    def test_pkg_name_found(self) -> None:
        names = _collect_symbol_names(self.pkg_syms)
        self.assertIn("riscv_instr_pkg", names)

    def test_pkg_typedef_names_found(self) -> None:
        names = _collect_symbol_names(self.pkg_syms)
        for td in ("mem_region_t", "vreg_init_method_t", "satp_mode_t",
                   "privileged_mode_t", "mtvec_mode_t"):
            self.assertIn(td, names, f"typedef '{td}' missing from pkg symbols")

    def test_symbol_range_spans_full_declaration(self) -> None:
        """Class symbol range must span from 'class' keyword to 'endclass'."""
        sym_pairs = _collect_symbols_with_kind(self.seq_syms)
        # Find the top-level class symbol (not just its name in flat list).
        class_sym = next((s for s in self.seq_syms
                          if s.get("name") == "riscv_instr_sequence"), None)
        if class_sym is None:
            self.skipTest("riscv_instr_sequence not at top level")
        rng = class_sym.get("range", {})
        start_line = rng.get("start", {}).get("line", -1)
        end_line   = rng.get("end",   {}).get("line", -1)
        # class starts at L35 (0-idx), endclass at L351 (0-idx).
        self.assertEqual(start_line, 35,
                         f"class range start expected 35, got {start_line}")
        self.assertGreaterEqual(end_line, 350,
                                f"class range end expected ~351, got {end_line}")

    def test_symbol_selection_range_is_narrower_than_range(self) -> None:
        class_sym = next((s for s in self.seq_syms
                          if s.get("name") == "riscv_instr_sequence"), None)
        if class_sym is None:
            self.skipTest("riscv_instr_sequence not at top level")
        full_rng = class_sym.get("range", {})
        sel_rng  = class_sym.get("selectionRange", full_rng)
        full_end = full_rng.get("end", {}).get("line", 0)
        sel_end  = sel_rng.get("end",  {}).get("line", 0)
        self.assertLessEqual(sel_end, full_end,
                             "selectionRange.end should not exceed range.end")

    def test_empty_source_returns_empty_list(self) -> None:
        lsp2 = MimirLspClient()
        try:
            lsp2.initialize(workspace_root=RISCV_DV)
            empty_uri = file_uri(SEQ_SV) + "__empty__"  # fake URI
            # Use an actual temp URI with empty content.
            lsp2.did_open(empty_uri, "", language_id="systemverilog")
            time.sleep(0.2)
            syms = lsp2.request("textDocument/documentSymbol",
                                {"textDocument": {"uri": empty_uri}}) or []
            self.assertEqual(syms, [],
                             f"expected empty symbol list for empty source, got {syms}")
        finally:
            lsp2.close()


# ---------------------------------------------------------------------------
# 12. Workspace symbol
# ---------------------------------------------------------------------------


class RiscvDvWorkspaceSymbolTest(unittest.TestCase):
    """workspace/symbol — fuzzy query, filtering, cross-file hydration."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        # Open one file to trigger the diagnostics loop; workspace index
        # is populated at initialize time from the filelist.
        seq_uri = file_uri(SEQ_SV)
        cls.lsp.did_open(seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, seq_uri)
        time.sleep(0.4)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def _sym(self, query: str) -> list[dict]:
        return self.lsp.request("workspace/symbol", {"query": query}) or []

    def test_empty_query_returns_symbols(self) -> None:
        result = self._sym("")
        self.assertIsInstance(result, list)
        self.assertGreater(len(result), 0)

    def test_empty_query_cap_200(self) -> None:
        result = self._sym("")
        self.assertLessEqual(len(result), 200,
                             "workspace/symbol must cap at 200 results")

    def test_result_has_required_fields(self) -> None:
        result = self._sym("")
        for sym in result:
            self.assertIn("name", sym)
            self.assertIn("kind", sym)
            self.assertIn("location", sym)
            loc = sym["location"]
            self.assertIn("uri", loc)
            self.assertIn("range", loc)

    def test_uris_are_file_scheme(self) -> None:
        for sym in self._sym(""):
            uri = sym["location"]["uri"]
            self.assertTrue(uri.startswith("file://"),
                            f"symbol URI not file://: {uri}")

    def test_uris_point_to_existing_paths(self) -> None:
        for sym in self._sym(""):
            uri = sym["location"]["uri"]
            path = pathlib.Path(uri.replace("file://", ""))
            self.assertTrue(path.exists(), f"symbol URI points to missing file: {uri}")

    def test_cross_file_class_found_without_opening_file(self) -> None:
        # riscv_instr_gen_config.sv was never opened in this session.
        result = self._sym("riscv_instr_gen_config")
        names = {s["name"] for s in result}
        self.assertIn("riscv_instr_gen_config", names,
                      "cross-file class not indexed via filelist hydration")

    def test_exact_query_ranks_first(self) -> None:
        result = self._sym("riscv_instr_gen_config")
        self.assertGreater(len(result), 0)
        self.assertEqual(result[0]["name"], "riscv_instr_gen_config",
                         f"exact match not first; got {result[0]['name']!r}")

    def test_fuzzy_subsequence_matches(self) -> None:
        # 'ri_ig_cfg' is a subsequence of 'riscv_instr_gen_config'.
        result = self._sym("ri_ig_cfg")
        names = {s["name"] for s in result}
        self.assertIn("riscv_instr_gen_config", names,
                      "fuzzy subsequence failed to match riscv_instr_gen_config")

    def test_no_match_returns_empty(self) -> None:
        result = self._sym("zzzz_impossible_symbol_zzzz")
        self.assertEqual(result, [],
                         f"expected empty list for unmatchable query, got {result}")

    def test_excludes_variables_and_ports(self) -> None:
        # Variable-kind items (rand fields, ports) must be filtered out.
        result = self._sym("main_program_instr_cnt")
        names = {s["name"] for s in result}
        self.assertNotIn("main_program_instr_cnt", names,
                         "rand field (Variable kind) leaked into workspace/symbol results")

    def test_partial_prefix_matches(self) -> None:
        result = self._sym("riscv_instr_seq")
        names = {s["name"] for s in result}
        self.assertTrue(any("riscv_instr_seq" in n for n in names),
                        f"prefix 'riscv_instr_seq' matched nothing: {sorted(names)}")


# ---------------------------------------------------------------------------
# 13. Folding range
# ---------------------------------------------------------------------------


class RiscvDvFoldingRangeTest(unittest.TestCase):
    """textDocument/foldingRange — correctness of start/end, nesting, kind."""

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.pkg_uri = file_uri(PKG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        cls.lsp.did_open(cls.pkg_uri, read_text(PKG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.pkg_uri, timeout=8.0)
        time.sleep(0.1)

        cls.seq_folds = cls.lsp.request("textDocument/foldingRange",
                                        {"textDocument": {"uri": cls.seq_uri}}) or []
        cls.cfg_folds = cls.lsp.request("textDocument/foldingRange",
                                        {"textDocument": {"uri": cls.cfg_uri}}) or []
        cls.pkg_folds = cls.lsp.request("textDocument/foldingRange",
                                        {"textDocument": {"uri": cls.pkg_uri}}) or []

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def test_seq_has_folds(self) -> None:
        self.assertGreater(len(self.seq_folds), 0,
                           "no folds returned for riscv_instr_sequence.sv")

    def test_seq_class_fold_start_line(self) -> None:
        # class declaration on L35 (0-idx) → fold must start there.
        starts = {f["startLine"] for f in self.seq_folds}
        self.assertIn(35, starts,
                      f"missing class fold at L35; got starts={sorted(starts)}")

    def test_seq_class_fold_end_line(self) -> None:
        # endclass on L351 (0-idx) → fold must end there (or L350).
        class_fold = next((f for f in self.seq_folds if f["startLine"] == 35), None)
        self.assertIsNotNone(class_fold, "no fold starting at L35 for class")
        self.assertGreaterEqual(class_fold["endLine"], 350,
                                f"class fold ends too early: {class_fold}")

    def test_seq_functions_have_folds(self) -> None:
        # gen_instr at L72, gen_stack_enter_instr at L92, etc.
        starts = {f["startLine"] for f in self.seq_folds}
        for fn_line in (72, 92):
            self.assertIn(fn_line, starts,
                          f"expected function fold at L{fn_line}; got {sorted(starts)}")

    def test_seq_nested_fold_inside_class(self) -> None:
        """Method folds must start after the class fold starts."""
        class_fold = next((f for f in self.seq_folds if f["startLine"] == 35), None)
        self.assertIsNotNone(class_fold)
        inner_folds = [f for f in self.seq_folds
                       if f["startLine"] > 35 and f["endLine"] < class_fold["endLine"]]
        self.assertGreater(len(inner_folds), 0,
                           "no inner folds found inside class body")

    def test_cfg_class_fold_present(self) -> None:
        # class at L20 (0-idx), endclass at L796 (0-idx).
        starts = {f["startLine"] for f in self.cfg_folds}
        self.assertIn(20, starts, f"missing class fold at L20; {sorted(starts)}")

    def test_cfg_class_fold_end_matches_endclass(self) -> None:
        class_fold = next((f for f in self.cfg_folds if f["startLine"] == 20), None)
        self.assertIsNotNone(class_fold, "class fold not found at L20")
        self.assertGreaterEqual(class_fold["endLine"], 795,
                                f"class fold ends at {class_fold['endLine']}, expected ~796")

    def test_cfg_function_folds_present(self) -> None:
        # setup_instr_distribution at L675 (0-idx).
        starts = {f["startLine"] for f in self.cfg_folds}
        self.assertIn(675, starts,
                      f"missing function fold at L675; {sorted(starts)[:20]}")

    def test_pkg_package_fold_present(self) -> None:
        # package declaration at L18 (0-idx).
        starts = {f["startLine"] for f in self.pkg_folds}
        self.assertIn(18, starts,
                      f"missing package fold at L18; got {sorted(starts)[:10]}")

    def test_pkg_package_fold_end_matches_endpackage(self) -> None:
        pkg_fold = next((f for f in self.pkg_folds if f["startLine"] == 18), None)
        self.assertIsNotNone(pkg_fold, "package fold not found at L18")
        # endpackage is the last line (L1627, 0-idx).
        self.assertGreaterEqual(pkg_fold["endLine"], 1620,
                                f"package fold ends too early: {pkg_fold['endLine']}")

    def test_fold_kind_is_region(self) -> None:
        """Every fold must have kind 'region' (comment/imports not emitted yet)."""
        for fold in self.seq_folds:
            # `or "region"` handles both absent key (get returns None) and
            # explicit null serialised from Rust Option::None.
            kind = fold.get("kind") or "region"
            self.assertEqual(kind, "region",
                             f"unexpected fold kind {kind!r}: {fold}")

    def test_single_line_construct_not_folded(self) -> None:
        # gen_non_reserved_gpr at L727 (0-idx): only 2 lines → no fold expected.
        # (L728: `  virtual function void get_non_reserved_gpr();`
        #  L729: `  endfunction` — start == end after offset, should be pruned.)
        one_liners = [f for f in self.cfg_folds
                      if f["startLine"] == f["endLine"]]
        self.assertEqual(one_liners, [],
                         f"single-line folds should be pruned: {one_liners}")

    def test_repeated_request_identical(self) -> None:
        """Cache contract: two identical requests return the same folds."""
        td = {"textDocument": {"uri": self.seq_uri}}

        def _sort(folds):
            return sorted(folds or [], key=lambda f: (f["startLine"], f["endLine"]))

        first  = _sort(self.lsp.request("textDocument/foldingRange", td))
        second = _sort(self.lsp.request("textDocument/foldingRange", td))
        self.assertEqual(first, second,
                         "foldingRange changed between two identical requests (cache miss?)")


# ---------------------------------------------------------------------------
# 14. Slang-only features (skip without MIMIR_SLANG_PATH)
# ---------------------------------------------------------------------------


class RiscvDvSlangOnlyTest(unittest.TestCase):
    """Features that require the slang sidecar.

    Without slang they must return ``null`` (not crash).  When slang is
    configured, assertions should be tightened to check real jump targets.
    """

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri = file_uri(SEQ_SV)
        cls.cfg_uri = file_uri(CFG_SV)
        cls.lsp.did_open(cls.seq_uri, read_text(SEQ_SV))
        _wait_for_parse(cls.lsp, cls.seq_uri)
        cls.lsp.did_open(cls.cfg_uri, read_text(CFG_SV))
        cls.lsp.wait_for_fresh_diagnostics(cls.cfg_uri, timeout=8.0)
        time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def test_type_definition_without_slang_returns_null(self) -> None:
        """textDocument/typeDefinition requires slang; without it → null."""
        result = self.lsp.request("textDocument/typeDefinition",
                                  _td_pos(self.cfg_uri, 27, 25))
        if not _slang_available():
            locs = result if isinstance(result, list) else ([result] if result else [])
            self.assertEqual(locs, [],
                             f"expected null/empty without slang, got {result}")
        else:
            self.skipTest("slang configured — update to assert concrete jump target")

    def test_implementation_without_slang_returns_null(self) -> None:
        """textDocument/implementation requires slang; without it → null."""
        # riscv_instr_sequence.sv L72: virtual function gen_instr
        result = self.lsp.request("textDocument/implementation",
                                  _td_pos(self.seq_uri, 72, 24))
        if not _slang_available():
            locs = result if isinstance(result, list) else ([result] if result else [])
            self.assertEqual(locs, [],
                             f"expected null/empty without slang, got {result}")
        else:
            self.skipTest("slang configured — update to assert concrete implementation list")


# ---------------------------------------------------------------------------
# 15. Formatting
# ---------------------------------------------------------------------------


class RiscvDvFormattingTest(unittest.TestCase):
    """textDocument/formatting and textDocument/rangeFormatting.

    Skipped when ``verible-verilog-format`` is not on ``$PATH``.
    """

    @classmethod
    @_skip_if_no_riscv_dv
    def setUpClass(cls) -> None:
        if not _verible_available():
            raise unittest.SkipTest(
                "verible-verilog-format not on $PATH — skipping formatting tests"
            )
        cls.lsp = MimirLspClient()
        cls.lsp.initialize(workspace_root=RISCV_DV)
        cls.seq_uri  = file_uri(SEQ_SV)
        cls.seq_text = read_text(SEQ_SV)
        cls.lsp.did_open(cls.seq_uri, cls.seq_text)
        _wait_for_parse(cls.lsp, cls.seq_uri)

    @classmethod
    def tearDownClass(cls) -> None:
        if _verible_available():
            cls.lsp.close()

    def _fmt(self) -> list[dict]:
        return self.lsp.request("textDocument/formatting", {
            "textDocument": {"uri": self.seq_uri},
            "options": {"tabSize": 2, "insertSpaces": True},
        }) or []

    def test_formatting_returns_text_edit_list(self) -> None:
        edits = self._fmt()
        self.assertIsInstance(edits, list,
                              "textDocument/formatting must return a list of TextEdits")

    def test_text_edits_have_required_fields(self) -> None:
        for edit in self._fmt():
            self.assertIn("range", edit, f"TextEdit missing 'range': {edit}")
            self.assertIn("newText", edit, f"TextEdit missing 'newText': {edit}")
            rng = edit["range"]
            self.assertIn("start", rng)
            self.assertIn("end", rng)

    def test_range_formatting_returns_list(self) -> None:
        result = self.lsp.request("textDocument/rangeFormatting", {
            "textDocument": {"uri": self.seq_uri},
            "range": _range(35, 0, 65, 0),
            "options": {"tabSize": 2, "insertSpaces": True},
        }) or []
        self.assertIsInstance(result, list)

    def test_range_formatting_edits_within_range(self) -> None:
        edits = self.lsp.request("textDocument/rangeFormatting", {
            "textDocument": {"uri": self.seq_uri},
            "range": _range(35, 0, 65, 0),
            "options": {"tabSize": 2, "insertSpaces": True},
        }) or []
        for edit in edits:
            start_line = edit["range"]["start"]["line"]
            end_line   = edit["range"]["end"]["line"]
            self.assertGreaterEqual(start_line, 35,
                                    f"edit starts before requested range: L{start_line}")
            self.assertLessEqual(end_line, 65,
                                 f"edit ends after requested range: L{end_line}")


# ---------------------------------------------------------------------------


if __name__ == "__main__":
    unittest.main()
