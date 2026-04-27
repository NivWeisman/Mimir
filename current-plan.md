# current-plan.md — slang sidecar integration

Live working plan. Updated as stages land. The README feature checklist
remains canonical for shipped status; this file tracks the multi-stage
work the checklist boxes don't surface yet.

---

## Where we stopped

- Stage 0 (Rust client + protocol): **done** — `mimir-slang` crate exists with
  `protocol.rs`, `client.rs` (`Connection` framing layer + `Client` process
  layer), full unit tests. `cargo check --workspace` passes.
- Stage 0 (server seam): **done** — `Backend` accepts
  `Option<Arc<SlangClient>>`, `merge_diagnostics(syntax, slang, slang_active)`
  is in place with the conflict policy ("when slang elaborates, syntax errors
  are dropped"), `slang_to_lsp_diagnostic` exists, the `MIMIR_SLANG_PATH`
  env var spawns the sidecar in `main.rs`. The flag is hard-wired to
  `false` today so behaviour is identical to tree-sitter-only mode.
- Stage 1 (build the C++ sidecar): **in progress, interrupted**.
  - `slang-sidecar/CMakeLists.txt` and `src/main.cpp` exist.
  - `cmake -G Ninja -S . -B build -DCMAKE_BUILD_TYPE=Release` ran fine.
  - `cmake --build build` reached ~step 25/152 before being killed
    (slang's own sources just started compiling; mimalloc + fmt linked).
  - No `mimir-slang-sidecar` binary on disk yet.

---

## Stage 1 — finish the C++ build (immediate)

1. Resume the build in background:
   `cmake --build slang-sidecar/build -j$(nproc)` until completion.
   First fresh build of slang on this machine; expect ~15–25 min on
   release, then link.
2. On success, verify `slang-sidecar/build/mimir-slang-sidecar` exists and
   `--help`-style probing isn't needed (the binary speaks NDJSON only).
3. Smoke-test by hand:
   - Send `{"id":1,"method":"elaborate","params":{"files":[{"path":"a.sv","text":"module m; endmodule"}]}}\n` on stdin, expect `{"id":1,"result":{"diagnostics":[]}}`.
   - Send `{"id":2,"method":"elaborate","params":{"files":[{"path":"a.sv","text":"module m endmodule"}]}}\n` (missing `;`), expect at least one diagnostic.
   - Send `{"id":3,"method":"shutdown"}\n`, expect `{"id":3,"result":null}` and a clean exit.
4. End-to-end smoke against the Rust server:
   `MIMIR_SLANG_PATH=…/mimir-slang-sidecar cargo run -p mimir-server` and
   confirm the `slang sidecar spawned` log line appears (no behavioural
   change yet — `slang_active = false` is still hard-wired).
5. Don't commit `slang-sidecar/build/`. `.gitignore` already excludes
   `target/`; add `slang-sidecar/build/` and `slang-sidecar/build.log`.

**Exit criteria.** Binary runs, three hand-fed JSON-RPC requests round-trip,
server log shows "slang sidecar spawned" on a real `MIMIR_SLANG_PATH`.

---

## Stage 2 — project support (planned, not yet started)

Slang needs the **whole compilation unit** to do anything useful for UVM
verification code: a single `.sv` file alone won't elaborate because UVM
classes pull `uvm_pkg`, the testbench pulls `+incdir+` paths, and macros
need `+define+`s. Without project context, calling slang on the open file
is no better than tree-sitter.

### Minimum viable config: `.mimir.toml`

A per-project file at the LSP root. First cut, fields:

```toml
# .mimir.toml
[slang]
filelist     = "sim/uvm.f"        # path (relative to .mimir.toml) to a .f filelist
include_dirs = ["rtl", "verif/inc"] # extra +incdir+ values
defines      = [                   # extra +define+s, name or name=value
    "UVM_NO_DPI",
    "BUS_WIDTH=32",
]
top          = "tb_top"            # optional top module name
```

- Discovery: walk up from `initialize.params.root_uri` looking for
  `.mimir.toml`. If absent, slang stays inactive (tree-sitter only) and we
  log at `info` ("no .mimir.toml; slang disabled for this project").
- Loading: pure Rust, `serde + toml` parsing into a `ProjectConfig` struct.
- Where the code lives: new module `crates/mimir-server/src/project.rs`. A
  full crate is overkill until we have multiple consumers.

### Filelist (`.f`) parsing

Verification standard. Each line is one of:

- A source file path (`./rtl/foo.sv`), possibly with a glob.
- `+incdir+PATH[+PATH...]` — colon/`+`-separated additional include dirs.
- `+define+NAME[=VALUE][+NAME[=VALUE]...]` — preprocessor defines.
- `-f OTHER.f` / `-F OTHER.f` — recursive include of another filelist.
- `//` or `#` line comment.
- Backslash line continuation, env-var expansion (`${VAR}`).

Parser lives in `project.rs` next to `ProjectConfig`. The output is a
fully-resolved `ElaborateParams` (paths absolutised, dedup'd, defines
flattened). Watch the recursion depth — guard against `-f a.f -f a.f`
cycles.

### Tests for Stage 2

- `.mimir.toml` round-trip + missing-file → `Default::default()`.
- `.f` parser: covers each directive, comments, `-f` recursion, cycle guard.
- Discovery walks up at most N parents (cap at 8 to bound an editor that
  opened a single file from `/`).

**Exit criteria.** `Backend::new` (or an `initialize` hook) loads project
config, resolves it to an `ElaborateParams` template (files filled in at
elaborate time from the document store + disk), and logs the resolved
input counts at `info`. Slang still not called from the diagnostic path —
that's Stage 3.

---

## Stage 3 — wire slang into diagnostics with debounce (planned)

### Why debounce

| Source        | Cost per run     | Run on every keystroke?                                     |
| ------------- | ---------------- | ----------------------------------------------------------- |
| tree-sitter   | ~1–10 ms         | Yes. Already does this, gives the responsive squiggles.     |
| slang elaborate | ~100 ms – seconds | **No.** Full preprocess + parse + elaborate over the whole compilation unit. Running on every keystroke wedges the editor's diagnostics. |

So the diagnostic pipeline becomes two-tier:

1. On every `did_change`: parse with tree-sitter, publish syntax diagnostics
   immediately (today's behaviour, unchanged).
2. **Schedule** a debounced slang run for that URI (cancelling any pending
   one). When the timer fires, call `slang.elaborate(project_params + open
   buffers)`, partition the result by file, and re-publish per-URL.

### Debounce design

- Per-URI timer with a default of **350 ms** quiet time. Configurable via
  `[slang] debounce_ms = 350` in `.mimir.toml`.
- Implementation: `documents` already lives behind an `RwLock<HashMap>`.
  Add a sibling `RwLock<HashMap<Url, JoinHandle<()>>>` (name:
  `pending_elaborations`). On `did_change`:
  1. Run tree-sitter and publish (synchronous, fast).
  2. Take the write lock on `pending_elaborations`, `.abort()` any prior
     handle for this URI (idempotent if the task already finished),
     `tokio::spawn` a new task that:
     - `tokio::time::sleep(debounce)`,
     - re-reads the document store snapshot under a read lock,
     - calls `slang.elaborate()` with the current snapshot,
     - publishes per-URL diagnostics (with `slang_active = true`),
     - clears its own entry from `pending_elaborations`.
- Cancellation safety: aborting between the sleep and the `elaborate` call
  is fine — the new task just runs again. Aborting *during* `elaborate`
  drops the response on the floor; the wire `id` mismatches will go to
  `ConnectionError::IdMismatch` for the next caller. Mitigation: only abort
  the *timer phase*; once we've sent the request, let it complete and
  discard the result if a newer one is already pending. Track a
  per-document monotonic "edit generation" and tag each elaborate with it.

### Per-file fan-out

`ElaborateResult.diagnostics` covers every file slang saw — possibly
hundreds. We:

1. Group by `path`.
2. Convert each path back to a `Url` (use the editor-supplied URI for any
   open document; for other files, build `file://<absolute-path>`).
3. Call `client.publish_diagnostics(url, diags, version)` per group.
4. **Crucially**, also publish empty for any URL that previously had
   slang diagnostics but no longer does — otherwise stale errors stick
   around. Track "URLs with non-empty slang diagnostics last cycle" in
   the backend and diff.

### Failure / fallback

- `ClientError::Connection(ConnectionError::is_terminal)` → drop the slang
  `Arc<Client>`, log at warn, fall back to tree-sitter (`slang_active =
  false`). No automatic respawn yet — user restarts the server. (Respawn
  with backoff is a follow-up if this turns out to be common.)
- `ConnectionError::Sidecar` (slang reported a structured error for *our*
  request, e.g. bad params) → log at error with the message, leave the
  client running, fall back to tree-sitter for *this* publish only.

### Tests for Stage 3

- Debounce: rapid edits coalesce — 5 edits in 100 ms produce exactly one
  elaborate call after the timer fires. Use `tokio::test(start_paused = true)`
  + `time::advance` so the test doesn't actually wait.
- Edit-generation tag: a stale slang response (older generation) is
  discarded.
- Per-file fan-out: a result with 3 paths produces 3 publish calls; on
  the next cycle one of those paths is gone → that URL gets an empty
  publish.
- Sidecar terminal failure: client dropped, subsequent edits use
  tree-sitter only.

**Exit criteria.** `did_change` produces tree-sitter diagnostics
immediately; ~350 ms after the user stops typing, slang diagnostics
replace them across all affected files; an idle session uses ~0% CPU.

---

## Out of scope (deliberately, for this slice)

- Sidecar respawn on terminal failure.
- Pull-based diagnostics (`textDocument/diagnostic`).
- Pipelined / out-of-order request dispatch on the wire.
- Caching `Compilation` across requests in the sidecar (would need a
  `Server` class on the C++ side; today each elaborate is from scratch).
- Cross-platform sidecar shipping (Linux build is what we exercise; macOS
  / Windows binaries are CI's job, not this branch's).

---

## Status table

| Stage                            | State        | Notes                                                  |
| -------------------------------- | ------------ | ------------------------------------------------------ |
| 0. Rust protocol + client + seam | done         | `mimir-slang` crate, server holds `Option<Client>`     |
| 1. Build the C++ sidecar         | done         | `slang-sidecar/build/mimir-slang-sidecar` runs; 3-request smoke test green; server logs "slang sidecar spawned" with `MIMIR_SLANG_PATH` set |
| 2. Project config (`.mimir.toml`, `.f`) | done         | `crates/mimir-server/src/project.rs`; 12 unit tests; end-to-end: `.mimir.toml` with `filelist = "sim.f"` resolves to "files=1 include_dirs=2 defines=3 top=Some(...) debounce_ms=350" on initialize |
| 3. Wire elaborate + debounce     | done         | `Backend::schedule_elaborate` per-URI debounced task; `assemble_elaborate_params` + `plan_slang_publishes` are pure helpers with 7 unit tests; end-to-end shows tree-sitter "syntax error" on didOpen instantly and slang's `ExpectedToken: expected ';'` after the configured `debounce_ms` |

---

## Follow-ups (not in scope for this slice)

These were called out in Stage 3's design but deferred to keep the
slice tight; track them here so they don't fall on the floor.

- **Edit-generation tagging.** Today, if a slang request is in flight
  and a new edit lands, the new edit's task waits for the connection
  Mutex; the old request's response is published anyway, then the new
  one supersedes it ~ms later. That's a brief flicker, not a
  correctness bug. Tagging each request with a per-document monotonic
  generation and discarding stale results would eliminate the flicker.
- **Sidecar terminal-failure handling.** `ConnectionError::is_terminal`
  exists; the backend currently logs `error` and keeps the
  `Arc<SlangClient>`. The next edit will fail the same way and log
  again. Right move is: drop the `Arc`, log a one-time warn, and let
  the user restart the server. Even better: respawn with backoff.
- **`is_terminal`-driven fall-back to tree-sitter only.** Tied to the
  above — once slang dies, `slang_published` should be cleared so the
  next tree-sitter publish for those files isn't immediately
  overwritten by stale slang state.
- **README install-the-sidecar section.** The C++ build is documented
  in `slang-sidecar/CMakeLists.txt` but not in the user-facing README.
  Should land before anyone outside the team tries to use slang.
