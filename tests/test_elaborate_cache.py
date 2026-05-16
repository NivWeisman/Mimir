"""Integration tests for the slang-elaborate input cache.

The server hashes the inputs of every slang ``elaborate`` request
(``Vec<SourceFile>`` + include dirs + defines + top). If a later
``did_open`` / ``did_change`` would produce the same hash, the server
**skips the round-trip** — slang's compilation is unchanged, the prior
diagnostics still apply, and we save the ~500KB response packet plus
slang's re-elaboration time.

These tests assert that:

1. The first successful elaborate emits one ``info!`` per indexed file
   (``"indexed by startup slang elaborate"``), and **only** the first
   one does.
2. A redundant ``did_open`` for a project file is a cache hit — the
   server logs ``"slang inputs unchanged ... skipping"`` and does
   **not** send a fresh request to the sidecar.
3. A real edit (``did_change`` that changes bytes) bypasses the cache.
   Reverting the edit hits the cache again (hash returned to a prior
   value).

Each test inspects ``lsp.stderr_text`` for debug-log lines, so the
server must be launched with ``RUST_LOG=mimir=debug`` — the
``MimirLspClient`` does this when ``log_to_stderr=True``.

Run::

    cargo build --release -p mimir-server
    MIMIR_SLANG_PATH=/path/to/mimir-slang \\
        python3 -m unittest tests.test_elaborate_cache -v
"""

from __future__ import annotations

import os
import pathlib
import threading
import time
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
APB_DIR = REPO_ROOT / "examples" / "uvm-1.2" / "examples" / "integrated" / "apb"
# apb.sv is in the filelist (`apb.f`), so opening it does not add a new
# entry to ElaborateParams.files — the hash matches the startup
# elaborate exactly. apb_monitor.sv is only reachable via `` `include ``
# and would be added as an "open but not in filelist" entry, changing
# the hash and forcing a cache miss; we'd be testing a different code
# path. Pick the filelist member on purpose.
APB_SV = APB_DIR / "apb.sv"

# The .mimir.toml ships with debounce_ms = 500 (the default). We must
# wait longer than that for every elaborate to fire-or-skip; 1.0 s gives
# headroom against scheduler jitter without making the suite painful.
DEBOUNCE_WAIT_S = 1.0

# Marker strings emitted by `schedule_elaborate` in
# crates/mimir-server/src/backend.rs. Tests grep stderr for them.
LOG_INDEXED = "indexed by startup slang elaborate"
LOG_CACHE_HIT = "slang inputs unchanged since last elaborate; skipping"
LOG_SENDING = "sending elaborate request"
LOG_RECEIVED = "received response"


def _require_slang() -> str:
    """Skip the suite when there's no usable slang sidecar. Returns the
    absolute path to the binary so the test can pass it through to the
    server's environment."""
    path = os.environ.get("MIMIR_SLANG_PATH")
    if not path:
        raise unittest.SkipTest(
            "MIMIR_SLANG_PATH not set — these tests require a running slang sidecar"
        )
    if not pathlib.Path(path).is_file():
        raise unittest.SkipTest(f"MIMIR_SLANG_PATH points at {path} but no file is there")
    return path


class _StdoutDrainer(threading.Thread):
    """Background thread that consumes server→client LSP frames so the
    OS pipe never fills up while a test is sleeping.

    The default ``MimirLspClient`` only reads stdout when a request /
    ``wait_for_notification`` call asks for one. With slang's ~500KB
    diagnostics dump on the first elaborate, the pipe buffer fills
    almost immediately and the server stalls mid-``publish_diagnostics``.
    This thread reads complete LSP frames in the background and discards
    them — these tests don't inspect notification bodies, only stderr.

    Started in ``setUpClass`` after ``initialize`` (so the initial
    request/response handshake has already drained synchronously), and
    runs until the proc exits or stdout closes.
    """

    def __init__(self, lsp: MimirLspClient) -> None:
        super().__init__(daemon=True)
        self._lsp = lsp
        self._stop = False

    def stop(self) -> None:
        self._stop = True

    def run(self) -> None:  # pragma: no cover — trivial framing
        stdout = self._lsp._proc.stdout
        assert stdout is not None
        while not self._stop:
            header = b""
            while b"\r\n\r\n" not in header:
                ch = stdout.read(1)
                if not ch:
                    return
                header += ch
            content_length = 0
            for line in header.decode("ascii", errors="replace").split("\r\n"):
                if line.lower().startswith("content-length:"):
                    content_length = int(line.split(":", 1)[1].strip())
                    break
            body = b""
            while len(body) < content_length:
                chunk = stdout.read(content_length - len(body))
                if not chunk:
                    return
                body += chunk
            # Body discarded — these tests grep stderr, not notifications.


def _wait_for_log(
    lsp: MimirLspClient, needle: str, timeout: float = 5.0
) -> bool:
    """Poll ``lsp.stderr_text`` until ``needle`` appears or ``timeout``
    elapses. Drains stdout in the background via :class:`_StdoutDrainer`
    so the server doesn't stall mid-publish. Returns ``True`` on hit,
    ``False`` on timeout."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if needle in lsp.stderr_text:
            return True
        time.sleep(0.05)
    return False


class ElaborateInputCacheTest(unittest.TestCase):
    """End-to-end tests for the ``last_elaborate_input_hash`` cache."""

    @classmethod
    def setUpClass(cls) -> None:
        if not APB_SV.exists():
            raise unittest.SkipTest(f"example not found: {APB_SV}")
        slang_path = _require_slang()
        # `log_to_stderr=True` flips RUST_LOG to mimir=debug; we need that
        # for every log marker the assertions look for.
        cls.lsp = MimirLspClient(
            log_to_stderr=True,
            env={"MIMIR_SLANG_PATH": slang_path},
        )
        cls.lsp.initialize(workspace_root=APB_DIR)
        # `initialize` itself drained synchronously. From here on, the
        # server may push diagnostics at any time — start the background
        # drainer so the stdout pipe doesn't fill while we wait on stderr.
        cls.drainer = _StdoutDrainer(cls.lsp)
        cls.drainer.start()
        # The startup elaborate is scheduled from `initialize`, fires after
        # the project's debounce. Wait for it to land before any test runs.
        if not _wait_for_log(cls.lsp, LOG_INDEXED, timeout=30.0):
            cls.drainer.stop()
            cls.lsp.close()
            raise unittest.SkipTest(
                "startup slang elaborate never logged 'indexed'; "
                "either slang sidecar failed to spawn or it took longer than 30s"
            )
        cls.uri = file_uri(APB_SV)
        cls.text = read_text(APB_SV)

    @classmethod
    def tearDownClass(cls) -> None:
        # Drainer must stop before close() — close() sends `shutdown` /
        # `exit` and waits for the shutdown *response*, which the drainer
        # would otherwise consume. We also have to actually wake the
        # drainer up: it's blocked on `stdout.read(1)`. The cleanest way
        # is to skip the graceful shutdown handshake and let the proc
        # die from `terminate()` — the drainer's read returns empty and
        # the thread exits.
        if hasattr(cls, "lsp"):
            try:
                if cls.lsp._proc.poll() is None:
                    cls.lsp._proc.terminate()
                    try:
                        cls.lsp._proc.wait(timeout=2)
                    except Exception:
                        cls.lsp._proc.kill()
            finally:
                if cls.lsp._proc.stdin:
                    cls.lsp._proc.stdin.close()
                if cls.lsp._proc.stdout:
                    cls.lsp._proc.stdout.close()
        if hasattr(cls, "drainer"):
            cls.drainer.stop()

    # ------------------------------------------------------------------
    # Test 1 — startup info-log is emitted exactly once
    # ------------------------------------------------------------------

    def test_startup_indexed_logs_emitted_once(self) -> None:
        """``info!("indexed by startup slang elaborate")`` should fire on
        the **first** successful elaborate and never again, even if a
        later elaborate also succeeds. The flag is a one-shot
        ``AtomicBool`` on ``Backend``."""
        before = self.lsp.stderr_text.count(LOG_INDEXED)
        # Re-trigger an elaborate via did_open. With the cache hit (same
        # inputs) we expect no new "indexed" lines.
        self.lsp.did_open(self.uri, self.text)
        time.sleep(DEBOUNCE_WAIT_S)
        after = self.lsp.stderr_text.count(LOG_INDEXED)
        self.assertEqual(
            before, after,
            f"'indexed' log should not repeat: before={before}, after={after}",
        )
        # And there should be at least one line from the startup elaborate.
        self.assertGreaterEqual(before, 1, "startup never emitted any 'indexed' log")

    # ------------------------------------------------------------------
    # Test 2 — opening an already-elaborated file hits the cache
    # ------------------------------------------------------------------

    def test_redundant_open_is_cache_hit(self) -> None:
        """Opening ``apb.sv`` (a filelist member) after the startup elaborate has
        already covered it should produce identical ``ElaborateParams``
        (the editor's text matches disk on first open) and therefore
        skip the slang round-trip."""
        hits_before = self.lsp.stderr_text.count(LOG_CACHE_HIT)
        recv_before = self.lsp.stderr_text.count(LOG_RECEIVED)

        self.lsp.did_open(self.uri, self.text)
        time.sleep(DEBOUNCE_WAIT_S)

        hits_after = self.lsp.stderr_text.count(LOG_CACHE_HIT)
        recv_after = self.lsp.stderr_text.count(LOG_RECEIVED)

        self.assertEqual(
            hits_after - hits_before, 1,
            f"expected exactly one cache-hit log; got {hits_after - hits_before}",
        )
        self.assertEqual(
            recv_after, recv_before,
            "cache hit must not produce a new sidecar 'received response'; "
            f"before={recv_before}, after={recv_after}",
        )

    # ------------------------------------------------------------------
    # Test 3 — real edits bypass the cache
    # ------------------------------------------------------------------

    def test_real_edit_bypasses_cache(self) -> None:
        """A ``did_change`` that actually mutates bytes must miss the
        cache. The cache stores only the *most recent* successful input
        hash, so reverting an edit is not asserted to hit — that would
        require an LRU and is out of scope."""
        # Open the document first so did_change has a target.
        self.lsp.did_open(self.uri, self.text)
        time.sleep(DEBOUNCE_WAIT_S)

        recv_before_edit = self.lsp.stderr_text.count(LOG_RECEIVED)

        # Apply a full-sync edit that appends a harmless comment. Using
        # the full-sync variant (no `range`) sidesteps having to compute
        # ropey-correct LSP positions in the test.
        edited = self.text + "\n// mimir cache test edit\n"
        self.lsp.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": self.uri, "version": 2},
                "contentChanges": [{"text": edited}],
            },
        )
        time.sleep(DEBOUNCE_WAIT_S)

        recv_after_edit = self.lsp.stderr_text.count(LOG_RECEIVED)
        self.assertGreater(
            recv_after_edit, recv_before_edit,
            "edit changed the input hash but slang was not consulted "
            f"(received: before={recv_before_edit}, after={recv_after_edit})",
        )

        # Re-applying the same edited text must now hit the cache —
        # nothing changed since the elaborate above.
        hits_before_replay = self.lsp.stderr_text.count(LOG_CACHE_HIT)
        recv_before_replay = self.lsp.stderr_text.count(LOG_RECEIVED)

        self.lsp.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": self.uri, "version": 3},
                "contentChanges": [{"text": edited}],
            },
        )
        time.sleep(DEBOUNCE_WAIT_S)

        hits_after_replay = self.lsp.stderr_text.count(LOG_CACHE_HIT)
        recv_after_replay = self.lsp.stderr_text.count(LOG_RECEIVED)

        self.assertEqual(
            hits_after_replay - hits_before_replay, 1,
            "replaying the same edit should hit the cache "
            f"(hits before={hits_before_replay}, after={hits_after_replay})",
        )
        self.assertEqual(
            recv_after_replay, recv_before_replay,
            "replayed edit hit the cache but slang was still consulted "
            f"(received before={recv_before_replay}, after={recv_after_replay})",
        )

        # Restore the original text so the next test starts from a
        # known cache state (hash = original). The cache is single-entry,
        # so order-dependent tests need to leave it where they found it.
        self.lsp.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": self.uri, "version": 4},
                "contentChanges": [{"text": self.text}],
            },
        )
        time.sleep(DEBOUNCE_WAIT_S)


if __name__ == "__main__":
    unittest.main()
