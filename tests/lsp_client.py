"""Minimal LSP client for driving `mimir-server` from integration tests.

Stdlib only — no external deps. Spawns the server as a subprocess, speaks
LSP/JSON-RPC over stdio with the standard `Content-Length` framing, and
exposes a small request/notify API.

Designed for short-lived tests: open the document, fire one or two
requests, shut down. For long-running editor-style sessions you'd want a
background reader thread; here the synchronous request/response pattern
is simpler and sufficient.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import threading
import time
from typing import Any


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT_BINARY = REPO_ROOT / "target" / "release" / "mimir-server"


class LspError(RuntimeError):
    """Raised when the server returns a JSON-RPC error response."""


class MimirLspClient:
    """Synchronous LSP client. One server subprocess per instance.

    Usage:

        with MimirLspClient() as lsp:
            lsp.initialize(workspace_root=...)
            lsp.did_open(uri, text, language_id="systemverilog")
            folds = lsp.request("textDocument/foldingRange", {...})
    """

    def __init__(
        self,
        binary: pathlib.Path | str = DEFAULT_BINARY,
        env: dict[str, str] | None = None,
        log_to_stderr: bool = False,
    ) -> None:
        self.binary = pathlib.Path(binary)
        if not self.binary.exists():
            raise FileNotFoundError(
                f"mimir-server binary not found at {self.binary}. "
                "Run `cargo build --release -p mimir-server` first."
            )

        merged_env = os.environ.copy()
        if env:
            merged_env.update(env)
        # Always enable debug logging if asked; otherwise quiet.
        if log_to_stderr and "RUST_LOG" not in merged_env:
            merged_env["RUST_LOG"] = "mimir=debug"

        # Redirect stderr to a pipe we drain in a background thread so it
        # doesn't fill the OS buffer and block the server.
        self._proc = subprocess.Popen(
            [str(self.binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=merged_env,
            bufsize=0,
        )
        self._next_id = 1
        self._notifications: list[tuple[str, dict]] = []
        self._stderr_lines: list[str] = []

        # Drain stderr in a background thread — otherwise debug logs back
        # up the pipe and the server stalls.
        self._stderr_thread = threading.Thread(
            target=self._drain_stderr, daemon=True
        )
        self._stderr_thread.start()

    # -------- context manager --------

    def __enter__(self) -> "MimirLspClient":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    # -------- public API --------

    def initialize(
        self,
        workspace_root: pathlib.Path | str | None = None,
    ) -> dict[str, Any]:
        """Run the LSP `initialize` + `initialized` handshake."""
        params: dict[str, Any] = {
            "processId": os.getpid(),
            "rootUri": _path_to_uri(workspace_root) if workspace_root else None,
            "capabilities": {},
        }
        if workspace_root:
            params["workspaceFolders"] = [
                {
                    "uri": _path_to_uri(workspace_root),
                    "name": pathlib.Path(workspace_root).name,
                }
            ]
        result = self.request("initialize", params)
        self.notify("initialized", {})
        return result

    def did_open(
        self,
        uri: str,
        text: str,
        language_id: str = "systemverilog",
        version: int = 1,
    ) -> None:
        self.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                }
            },
        )

    def did_close(self, uri: str) -> None:
        self.notify(
            "textDocument/didClose", {"textDocument": {"uri": uri}}
        )

    def shutdown(self) -> None:
        try:
            self.request("shutdown", None)
            self.notify("exit", None)
        except Exception:
            pass

    def close(self) -> None:
        try:
            self.shutdown()
        except Exception:
            pass
        try:
            if self._proc.poll() is None:
                self._proc.terminate()
                try:
                    self._proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self._proc.kill()
        finally:
            if self._proc.stdin:
                self._proc.stdin.close()
            if self._proc.stdout:
                self._proc.stdout.close()

    @property
    def stderr_text(self) -> str:
        """All stderr captured so far. Useful for assertions on log lines."""
        return "".join(self._stderr_lines)

    # -------- request / notify primitives --------

    def request(
        self, method: str, params: Any, timeout: float = 10.0
    ) -> Any:
        msg_id = self._next_id
        self._next_id += 1
        self._send({"jsonrpc": "2.0", "id": msg_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"timed out waiting for response to {method!r} (id={msg_id})"
                )
            msg = self._recv(timeout=remaining)
            if msg.get("id") == msg_id:
                if "error" in msg:
                    raise LspError(f"{method} -> {msg['error']}")
                return msg.get("result")
            # Anything else is a server-originated notification; queue it.
            if "method" in msg and "id" not in msg:
                self._notifications.append((msg["method"], msg.get("params", {})))
            # Responses to other requests shouldn't exist for this synchronous
            # client; if they do, drop them.

    def notify(self, method: str, params: Any) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def collected_notifications(self, method: str | None = None) -> list[dict]:
        if method is None:
            return [p for _, p in self._notifications]
        return [p for m, p in self._notifications if m == method]

    def wait_for_notification(
        self, method: str, timeout: float = 3.0
    ) -> dict | None:
        """Block until a notification with `method` arrives, or timeout.

        Useful for waiting on `textDocument/publishDiagnostics` after a
        `did_open` / `did_change`.
        """
        existing = self.collected_notifications(method)
        if existing:
            return existing[-1]
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                msg = self._recv(timeout=deadline - time.monotonic())
            except TimeoutError:
                return None
            if "method" in msg and "id" not in msg:
                self._notifications.append((msg["method"], msg.get("params", {})))
                if msg["method"] == method:
                    return msg.get("params", {})
        return None

    # -------- internals --------

    def _send(self, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        assert self._proc.stdin is not None
        self._proc.stdin.write(header + body)
        self._proc.stdin.flush()

    def _recv(self, timeout: float) -> dict:
        # subprocess pipes are blocking. We rely on the test runner to set
        # a wall-clock timeout via the request() loop. The OS read here
        # will block until the server writes a complete frame.
        deadline = time.monotonic() + timeout
        assert self._proc.stdout is not None

        # Read headers.
        header_bytes = b""
        while b"\r\n\r\n" not in header_bytes:
            if time.monotonic() > deadline:
                raise TimeoutError("timed out reading LSP header")
            chunk = self._proc.stdout.read(1)
            if not chunk:
                stderr_tail = "".join(self._stderr_lines[-50:])
                raise RuntimeError(
                    "server stdout closed unexpectedly. "
                    f"recent stderr:\n{stderr_tail}"
                )
            header_bytes += chunk

        headers_str = header_bytes.decode("ascii")
        content_length = 0
        for line in headers_str.split("\r\n"):
            if line.lower().startswith("content-length:"):
                content_length = int(line.split(":", 1)[1].strip())
                break
        if content_length <= 0:
            raise RuntimeError(f"missing/invalid Content-Length in {headers_str!r}")

        body = b""
        while len(body) < content_length:
            if time.monotonic() > deadline:
                raise TimeoutError("timed out reading LSP body")
            chunk = self._proc.stdout.read(content_length - len(body))
            if not chunk:
                raise RuntimeError("server stdout closed mid-body")
            body += chunk
        return json.loads(body.decode("utf-8"))

    def _drain_stderr(self) -> None:
        assert self._proc.stderr is not None
        for raw in self._proc.stderr:
            try:
                self._stderr_lines.append(raw.decode("utf-8", errors="replace"))
            except Exception:
                pass


def _path_to_uri(path: pathlib.Path | str) -> str:
    p = pathlib.Path(path).resolve()
    return p.as_uri()


def file_uri(path: pathlib.Path | str) -> str:
    """Public helper — convert a filesystem path to a `file://` URI."""
    return _path_to_uri(path)


def read_text(path: pathlib.Path | str) -> str:
    return pathlib.Path(path).read_text(encoding="utf-8")


if __name__ == "__main__":
    # Smoke test: initialize + shutdown.
    with MimirLspClient(log_to_stderr=True) as lsp:
        result = lsp.initialize()
        print(json.dumps(result, indent=2))
        sys.exit(0)
