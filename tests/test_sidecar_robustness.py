"""Robustness tests that talk to the slang sidecar *directly* over NDJSON.

The sidecar is a separate process; when it dies, every slang-backed feature
in the editor goes dark until the server is restarted. These tests pin down
inputs that used to kill it.

Requires the sidecar binary (``make sidecar``); skipped when it is absent.
Override the location with ``MIMIR_SLANG_PATH``.

Run:

    make sidecar
    python3 -m unittest tests.test_sidecar_robustness -v
"""

from __future__ import annotations

import json
import os
import pathlib
import select
import subprocess
import tempfile
import time
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT_SIDECAR = REPO_ROOT / "slang-sidecar" / "build" / "mimir-slang-sidecar"


def _sidecar_path() -> pathlib.Path:
    return pathlib.Path(os.environ.get("MIMIR_SLANG_PATH", DEFAULT_SIDECAR))


class Sidecar:
    """One sidecar process with a line-oriented request/response helper."""

    def __init__(self) -> None:
        self.proc = subprocess.Popen(
            [str(_sidecar_path())],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
        )
        self._buf = b""

    def send_raw(self, line: str) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(line.encode("utf-8") + b"\n")
        self.proc.stdin.flush()

    def read_response(self, timeout: float = 30.0) -> dict:
        """Read one response line; fail loudly if the process died."""
        assert self.proc.stdout is not None
        fd = self.proc.stdout.fileno()
        deadline = time.monotonic() + timeout
        while b"\n" not in self._buf:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("sidecar did not answer in time")
            ready, _, _ = select.select([fd], [], [], remaining)
            if not ready:
                raise TimeoutError("sidecar did not answer in time")
            chunk = os.read(fd, 65536)
            if not chunk:
                code = self.proc.wait(timeout=5)
                raise RuntimeError(f"sidecar exited (code {code}) without answering")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        # The wire format is JSON, which must be valid UTF-8.
        return json.loads(line.decode("utf-8"))

    def request(self, req_id: int, method: str, params: dict) -> dict:
        self.send_raw(json.dumps({"id": req_id, "method": method, "params": params}))
        return self.read_response()

    def close(self) -> None:
        try:
            if self.proc.poll() is None:
                self.proc.kill()
            self.proc.wait(timeout=5)
        finally:
            for stream in (self.proc.stdin, self.proc.stdout):
                if stream:
                    stream.close()


class SidecarRobustnessTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not _sidecar_path().exists():
            raise unittest.SkipTest(
                f"slang sidecar not built at {_sidecar_path()} (run `make sidecar`)"
            )

    def setUp(self) -> None:
        self.sidecar = Sidecar()
        self.addCleanup(self.sidecar.close)

    def _assert_alive(self) -> list[dict]:
        """A trivial compile must still be answered. Returns whatever other
        replies (to earlier, rejected requests) arrived before it."""
        self.sidecar.send_raw(json.dumps({
            "id": 9999,
            "method": "compile",
            "params": {"files": [{"path": "/virtual/ok.sv", "text": "module ok; endmodule\n"}]},
        }))
        earlier: list[dict] = []
        while True:
            resp = self.sidecar.read_response()
            if resp.get("id") == 9999:
                break
            earlier.append(resp)
        self.assertIn("result", resp, f"sidecar is no longer answering correctly: {resp}")
        return earlier

    def test_non_utf8_bytes_in_included_header_do_not_kill_the_sidecar(self) -> None:
        """Regression: text that slang read from disk itself (an `include`d
        header) can contain bytes that aren't valid UTF-8 — Latin-1 in a
        string or comment. When such text ended up in a response (a macro
        expansion here), serialising the reply threw, the exception escaped
        the request loop, and the sidecar terminated. Invalid bytes must be
        replaced, not fatal."""
        with tempfile.TemporaryDirectory() as tmp:
            header = pathlib.Path(tmp) / "hdr.svh"
            # `define GREET $display("caf<E9>")   — 0xE9 is Latin-1 'é'.
            header.write_bytes(b'`define GREET $display("caf\xe9")\n')
            top = pathlib.Path(tmp) / "top.sv"
            top_text = '`include "hdr.svh"\nmodule m;\n  initial `GREET;\nendmodule\n'
            top.write_text(top_text)
            files = [{"path": str(top), "text": top_text}]

            resp = self.sidecar.request(
                1,
                "expandMacro",
                {
                    "files": files,
                    "target_path": str(top),
                    "position": {"line": 2, "character": 12},
                },
            )
            self.assertEqual(resp.get("id"), 1)
            self.assertIn("result", resp, f"expected a result, got {resp}")
            self.assertTrue(resp["result"].get("found"), resp)
            self.assertIn("$display", resp["result"]["expanded_text"])

            # A compile over the same sources must survive as well.
            resp = self.sidecar.request(2, "compile", {"files": files})
            self.assertEqual(resp.get("id"), 2)
            self.assertIn("result", resp, f"expected a result, got {resp}")

        self._assert_alive()

    def test_malformed_requests_do_not_kill_the_sidecar(self) -> None:
        """Regression: a request line that is valid JSON but not an object
        (or whose `id` / `method` has the wrong type) made the field
        accessors throw outside the handler's try-block — another way to
        terminate the process. Such lines are rejected, not fatal."""
        for line in ("[1, 2, 3]", "42", '"text"', "null",
                     '{"id": "not-a-number", "method": "compile"}',
                     '{"id": 7, "method": 5}',
                     '{"id": 8, "method": "compile", "params": []}'):
            self.sidecar.send_raw(line)
        rejected = self._assert_alive()
        # Each malformed request is answered with an error (never a result),
        # so a client waiting on it isn't left hanging until its deadline.
        self.assertEqual(len(rejected), 7, rejected)
        for resp in rejected:
            self.assertIn("error", resp, resp)
            self.assertNotIn("result", resp, resp)
        self.assertEqual(rejected[5]["id"], 7, "a readable id is echoed back")


if __name__ == "__main__":
    unittest.main()
