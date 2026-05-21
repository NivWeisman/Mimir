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
import select
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

    def clear_notifications(self, method: str | None = None) -> None:
        """Remove queued notifications so the next ``wait_for_notification``
        blocks until a truly new message arrives."""
        if method is None:
            self._notifications.clear()
        else:
            self._notifications = [(m, p) for m, p in self._notifications
                                   if m != method]

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

    def wait_for_fresh_diagnostics(
        self, uri: str, timeout: float = 8.0
    ) -> dict | None:
        """Block until a **new** ``publishDiagnostics`` for ``uri`` arrives.

        Drains any already-queued notifications for ``uri`` first, then
        reads from the stream until a matching notification arrives.
        Notifications for other URIs are queued and not returned.
        """
        # Discard stale notifications for this URI so we wait for a truly new one.
        self._notifications = [
            (m, p) for m, p in self._notifications
            if not (m == "textDocument/publishDiagnostics" and p.get("uri") == uri)
        ]
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                msg = self._recv(timeout=deadline - time.monotonic())
            except TimeoutError:
                return None
            if "method" in msg and "id" not in msg:
                params = msg.get("params", {})
                self._notifications.append((msg["method"], params))
                if (msg["method"] == "textDocument/publishDiagnostics"
                        and params.get("uri") == uri):
                    return params
        return None

    # -------- internals --------

    def _send(self, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        assert self._proc.stdin is not None
        self._proc.stdin.write(header + body)
        self._proc.stdin.flush()

    def _recv(self, timeout: float) -> dict:
        # Use select() before every read so the deadline is actually honoured.
        # The old pattern of checking time.monotonic() *after* read(1) would
        # block indefinitely when the server stopped responding (hung or slow).
        deadline = time.monotonic() + timeout
        assert self._proc.stdout is not None
        fd = self._proc.stdout.fileno()

        def _read_bytes(n: int) -> bytes:
            """Read exactly n bytes, respecting the outer deadline."""
            buf = b""
            while len(buf) < n:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("timed out reading from server")
                ready, _, _ = select.select([fd], [], [], remaining)
                if not ready:
                    raise TimeoutError("timed out reading from server")
                chunk = self._proc.stdout.read(n - len(buf))
                if not chunk:
                    stderr_tail = "".join(self._stderr_lines[-50:])
                    raise RuntimeError(
                        "server stdout closed unexpectedly. "
                        f"recent stderr:\n{stderr_tail}"
                    )
                buf += chunk
            return buf

        # Read the header one byte at a time until we see \r\n\r\n.
        header_bytes = b""
        while b"\r\n\r\n" not in header_bytes:
            header_bytes += _read_bytes(1)

        headers_str = header_bytes.decode("ascii")
        content_length = 0
        for line in headers_str.split("\r\n"):
            if line.lower().startswith("content-length:"):
                content_length = int(line.split(":", 1)[1].strip())
                break
        if content_length <= 0:
            raise RuntimeError(f"missing/invalid Content-Length in {headers_str!r}")

        body = _read_bytes(content_length)
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
