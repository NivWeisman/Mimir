#!/usr/bin/env python3
"""A scriptable stand-in for the slang sidecar, for hermetic server tests.

Speaks the same NDJSON protocol as ``mimir-slang-sidecar`` (one JSON request
per stdin line, one JSON response per stdout line) but does no elaboration.
That lets the integration suite exercise the server's *sidecar plumbing* —
request assembly, diagnostic publishing, caching, lifecycle — without a slang
build, and with fully predictable results.

Behaviour
---------
``compile``
    Returns one AST file entry per request file (no declarations, no
    references) and one ``FakeError`` diagnostic for every line of every
    *compilation-unit* file that contains the marker ``SLANG_ERROR_HERE``.
``expandMacro``
    Always ``{"found": false}``.
``shutdown``
    Replies ``null`` and exits.

Slow mode
---------
``FAKE_SIDECAR_COMPILE_DELAY_MS`` makes every ``compile`` take that long,
standing in for a real project whose elaboration takes seconds.

Request log
-----------
When ``FAKE_SIDECAR_LOG`` names a file, every request is appended to it as
one JSON line::

    {"method": "compile",
     "files": [{"path": ..., "bytes": ..., "cu": true, "marker": false}, ...]}

Tests read it to assert on exactly what the server sent. The server spawns
*two* sidecar processes (compile + expand); both append to the same log, and
each line is written with a single ``write`` so entries never interleave.
"""

from __future__ import annotations

import json
import os
import sys
import time

MARKER = "SLANG_ERROR_HERE"


def _log(entry: dict) -> None:
    path = os.environ.get("FAKE_SIDECAR_LOG")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(entry) + "\n")


def _compile(params: dict) -> dict:
    files = params.get("files", [])
    diagnostics = []
    ast_files = []
    for f in files:
        path = f.get("path", "")
        text = f.get("text", "")
        ast_files.append({
            "uri": path,
            "diagnostics": [],
            "top_scope": {
                "range": {"start": {"line": 0, "character": 0},
                          "end": {"line": 999999, "character": 0}},
                "declarations": [],
                "children": [],
                "imported_packages": [],
            },
            "references": [],
        })
        if not f.get("is_compilation_unit", True):
            continue
        for line_no, line in enumerate(text.split("\n")):
            col = line.find(MARKER)
            if col < 0:
                continue
            diagnostics.append({
                "path": path,
                "range": {"start": {"line": line_no, "character": col},
                          "end": {"line": line_no, "character": col + len(MARKER)}},
                "severity": "error",
                "code": "FakeError",
                "message": "fake slang error",
            })
    return {"ast": {"files": ast_files}, "diagnostics": diagnostics}


def main() -> int:
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
        except ValueError:
            continue
        method = req.get("method", "")
        params = req.get("params") or {}
        _log({
            "method": method,
            "files": [
                {
                    "path": f.get("path", ""),
                    "bytes": len(f.get("text", "").encode("utf-8")),
                    "cu": f.get("is_compilation_unit", True),
                    "marker": MARKER in f.get("text", ""),
                }
                for f in params.get("files", [])
            ],
        })
        resp: dict = {"id": req.get("id", 0)}
        if method == "compile":
            delay_ms = int(os.environ.get("FAKE_SIDECAR_COMPILE_DELAY_MS", "0") or 0)
            if delay_ms:
                time.sleep(delay_ms / 1000.0)
            resp["result"] = _compile(params)
        elif method == "expandMacro":
            resp["result"] = {"found": False}
        elif method == "shutdown":
            resp["result"] = None
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()
            return 0
        else:
            resp["error"] = {"code": -32601, "message": f"method not found: {method}"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
