"""Time `mimir/expandMacro` through Emacs' LSP client (eglot), not VS Code.

The user reported macro expansion feeling "clunky and slow" in VS Code. This
test drives the *exact same* server request through Emacs' built-in eglot
JSON-RPC client (see ``tests/emacs_expand_timing.el``) so the measurement
excludes everything VS-Code-specific — the ``mimir-expand:`` content provider,
the read-only tab, markdown rendering, the progress notification. If expansion
is fast here but slow in VS Code, the bottleneck is the extension, not the
server.

It builds a throwaway project that puts riscv-dv's UVM compilation unit plus
UVM-1.2 on the include path (riscv-dv's own ``.mimir.toml`` ships no UVM, so
``uvm_*`` macros aren't reachable there), opens the real
``riscv_vector_cfg.sv``, and expands ``uvm_field_queue_int``.

Run manually:

    MIMIR_SLANG_PATH=slang-sidecar/build/mimir-slang-sidecar \\
        python3 -m unittest tests.test_emacs_expand -v

Skips cleanly when emacs, the riscv-dv / uvm-1.2 examples, the release server
binary, or the slang sidecar are missing.
"""

from __future__ import annotations

import os
import pathlib
import re
import shutil
import subprocess
import sys
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
EL = REPO_ROOT / "tests" / "emacs_expand_timing.el"
SERVER_BIN = REPO_ROOT / "target" / "release" / "mimir-server"
RISCV_DV = REPO_ROOT / "examples" / "riscv-dv"
UVM_MACROS = REPO_ROOT / "examples" / "uvm-1.2" / "src" / "uvm_macros.svh"

# The server round trip (even cold, expanding the ~1000-line uvm_object_utils
# block) is sub-second; this bound is generous for slow CI but far below the
# "minutes" / "clunky" the VS Code path was reported at.
_COLD_BUDGET_S = 10.0


class EmacsExpandTimingTest(unittest.TestCase):
    def setUp(self) -> None:
        if shutil.which("emacs") is None:
            raise unittest.SkipTest("emacs not installed")
        slang = os.environ.get("MIMIR_SLANG_PATH")
        if not slang:
            raise unittest.SkipTest("MIMIR_SLANG_PATH not set — expansion needs the sidecar")
        if not SERVER_BIN.is_file():
            raise unittest.SkipTest(
                f"release server not built at {SERVER_BIN} "
                "(cargo build --release -p mimir-server)"
            )
        if not (RISCV_DV / "src" / "riscv_vector_cfg.sv").is_file():
            raise unittest.SkipTest("riscv-dv example not cloned")
        if not UVM_MACROS.is_file():
            raise unittest.SkipTest("uvm-1.2 example not present")

    def test_expand_uvm_macro_through_emacs_is_fast(self) -> None:
        env = dict(os.environ)
        env["MIMIR_BIN"] = str(SERVER_BIN)
        proc = subprocess.run(
            ["emacs", "-Q", "--batch", "-l", str(EL)],
            cwd=str(REPO_ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=180,
        )
        out = proc.stdout + proc.stderr
        lines = [l for l in out.splitlines() if l.startswith("EMACS_EXPAND")]
        self.assertTrue(lines, f"no EMACS_EXPAND output; emacs said:\n{out[-2000:]}")

        skip = next((l for l in lines if "skip=" in l), None)
        if skip:
            raise unittest.SkipTest(skip)

        result = next((l for l in lines if l.startswith("EMACS_EXPAND cold=")), None)
        self.assertIsNotNone(
            result,
            "emacs never produced a real expansion:\n" + "\n".join(lines),
        )

        m = re.search(
            r"cold=([\d.]+) repeats=([\d.,]+) name=(\S+) lines=(\d+)", result
        )
        self.assertIsNotNone(m, f"could not parse: {result!r}")
        cold = float(m.group(1))
        repeats = [float(x) for x in m.group(2).split(",")]
        name = m.group(3)
        lines_n = int(m.group(4))

        # Surface the numbers for the VS-Code-vs-server comparison.
        print(
            f"\n[emacs expand] {name}: cold={cold:.3f}s "
            f"warm={['%.3f' % r for r in repeats]} lines={lines_n}",
            file=sys.stderr,
        )

        self.assertEqual(name, "uvm_object_utils_begin")
        self.assertGreater(lines_n, 100, "expected the full utility block")
        self.assertLess(
            cold, _COLD_BUDGET_S,
            f"cold expand took {cold:.2f}s through emacs — the server, not VS "
            "Code, would then be the bottleneck",
        )
        self.assertLess(
            max(repeats), 2.0,
            "warm (cached) expansion should be near-instant through the LSP client",
        )
