"""End-to-end regression tests: one per bug, driven through the real server.

Every test here reproduces a failure that was seen in the field or found in
review — a crash, a hang, or a wrong answer — by speaking LSP to the release
binary exactly as an editor does. Unit-level regressions live next to the
code they cover (``regression_*`` tests in the Rust crates); the tests in this
file are the ones that need the whole server: request interleaving, the
document store, the project loader, the sidecar plumbing.

The suite is **hermetic**: it builds its own throw-away workspaces and uses
``tests/fake_sidecar.py`` / ``tests/fake_formatter.py`` in place of slang and
Verible, so it runs on a bare checkout.

Run:

    cargo build --release -p mimir-server
    python3 -m unittest tests.test_regressions -v
"""

from __future__ import annotations

import json
import pathlib
import select
import tempfile
import time
import unittest

from .lsp_client import MimirLspClient, file_uri


TESTS_DIR = pathlib.Path(__file__).resolve().parent
FAKE_SIDECAR = TESTS_DIR / "fake_sidecar.py"
FAKE_FORMATTER = TESTS_DIR / "fake_formatter.py"


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------


def _pos(line: int, character: int) -> dict:
    return {"line": line, "character": character}


def _range(sl: int, sc: int, el: int, ec: int) -> dict:
    return {"start": _pos(sl, sc), "end": _pos(el, ec)}


def _symbol_names(symbols: list[dict] | None) -> list[str]:
    """Flatten a nested ``documentSymbol`` response into a list of names."""
    out: list[str] = []
    for sym in symbols or []:
        out.append(sym["name"])
        out.extend(_symbol_names(sym.get("children")))
    return out


def _apply_edits(text: str, edits: list[dict]) -> str:
    """Apply LSP ``TextEdit``s to ``text`` the way an editor does.

    ASCII fixtures only, so UTF-16 columns equal string indices.
    """
    lines = text.split("\n")

    def offset(p: dict) -> int:
        line = min(p["line"], len(lines) - 1)
        before = sum(len(l) + 1 for l in lines[:line])
        if p["line"] > line:  # past the last line ⇒ end of document
            return len(text)
        return before + min(p["character"], len(lines[line]))

    # Back to front so earlier offsets stay valid.
    for e in sorted(edits, key=lambda e: (e["range"]["start"]["line"],
                                           e["range"]["start"]["character"]),
                    reverse=True):
        start, end = offset(e["range"]["start"]), offset(e["range"]["end"])
        text = text[:start] + e["newText"] + text[end:]
        lines = text.split("\n")
    return text


def _change(lsp: MimirLspClient, uri: str, version: int, rng: dict | None, text: str) -> None:
    change: dict = {"text": text}
    if rng is not None:
        change["range"] = rng
    lsp.notify(
        "textDocument/didChange",
        {"textDocument": {"uri": uri, "version": version}, "contentChanges": [change]},
    )


class _Workspace:
    """A throw-away project directory with an optional ``.mimir.toml``."""

    def __init__(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="mimir-regress-")
        # `resolve()` so paths match what the server derives from URIs even
        # when the temp dir sits behind a symlink (macOS `/var` → `/private`).
        self.root = pathlib.Path(self._tmp.name).resolve()

    def write(self, rel: str, content: str | bytes) -> pathlib.Path:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, bytes):
            path.write_bytes(content)
        else:
            path.write_text(content, encoding="utf-8")
        return path

    def uri(self, rel: str) -> str:
        return file_uri(self.root / rel)

    def cleanup(self) -> None:
        self._tmp.cleanup()


# ---------------------------------------------------------------------------
# Crashes and wrong answers that need no sidecar
# ---------------------------------------------------------------------------


class ServerRegressionTest(unittest.TestCase):
    """Tree-sitter-only regressions (no sidecar configured)."""

    def setUp(self) -> None:
        self.ws = _Workspace()
        self.addCleanup(self.ws.cleanup)

    def _start(self, env: dict[str, str] | None = None) -> MimirLspClient:
        # An empty `MIMIR_SLANG_PATH` keeps a sidecar configured in the
        # developer's shell out of these tree-sitter-only tests.
        lsp = MimirLspClient(env={"MIMIR_SLANG_PATH": "", **(env or {})})
        self.addCleanup(lsp.close)
        lsp.initialize(workspace_root=self.ws.root)
        return lsp

    def _assert_alive(self, lsp: MimirLspClient, uri: str) -> list[str]:
        """The server must still answer; returns the document's symbol names."""
        try:
            symbols = lsp.request(
                "textDocument/documentSymbol", {"textDocument": {"uri": uri}}, timeout=60.0
            )
        except Exception as exc:  # stdout closed ⇒ the server died
            self.fail(f"server stopped responding: {exc}\n--- stderr ---\n{lsp.stderr_text[-4000:]}")
        return _symbol_names(symbols)

    # -- crashes -----------------------------------------------------------

    def test_syntax_error_next_to_non_ascii_text_does_not_crash(self) -> None:
        """A parse error whose snippet is cut inside a multi-byte character
        (non-ASCII string next to a typo) used to panic the diagnostics
        pass and take the server down on a keystroke."""
        lsp = self._start()
        uri = self.ws.uri("a.sv")
        for shift in range(4):
            text = (
                "module m;\n"
                f'  int x = "{"x" * shift}{"é" * 47}" "ééé" ééé;\n'
                "endmodule\n"
            )
            if shift == 0:
                lsp.did_open(uri, text)
            else:
                _change(lsp, uri, shift + 1, None, text)
            diag = lsp.wait_for_fresh_diagnostics(uri, timeout=10.0)
            self.assertIsNotNone(diag, "no diagnostics — did the server crash?\n"
                                 + lsp.stderr_text[-2000:])
            self.assertTrue(diag["diagnostics"], "the broken line must be reported")
        self.assertIn("m", self._assert_alive(lsp, uri))

    def test_deeply_nested_expression_does_not_overflow_the_stack(self) -> None:
        """A very long `a + a + a + …` chain parses into a tree tens of
        thousands of levels deep. Recursive tree walkers overflowed the
        native stack on it — an abort, not even a panic."""
        lsp = self._start()
        uri = self.ws.uri("deep.sv")
        text = "module deep;\n  assign x = a" + " + a" * 60_000 + ";\nendmodule\n"
        lsp.did_open(uri, text)
        self.assertIsNotNone(
            lsp.wait_for_fresh_diagnostics(uri, timeout=60.0),
            "no diagnostics — did the server crash?\n" + lsp.stderr_text[-2000:],
        )
        self.assertIn("deep", self._assert_alive(lsp, uri))
        td = {"textDocument": {"uri": uri}}
        for method, params in (
            ("textDocument/foldingRange", td),
            ("textDocument/semanticTokens/full", td),
            ("textDocument/documentHighlight", {**td, "position": _pos(1, 13)}),
            ("textDocument/hover", {**td, "position": _pos(1, 13)}),
            ("textDocument/selectionRange", {**td, "positions": [_pos(1, 13)]}),
            ("textDocument/inlayHint", {**td, "range": _range(0, 0, 3, 0)}),
        ):
            try:
                lsp.request(method, params, timeout=120.0)
            except Exception as exc:
                self.fail(f"{method} killed the server: {exc}\n{lsp.stderr_text[-2000:]}")
        self.assertIn("deep", self._assert_alive(lsp, uri))

    def test_edits_during_workspace_indexing_do_not_corrupt_the_tree(self) -> None:
        """Typing while the workspace is being (re-)indexed: the indexer
        held the parser, so the re-parses for those keystrokes queued up and
        then each patched the *same* stale tree with only its own edit. The
        result was a tree that didn't match the text — wrong outline at
        best, a slice-out-of-range panic at worst."""
        # Enough project files that indexing takes a noticeable while.
        body = "".join(
            f"  function automatic int f{i}(int a, int b);\n"
            f"    return a + b + {i};\n"
            f"  endfunction\n"
            for i in range(400)
        )
        rels = []
        for n in range(150):
            rel = f"rtl/pkg_{n}.sv"
            self.ws.write(rel, f"package pkg_{n};\n{body}endpackage\n")
            rels.append(rel)
        files = ", ".join(f'"{r}"' for r in rels)
        self.ws.write(".mimir.toml", f"[slang]\nfiles = [{files}]\n")

        lsp = self._start()
        uri = self.ws.uri("edit.sv")
        n_modules = 40
        text = "".join(f"module m{i};\n  int v{i};\nendmodule\n" for i in range(n_modules))
        lsp.did_open(uri, text)
        self.assertIsNotNone(lsp.wait_for_fresh_diagnostics(uri, timeout=60.0))

        # Kick off a re-index (what a `.mimir.toml` edit or a settings change
        # does) and type straight through it.
        lsp.notify("workspace/didChangeConfiguration", {"settings": {}})

        # Burst of edits with no waiting in between: delete the first module
        # (3 lines) over and over. Each one shifts everything after it.
        version = 1
        for _ in range(n_modules - 2):
            version += 1
            _change(lsp, uri, version, _range(0, 0, 3, 0), "")

        names = self._assert_alive(lsp, uri)
        # The outline is published per parse; wait until the last edit's
        # parse has landed before judging it.
        deadline = time.monotonic() + 60.0
        expected = [f"m{n_modules - 2}", f"v{n_modules - 2}",
                    f"m{n_modules - 1}", f"v{n_modules - 1}"]
        while names != expected and time.monotonic() < deadline:
            time.sleep(0.2)
            names = self._assert_alive(lsp, uri)
        self.assertEqual(names, expected, "outline must describe the final text")
        # And the semantic-token pass (which slices by tree offsets) survives.
        lsp.request("textDocument/semanticTokens/full", {"textDocument": {"uri": uri}}, timeout=30.0)

    def test_a_panicking_handler_terminates_the_process(self) -> None:
        """When a request handler panicked, the process neither answered nor
        exited: the runtime's shutdown waits on the thread blocked reading
        stdin, so the server lingered as a zombie (still holding its sidecar
        children) and the editor never got the "server died" signal it needs
        to restart it. A panic on the request loop must end the process."""
        lsp = self._start(env={"MIMIR_DEBUG_HOOKS": "1"})
        # A request (it has an id) — sent raw, because no reply will come.
        lsp._send({"jsonrpc": "2.0", "id": 4242, "method": "mimir/debug/panic", "params": None})
        try:
            code = lsp._proc.wait(timeout=10)
        except Exception:
            self.fail(
                "server is still running 10 s after a handler panic\n"
                + lsp.stderr_text[-2000:]
            )
        self.assertNotEqual(code, 0, "a crash must not look like a clean exit")
        self.assertIn("panicked", lsp.stderr_text, "the panic must be logged first")

    def test_debug_panic_hook_is_inert_by_default(self) -> None:
        """Without `MIMIR_DEBUG_HOOKS` the fault-injection request is just
        an unknown method — it must not be able to take a server down."""
        lsp = self._start()
        with self.assertRaises(Exception):
            lsp.request("mimir/debug/panic", None)
        uri = self.ws.uri("alive.sv")
        lsp.did_open(uri, "module alive;\nendmodule\n")
        self.assertIn("alive", self._assert_alive(lsp, uri))

    # -- buffer synchronisation -------------------------------------------

    def test_edit_ending_past_the_document_is_applied(self) -> None:
        """An edit whose end position lies beyond the last line / column is
        legal LSP (positions clamp). It used to be rejected and dropped,
        leaving the server's copy of the buffer out of sync for good."""
        lsp = self._start()
        uri = self.ws.uri("clamp.sv")
        lsp.did_open(uri, "module a;\nendmodule")
        _change(lsp, uri, 2, _range(1, 0, 2, 0), "  int added_var;\nendmodule\n")
        self.assertIn("added_var", self._assert_alive(lsp, uri))
        # Column past the end of the line: replaces to end of line only.
        _change(lsp, uri, 3, _range(1, 6, 1, 999), "renamed_var;")
        names = self._assert_alive(lsp, uri)
        self.assertIn("renamed_var", names)
        self.assertNotIn("added_var", names)

    def test_form_feed_does_not_shift_later_lines(self) -> None:
        """A form feed (`^L` page break) is not a line terminator in LSP.
        Counting it as one shifted every later line by one, so edits and
        lookups below it hit the wrong line."""
        lsp = self._start()
        uri = self.ws.uri("ff.sv")
        lsp.did_open(uri, "// page one\x0c\nmodule a;\n  int old_name;\nendmodule\n")
        # Editor coordinates: `old_name` is on line 2, columns 6..14.
        _change(lsp, uri, 2, _range(2, 6, 2, 14), "new_name")
        names = self._assert_alive(lsp, uri)
        self.assertEqual(names, ["a", "new_name"])

    # -- references / rename ----------------------------------------------

    def test_rename_of_a_local_variable_stays_in_its_file(self) -> None:
        """Renaming a function-local variable must not rewrite same-named
        symbols in other files."""
        lsp = self._start()
        a, b = self.ws.uri("a.sv"), self.ws.uri("b.sv")
        a_text = (
            "module a;\n"
            "  function void f();\n"
            "    int idx;\n"
            "    idx = 1;\n"
            "  endfunction\n"
            "endmodule\n"
        )
        b_text = "int idx;\nmodule b;\n  initial $display(idx);\nendmodule\n"
        lsp.did_open(a, a_text)
        lsp.did_open(b, b_text)
        self._assert_alive(lsp, b)

        edit = lsp.request(
            "textDocument/rename",
            {"textDocument": {"uri": a}, "position": _pos(3, 5), "newName": "counter"},
        )
        changes = edit["changes"]
        self.assertEqual(list(changes), [a], f"only a.sv may change, got {sorted(changes)}")
        self.assertEqual(
            _apply_edits(a_text, changes[a]),
            a_text.replace("idx", "counter"),
        )

    def test_rename_from_a_class_declaration_updates_same_file_uses(self) -> None:
        """Renaming a class with the cursor on its *declaration* used to
        change only the declaration, leaving every use in the file behind."""
        lsp = self._start()
        uri = self.ws.uri("c.sv")
        text = (
            "class packet;\n"
            "  int x;\n"
            "endclass\n"
            "module m;\n"
            "  packet p;\n"
            "  initial p = new();\n"
            "endmodule\n"
        )
        lsp.did_open(uri, text)
        self._assert_alive(lsp, uri)
        edit = lsp.request(
            "textDocument/rename",
            {"textDocument": {"uri": uri}, "position": _pos(0, 8), "newName": "frame"},
        )
        self.assertEqual(
            _apply_edits(text, edit["changes"][uri]),
            text.replace("packet", "frame"),
        )

    # -- formatting --------------------------------------------------------

    def test_range_formatting_edit_reproduces_the_formatter_output(self) -> None:
        """Range formatting paired "lines s..e of the buffer" with "lines
        s..e of the formatter's output". As soon as the formatter changed a
        line's length or the number of lines, applying the edit corrupted
        the buffer: leftover tails, duplicated lines, deleted code."""
        self.ws.write(".mimir.toml", f'[formatter]\nbinary = "{FAKE_FORMATTER}"\n')
        lsp = self._start()
        uri = self.ws.uri("fmt.sv")
        text = (
            "module m;\n"
            "  assign   a  =  b;\n"              # shorter after formatting
            "  assign c =\n"                     # three lines joined into one
            "      d +\n"
            "      e;\n"
            "  always_comb begin x = y; end\n"   # one line split into three
            "  wire   untouched;\n"              # outside the range
            "endmodule\n"
        )
        lsp.did_open(uri, text)
        self._assert_alive(lsp, uri)
        expected = (
            "module m;\n"
            "  assign a = b;\n"
            "  assign c = d + e;\n"
            "  always_comb begin\n"
            "    x = y;\n"
            "  end\n"
            "  wire   untouched;\n"
            "endmodule\n"
        )
        edits = lsp.request(
            "textDocument/rangeFormatting",
            {
                "textDocument": {"uri": uri},
                "range": _range(1, 0, 5, 30),
                "options": {"tabSize": 2, "insertSpaces": True},
            },
            timeout=30.0,
        )
        self.assertTrue(edits, "the formatter changed the text, so edits are expected")
        self.assertEqual(_apply_edits(text, edits), expected)


# ---------------------------------------------------------------------------
# Sidecar plumbing (fake sidecar)
# ---------------------------------------------------------------------------


class SlangPlumbingRegressionTest(unittest.TestCase):
    """How the server feeds the sidecar and what it does with the answers.

    Uses ``tests/fake_sidecar.py``: it reports one ``FakeError`` for every
    line containing ``SLANG_ERROR_HERE`` and logs each request it receives.
    """

    DEBOUNCE_MS = 100
    # Extra environment for the server (and, by inheritance, the sidecar).
    EXTRA_ENV: dict[str, str] = {}

    def setUp(self) -> None:
        self.ws = _Workspace()
        self.addCleanup(self.ws.cleanup)
        self.log = self.ws.root / "sidecar.log"
        self.a_text = "module a;\n  wire SLANG_ERROR_HERE;\nendmodule\n"
        self.b_text = "module b;\nendmodule\n"
        self.ws.write("a.sv", self.a_text)
        self.ws.write("b.sv", self.b_text)
        (self.ws.root / "sub").mkdir()
        # "// © Müller" in Latin-1 — not valid UTF-8.
        self.ws.write("legacy.sv", b"// \xa9 M\xfcller\nmodule legacy;\nendmodule\n")
        self.ws.write(
            ".mimir.toml",
            "[slang]\n"
            # `b.sv` is deliberately spelled with a `..` detour.
            'files = ["a.sv", "sub/../b.sv", "legacy.sv"]\n'
            f"debounce_ms = {self.DEBOUNCE_MS}\n",
        )
        self.a_uri = self.ws.uri("a.sv")
        self.b_uri = self.ws.uri("b.sv")
        self.lsp = MimirLspClient(env={
            "FAKE_SIDECAR_LOG": str(self.log),
            # Explicit, so a real sidecar exported in the developer's shell
            # can't take precedence over the fake one.
            "MIMIR_SLANG_PATH": str(FAKE_SIDECAR),
            **self.EXTRA_ENV,
        })
        self.addCleanup(self.lsp.close)
        self.lsp.initialize(workspace_root=self.ws.root)
        # The startup compile publishes for every project file.
        first = self._wait_for_diags(self.a_uri, lambda d: bool(d), timeout=20.0)
        self.assertEqual([d["code"] for d in first], ["FakeError"])

    # -- helpers -----------------------------------------------------------

    def _pump(self, seconds: float) -> None:
        """Read server messages for ``seconds`` (queues notifications)."""
        fd = self.lsp._proc.stdout.fileno()
        deadline = time.monotonic() + seconds
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            # Only start reading a frame once one is arriving, then read all
            # of it: `_recv` interrupted mid-header loses the bytes it has
            # already consumed and desynchronises the stream.
            ready, _, _ = select.select([fd], [], [], remaining)
            if not ready:
                return
            self.lsp._handle_incoming(self.lsp._recv(timeout=30.0))

    def _last_diags(self, uri: str) -> list[dict] | None:
        """Diagnostics of the most recent publish for ``uri`` (what the
        editor is showing right now), or ``None`` if there was none."""
        hits = [
            p for p in self.lsp.collected_notifications("textDocument/publishDiagnostics")
            if p.get("uri") == uri
        ]
        return hits[-1]["diagnostics"] if hits else None

    def _wait_for_diags(self, uri: str, accept, timeout: float = 10.0) -> list[dict]:
        """Pump until the latest publish for ``uri`` satisfies ``accept``."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            last = self._last_diags(uri)
            if last is not None and accept(last):
                return last
            self._pump(0.1)
        self.fail(
            f"latest diagnostics for {uri} never matched; last = {self._last_diags(uri)}\n"
            f"--- stderr ---\n{self.lsp.stderr_text[-3000:]}"
        )

    def _settle(self) -> None:
        """Let debounce timers fire and their publishes arrive."""
        self._pump(self.DEBOUNCE_MS / 1000.0 * 4 + 1.0)

    def _requests(self) -> list[dict]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines() if line.strip()]

    def _compiles(self) -> list[dict]:
        return [r for r in self._requests() if r["method"] == "compile"]

    # -- diagnostics lifecycle --------------------------------------------

    def test_opening_a_file_keeps_its_slang_diagnostics(self) -> None:
        """`didOpen` publishes tree-sitter diagnostics, replacing slang's in
        the editor. Nothing changed, so the follow-up compile is skipped —
        and slang's errors used to stay erased. They must come back."""
        compiles_before = len(self._compiles())
        self.lsp.did_open(self.a_uri, self.a_text)
        self._settle()
        self.assertEqual(
            [d["code"] for d in self._last_diags(self.a_uri)],
            ["FakeError"],
            "the slang error must still be showing after the file is opened",
        )
        self.assertEqual(
            len(self._compiles()), compiles_before,
            "identical inputs must not trigger another compile",
        )

    def test_edit_and_revert_keeps_slang_diagnostics(self) -> None:
        """Type a character and delete it again: the text — and therefore the
        compile inputs — end up unchanged, so the compile is skipped, yet the
        keystrokes' tree-sitter publishes had wiped slang's diagnostics."""
        self.lsp.did_open(self.a_uri, self.a_text)
        self._settle()
        _change(self.lsp, self.a_uri, 2, _range(0, 0, 0, 0), " ")
        _change(self.lsp, self.a_uri, 3, _range(0, 0, 0, 1), "")
        self._settle()
        self.assertEqual([d["code"] for d in self._last_diags(self.a_uri)], ["FakeError"])

    def test_closing_a_file_keeps_its_slang_diagnostics(self) -> None:
        """A project file's elaboration errors don't depend on whether it is
        open. `didClose` used to publish an empty list and the Problems
        panel lost them."""
        self.lsp.did_open(self.a_uri, self.a_text)
        self._settle()
        self.lsp.did_close(self.a_uri)
        self._settle()
        self.assertEqual([d["code"] for d in self._last_diags(self.a_uri)], ["FakeError"])

    def test_discarded_edits_are_not_left_in_the_compilation(self) -> None:
        """Fix the error in the buffer, then close *without saving*. The
        compilation must go back to the on-disk text — where the error still
        exists — instead of keeping the discarded buffer's verdict."""
        self.lsp.did_open(self.a_uri, self.a_text)
        self._settle()
        _change(self.lsp, self.a_uri, 2, None, "module a;\nendmodule\n")
        self._wait_for_diags(self.a_uri, lambda d: d == [])
        self.lsp.did_close(self.a_uri)
        self._wait_for_diags(self.a_uri, lambda d: [x["code"] for x in d] == ["FakeError"])

    # -- request assembly --------------------------------------------------

    def test_open_buffer_replaces_project_file_written_with_dotdot(self) -> None:
        """`sub/../b.sv` in the config and `b.sv` in the editor are the same
        file. They used not to compare equal, so slang got the stale on-disk
        copy as the compilation unit *plus* the buffer as a second file, and
        unsaved edits never reached elaboration."""
        self.lsp.did_open(self.b_uri, self.b_text)
        edited = "module b;\n  wire SLANG_ERROR_HERE;\nendmodule\n"
        _change(self.lsp, self.b_uri, 2, None, edited)
        diags = self._wait_for_diags(
            self.b_uri, lambda d: [x["code"] for x in d] == ["FakeError"]
        )
        self.assertEqual(diags[0]["range"]["start"]["line"], 1)

        last = self._compiles()[-1]
        b_entries = [f for f in last["files"] if pathlib.Path(f["path"]).name == "b.sv"]
        self.assertEqual(len(b_entries), 1, f"b.sv must be sent exactly once: {last['files']}")
        self.assertTrue(b_entries[0]["cu"], "…as a compilation unit")
        self.assertTrue(b_entries[0]["marker"], "…carrying the unsaved buffer text")
        self.assertNotIn("..", b_entries[0]["path"], "…under its normalised path")

        # No publish ever went to a URL with a `..` segment.
        uris = {p["uri"] for p in
                self.lsp.collected_notifications("textDocument/publishDiagnostics")}
        self.assertFalse([u for u in uris if "/../" in u], f"un-normalised URLs: {uris}")

    def test_non_utf8_project_file_is_sent_with_its_contents(self) -> None:
        """A source file with Latin-1 bytes used to count as unreadable and
        was handed to slang *empty*, producing bogus elaboration errors."""
        first = self._compiles()[0]
        legacy = [f for f in first["files"] if f["path"].endswith("legacy.sv")]
        self.assertEqual(len(legacy), 1)
        self.assertGreater(legacy[0]["bytes"], 20, "the file's text must be sent, not ''")

        # …and it is indexed for navigation as well.
        symbols = self.lsp.request("workspace/symbol", {"query": "legacy"})
        self.assertIn("legacy", [s["name"] for s in symbols or []])


class SlowCompileRegressionTest(SlangPlumbingRegressionTest):
    """The same plumbing, against a sidecar whose compiles take a while —
    the situation on any real project."""

    COMPILE_MS = 600
    EXTRA_ENV = {"FAKE_SIDECAR_COMPILE_DELAY_MS": str(COMPILE_MS)}

    def test_typing_through_a_slow_compile_does_not_pile_up_requests(self) -> None:
        """Every keystroke used to abort the elaborate round in flight and
        start a new one. Aborting only drops *our* side: the sidecar still
        works through the request it was sent, and then through every
        request sent after it. Typing steadily therefore queued one full
        compile per keystroke — the result for the final text arrived only
        after all the stale ones had been computed, and no diagnostics were
        published in between because each round was cancelled before it
        could publish.

        A round whose request is already out must be allowed to finish;
        newer edits wait for it and then compile the *latest* text once."""
        self.lsp.did_open(self.b_uri, self.b_text)
        self._settle()
        compiles_before = len(self._compiles())

        edits = 6
        gap = self.COMPILE_MS / 1000.0 / 2  # two keystrokes per compile
        started = time.monotonic()
        for i in range(edits):
            text = f"module b;\n  wire w{i};\nendmodule\n"
            if i == edits - 1:
                text = "module b;\n  wire SLANG_ERROR_HERE;\nendmodule\n"
            _change(self.lsp, self.b_uri, i + 2, None, text)
            self._pump(gap)

        diags = self._wait_for_diags(
            self.b_uri, lambda d: [x["code"] for x in d] == ["FakeError"], timeout=30.0
        )
        latency = time.monotonic() - started - (edits - 1) * gap
        self.assertEqual(diags[0]["range"]["start"]["line"], 1)

        sent = len(self._compiles()) - compiles_before
        self.assertLessEqual(
            sent, edits // 2 + 1,
            f"{sent} compiles for {edits} keystrokes: stale rounds are piling up",
        )
        # The verdict on the final text must not wait behind a queue of
        # compiles for text that no longer exists: at most the round in
        # flight when the last key was pressed, plus its own.
        self.assertLess(
            latency, 3 * self.COMPILE_MS / 1000.0 + 1.0,
            f"final diagnostics took {latency:.1f}s after the last keystroke",
        )


if __name__ == "__main__":
    unittest.main()
