#!/usr/bin/env python3
"""A deterministic stand-in for ``verible-verilog-format`` for server tests.

Reads the source from stdin and writes the "formatted" file to stdout, like
Verible with ``-``. Inside the ``--lines=A-B`` window (1-based, inclusive;
the whole file when the flag is absent) it applies two rules that between
them change both line *lengths* and the line *count* — the two things a real
formatter does that a range-format edit has to cope with:

* runs of interior whitespace collapse to one space (indentation is kept);
* a statement continued over several lines (a line that doesn't end in ``;``
  followed by more indented lines) is joined onto one line.
* a ``begin … end`` written on one line is split onto three.

Lines outside the window are echoed untouched.
"""

from __future__ import annotations

import re
import sys


def _format_block(lines: list[str]) -> list[str]:
    out: list[str] = []
    pending: str | None = None
    for line in lines:
        indent = line[: len(line) - len(line.lstrip())]
        body = re.sub(r"\s+", " ", line.strip())
        if pending is not None:
            pending += " " + body
            if body.endswith(";"):
                out.append(pending)
                pending = None
            continue
        if body.startswith("assign") and not body.endswith(";"):
            pending = indent + body
            continue
        m = re.fullmatch(r"(.*\bbegin) (.*;) (end)", body)
        if m:
            out.append(indent + m.group(1))
            out.append(indent + "  " + m.group(2))
            out.append(indent + m.group(3))
            continue
        out.append(indent + body if body else "")
    if pending is not None:
        out.append(pending)
    return out


def main() -> int:
    first, last = None, None
    for arg in sys.argv[1:]:
        if arg.startswith("--lines="):
            a, b = arg[len("--lines="):].split("-")
            first, last = int(a), int(b)
    text = sys.stdin.read()
    trailing_newline = text.endswith("\n")
    lines = text.split("\n")
    if trailing_newline:
        lines.pop()
    if first is None:
        first, last = 1, len(lines)
    head = lines[: first - 1]
    block = lines[first - 1 : last]
    tail = lines[last:]
    result = head + _format_block(block) + tail
    sys.stdout.write("\n".join(result) + ("\n" if trailing_newline else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
