"""Randomised long-running stress test for `mimir-server`.

Simulates several minutes of active development: random file edits (full and
incremental) interleaved with hover, completion, definition, semantic-tokens,
and document-symbol requests on three concurrently-open SV files.

If the server crashes the test fails immediately and prints the full stderr
output — which, when RUST_BACKTRACE=full is set, contains the panic location
and backtrace.

This test is **skipped in CI by default**. Run it manually:

    cargo build --release -p mimir-server
    RUST_BACKTRACE=full RUST_LOG=mimir=debug \\
        python3 -m unittest tests.test_stress -v

Override the run duration:

    MIMIR_STRESS_DURATION=60 python3 -m unittest tests.test_stress -v
"""

from __future__ import annotations

import os
import pathlib
import random
import time
import unittest

from .lsp_client import MimirLspClient, file_uri, read_text


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
RISCV_DV = REPO_ROOT / "examples" / "riscv-dv"

# These three files are large and exercise different SV constructs:
#  - riscv_instr_gen_config.sv  (~797 lines)  : class with rand fields + constraints
#  - riscv_instr_pkg.sv         (~1628 lines) : package with typedefs, enums, structs
#  - riscv_asm_program_gen.sv   (~1646 lines) : class with many tasks and functions
_STRESS_FILES = [
    RISCV_DV / "src" / "riscv_instr_gen_config.sv",
    RISCV_DV / "src" / "riscv_instr_pkg.sv",
    RISCV_DV / "src" / "riscv_asm_program_gen.sv",
]

# How long to run the stress loop, in seconds.  Override with the env var.
_DURATION_S = int(os.environ.get("MIMIR_STRESS_DURATION", "300"))

# Operations and their relative weights.
_OPS = [
    ("edit_insert",    10),  # insert a comment line
    ("edit_delete",     6),  # delete a line
    ("edit_full",       3),  # full-text replace (prevents unbounded drift)
    ("hover",          15),  # textDocument/hover
    ("completion",     12),  # textDocument/completion
    ("definition",     10),  # textDocument/definition
    ("semantic_tokens", 6),  # textDocument/semanticTokens/full
    ("doc_symbol",      8),  # textDocument/documentSymbol
]
_OP_NAMES  = [n for n, _ in _OPS]
_OP_WEIGHTS = [w for _, w in _OPS]


@unittest.skip("run manually: MIMIR_STRESS_DURATION=120 python3 -m unittest tests.test_stress -v")
class StressTest(unittest.TestCase):
    """Drive the LSP server for several minutes with random mixed workloads."""

    @classmethod
    def setUpClass(cls) -> None:
        missing = [f for f in _STRESS_FILES if not f.exists()]
        if missing:
            raise unittest.SkipTest(
                f"stress-test fixtures not found: {missing!r}. "
                "Clone the riscv-dv sub-example first."
            )

        cls.lsp = MimirLspClient(
            env={
                "RUST_BACKTRACE": "full",
                "RUST_LOG": "mimir=debug",
            }
        )
        cls.lsp.initialize(workspace_root=RISCV_DV)

        cls.uris  = [file_uri(f) for f in _STRESS_FILES]
        cls.texts = [read_text(f) for f in _STRESS_FILES]

        for uri, text in zip(cls.uris, cls.texts):
            cls.lsp.did_open(uri, text)

        # Let initial parsing settle before the stress loop starts.
        for uri in cls.uris:
            cls.lsp.wait_for_fresh_diagnostics(uri, timeout=15.0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.lsp.close()

    def test_long_run(self) -> None:
        """Run random mixed LSP operations for _DURATION_S seconds.

        Fails immediately if the server crashes (stdout closes) or any
        request times out. On failure, the last 8 KB of stderr — which
        contains the panic message and backtrace — is printed as the
        assertion message.
        """
        rng = random.Random(0xDEADBEEF)  # deterministic for reproducibility
        version = 10   # start above the initial open version
        edit_count = 0
        op_count = 0
        deadline = time.monotonic() + _DURATION_S

        print(
            f"\n[stress] starting {_DURATION_S}s run on "
            f"{len(self.uris)} files",
            flush=True,
        )

        try:
            while time.monotonic() < deadline:
                idx  = rng.randrange(len(self.uris))
                uri  = self.uris[idx]
                text = self.texts[idx]
                lines = text.splitlines(keepends=True)
                n_lines = len(lines)

                (op,) = rng.choices(_OP_NAMES, weights=_OP_WEIGHTS, k=1)
                print(
                    f"  op={op_count:4d} edit={edit_count:4d} "
                    f"file={idx} op={op}",
                    end="\r",
                    flush=True,
                )

                # ---- mutations --------------------------------------------------
                if op == "edit_insert":
                    row = rng.randrange(max(1, n_lines))
                    new_line = f"// stress-edit-{edit_count}\n"
                    new_text = "".join(lines[:row]) + new_line + "".join(lines[row:])
                    version += 1
                    self.texts[idx] = new_text
                    self.lsp.notify(
                        "textDocument/didChange",
                        {
                            "textDocument": {"uri": uri, "version": version},
                            "contentChanges": [
                                {
                                    "range": {
                                        "start": {"line": row, "character": 0},
                                        "end":   {"line": row, "character": 0},
                                    },
                                    "text": new_line,
                                }
                            ],
                        },
                    )
                    edit_count += 1

                elif op == "edit_delete" and n_lines > 20:
                    # Stay away from the first/last 10 lines to keep valid SV.
                    row = rng.randrange(10, n_lines - 10)
                    new_text = "".join(lines[:row]) + "".join(lines[row + 1:])
                    version += 1
                    self.texts[idx] = new_text
                    self.lsp.notify(
                        "textDocument/didChange",
                        {
                            "textDocument": {"uri": uri, "version": version},
                            "contentChanges": [
                                {
                                    "range": {
                                        "start": {"line": row,     "character": 0},
                                        "end":   {"line": row + 1, "character": 0},
                                    },
                                    "text": "",
                                }
                            ],
                        },
                    )
                    edit_count += 1

                elif op == "edit_full":
                    # Full-sync reset to original disk content — prevents
                    # unbounded document drift caused by accumulated inserts.
                    original = read_text(_STRESS_FILES[idx])
                    version += 1
                    self.texts[idx] = original
                    self.lsp.notify(
                        "textDocument/didChange",
                        {
                            "textDocument": {"uri": uri, "version": version},
                            "contentChanges": [{"text": original}],
                        },
                    )
                    edit_count += 1

                # ---- queries ----------------------------------------------------
                elif op == "hover":
                    row  = rng.randrange(min(n_lines, 200))
                    char = rng.randrange(30)
                    self.lsp.request(
                        "textDocument/hover",
                        {
                            "textDocument": {"uri": uri},
                            "position": {"line": row, "character": char},
                        },
                        timeout=8.0,
                    )

                elif op == "completion":
                    row  = rng.randrange(min(n_lines, 200))
                    char = rng.randrange(30)
                    self.lsp.request(
                        "textDocument/completion",
                        {
                            "textDocument": {"uri": uri},
                            "position": {"line": row, "character": char},
                            "context": {"triggerKind": 1},
                        },
                        timeout=8.0,
                    )

                elif op == "definition":
                    row  = rng.randrange(min(n_lines, 200))
                    char = rng.randrange(30)
                    self.lsp.request(
                        "textDocument/definition",
                        {
                            "textDocument": {"uri": uri},
                            "position": {"line": row, "character": char},
                        },
                        timeout=8.0,
                    )

                elif op == "semantic_tokens":
                    self.lsp.request(
                        "textDocument/semanticTokens/full",
                        {"textDocument": {"uri": uri}},
                        timeout=15.0,
                    )

                elif op == "doc_symbol":
                    self.lsp.request(
                        "textDocument/documentSymbol",
                        {"textDocument": {"uri": uri}},
                        timeout=8.0,
                    )

                op_count += 1

                # Every 10 edits, let diagnostics settle so the notification
                # queue doesn't grow without bound.
                if edit_count > 0 and edit_count % 10 == 0:
                    self.lsp.wait_for_fresh_diagnostics(uri, timeout=5.0)
                    # Drain queued notifications so _notifications list stays small.
                    self.lsp.clear_notifications()

                # Realistic inter-keystroke delay.
                time.sleep(rng.uniform(0.10, 0.35))

        except Exception as exc:
            stderr_tail = self.lsp.stderr_text[-8192:]
            elapsed = _DURATION_S - max(0.0, deadline - time.monotonic())
            raise AssertionError(
                f"\nServer crashed or timed out after {elapsed:.1f}s "
                f"({op_count} operations, {edit_count} edits).\n"
                f"\n{'='*60}\nLast server stderr:\n{'='*60}\n"
                f"{stderr_tail}\n{'='*60}"
            ) from exc

        elapsed = _DURATION_S - max(0.0, deadline - time.monotonic())
        print(
            f"\n[stress] completed {op_count} operations ({edit_count} edits) "
            f"in {elapsed:.1f}s — server alive",
            flush=True,
        )

        # Final health check: server must still respond to a request.
        self.lsp.request(
            "textDocument/documentSymbol",
            {"textDocument": {"uri": self.uris[0]}},
            timeout=8.0,
        )
