# Mimir code review — June 2026

> **Status: all findings addressed** in v0.7.36–v0.7.41 (commits
> `b91b5f6`…`99b8ec0`): A1–A4 + B1–B3 + C1/C3 in v0.7.36, C2/C4/C5 in
> v0.7.37, D1/D4/D5/D7/D8 in v0.7.38, D2/D3/D6 in v0.7.39, F1–F5 in
> v0.7.40 (F6 reviewed, intentionally left — env-var names are already
> unique per test), E1 in v0.7.41. Two deliberate scope notes: the
> formatting/range_formatting skeleton merge (D7) was skipped because the
> handlers diverge after the A1 fix, and B1 ships deadlines + framing
> resync but not sidecar respawn-on-hang (tracked as follow-up).

Full-workspace review: every crate read (`backend.rs`, all `mimir-server`
services and feature modules, `mimir-syntax` parser/symbols/calls/semantic
tokens, `mimir-slang` client + protocol, `mimir-ast`, `mimir-core`), plus a
cross-cutting grep pass for duplication, locking, and dead code.

Findings are grouped by category and ordered by severity within each group.
Line numbers reference the working tree at the time of review (v0.7.33).

---

## A. Bugs

### A1. `range_formatting` formats the wrong lines when ifdefs precede the selection

`crates/mimir-server/src/backend.rs:3108-3122` computes `--lines` from the
*original* document (`params.range.start.line + 1`) but runs Verible on the
text *after* `wrap_ifdefs` has inserted pragma lines. Every `` `ifdef ``
wrapped above the selection shifts the real target down, so Verible formats
lines above the user's selection.

**Fix:** map line numbers through the wrap (count pragmas inserted before
`start_line`), or wrap only the selected region.

### A2. Manual `file://` URL construction silently breaks on paths with spaces / non-ASCII

`crates/mimir-server/src/ast_features.rs:335`, `:348`, `:411` do
`Url::parse(&format!("file://{path}")).ok()?` — `Url::parse` rejects
unescaped spaces, so go-to-definition / type-definition into such a file
returns `None` with no log line.

**Fix:** use `Url::from_file_path` (it percent-encodes).
`crates/mimir-server/src/slang_service.rs:617` has the same pattern but only
as a documented last-ditch fallback.

### A3. Filelist tokenizer mis-parses common simulator flags

In `crates/mimir-server/src/filelist.rs` the token loop uses
`strip_prefix("-f")` / `strip_prefix("-F")`, so VCS's `-full64` becomes a
nested filelist named `ull64` → `Io` error → the **entire project load
fails**. Any other unknown `-flag` (`-sverilog`, `-y`, …) falls through the
else branch and is pushed as a *source file*.

**Fix:** require `-f`/`-F` to be a standalone token (path in the next
token), and skip-with-warning unknown `-`-prefixed tokens.

### A4. Config reload never invalidates the elaborate cache (dead invalidation path)

`ElaborateService::invalidate_hash`
(`crates/mimir-server/src/elaborate_service.rs:228`) and
`SlangAdapter::invalidate` (`crates/mimir-server/src/slang_adapter.rs:391`)
are both `#[allow(dead_code)]` — nothing calls them on project reload. A
`.mimir.toml` edit that only changes diagnostics policy / feature toggles
won't re-elaborate; stale diagnostics persist until the next content edit.

**Fix:** call both from the project-reload path.

---

## B. Robustness

### B1. No deadline on sidecar RPCs; `ConnectionError::Timeout` is never constructed

`crates/mimir-slang/src/client.rs:107` documents a timeout ("the connection
guard has been dropped…") but nothing constructs the variant, and no
`tokio::time::timeout` exists anywhere on the compile/expand path. A hung
sidecar blocks `compile` forever (hover was made non-blocking in v0.7.33,
but elaborate still waits indefinitely while holding the connection mutex).

**Fix:** implement the watchdog the variant promises, or delete the variant.
(Matches the known gap recorded for slang resilience: dedicated expand
connection shipped; watchdog still missing.)

### B2. Panic risk in `SlangService`

`compile`/`expand` use `.expect("…without a configured sidecar")` with a
TOCTOU window against `is_configured()`. Should return a typed error instead
of panicking the handler task.

### B3. `ElaborateService::schedule` race

The remove-old-task / spawn / insert-new-task sequence on the pending map is
not atomic; two rapid `did_change`s can interleave and leak an un-cancelled
debounce task.

---

## C. Performance & memory

### C1. Full closed-file cache clone per elaborate

`slang_service` param assembly (Phase 1) does `c.texts.clone()` — clones
every cached file's full text per debounced compile. Use `Arc<str>` values
or borrow under the lock.

### C2. Rope → String → Rope round-trips

Frequent `Rope::from_str(&state.document.text())` in backend handlers and
`crates/mimir-server/src/hierarchy_features.rs` — the rope already exists on
`TextDocument`; pass it (or a `&str` snapshot) through.

### C3. Include-dir existence probe reads the whole file

`crates/mimir-server/src/includes.rs` `expand_includes` probes candidates
with `read(p).is_some()` — a full file read per include-dir candidate just
to test existence. Use `try_exists` / metadata.

### C4. `expansion_cache` unbounded across URLs

`crates/mimir-server/src/slang_adapter.rs` caps the cache at 64 entries
*per document* but it is unbounded across URLs and never evicted on
`did_close`.

### C5. Double read lock in `references`

The handler takes the workspace read lock twice back-to-back
(`crates/mimir-server/src/backend.rs:2031`, `:2050`) — minor; one guard
scope would do.

---

## D. Duplication / reuse opportunities

### D1. Four copies of `range_contains`

- `crates/mimir-server/src/backend.rs:4774`
- `crates/mimir-server/src/slang_adapter.rs:121`
- `crates/mimir-server/src/chain_resolve.rs:35` (comment acknowledges it)
- `crates/mimir-server/src/code_lens.rs:140`

`mimir-ast` already has the model: `MimirRange::contains`
(`crates/mimir-ast/src/types.rs:275`). Add the same method to
`mimir_core::Range` and delete all four.

### D2. Five-plus "scan backwards from cursor for an identifier" implementations

`detect_member_access` + `detect_macro_trigger` (slang_service),
`receiver_ident_before_dot` + `receiver_chain_before_dot` (backend),
`prefix_at` (`crates/mimir-syntax/src/symbols.rs:1871`), `word_at_rope`
(`crates/mimir-server/src/ast_features.rs:70`). One shared cursor-scan
utility in mimir-core/mimir-syntax would replace all of them.

### D3. `references` / `rename` share ~60 lines

`crates/mimir-server/src/backend.rs:1985` and `:2118` both implement
"snapshot open-doc trees + presence-filtered closed-tree collection" —
extract one `collect_workspace_occurrences` helper.

### D4. `scan_includes` vs `scan_includes_with_spans`

`crates/mimir-server/src/includes.rs`: the former is an ~80-line near-copy
of the latter; it should delegate and drop spans.

### D5. `expand_macro` / `expand_macro_if_idle`

Near-duplicate bodies in `SlangAdapter` — share a common core taking a
"blocking vs try" flag.

### D6. Three independent inheritance-chain walkers

`crates/mimir-server/src/chain_resolve.rs`: `find_method_in_class` /
`find_field_in_class` are ~75-line twins differing only in the `DeclKind`
predicate; `crates/mimir-server/src/code_lens.rs` `find_override` is a third
walker. One generic `walk_inheritance(class, pred)` would serve all three.

### D7. Small structural duplications in backend.rs

- `formatting` / `range_formatting` share the whole feature-check → fetch
  doc → wrap → invoke → strip skeleton.
- The `inlay_hint` handler builds the same `InlayHint` struct literal 3×.
- The idiom `uri.to_file_path().ok().and_then(|p| p.to_str().map(str::to_owned))`
  recurs across backend/services.

Three small helpers cover all of these.

### D8. `parse_severity` duplicated with diverging behavior

`crates/mimir-server/src/project.rs:292` falls back to `Warning`;
`crates/mimir-server/src/diag_policy.rs:124` falls back to `Hint`. Same
input string, different result depending on which path parses it — unify in
one place.

---

## E. Architecture-rule violations (per CLAUDE.md)

### E1. backend.rs breaks its own "thin coordinator" rule

~1,600 lines of feature implementation live in
`crates/mimir-server/src/backend.rs:3212-4838` — `hover_for_symbol`,
`syntax_member_completion`, `collect_references`, `resolve_method_symbol`,
receiver-chain parsing, workspace-symbol ranking, semantic-token encoding.
These belong in `ast_features` / new feature modules; several handlers far
exceed the ~10-line non-delegation budget.

---

## F. Docs, dead code, hygiene

### F1. Doc-comment defects in mimir-server

- The `completion_resolve` doc comment sits on the formatting fn
  (`crates/mimir-server/src/backend.rs:3000-3007`).
- Duplicated sentence in the hover doc (`backend.rs:2218-2219`).
- `workspace_index.rs` module doc still claims the "external edits not seen"
  limitation that `didChangeWatchedFiles` fixed.
- `slang_service` doc says "three pieces of state" over four fields.

### F2. protocol.rs broken doc links + likely-dead type

`crates/mimir-slang/src/protocol.rs:107` and `:211` doc-link to
`methods::ELABORATE`, which doesn't exist (renamed `COMPILE`).
`ElaborateResult` is re-exported from `crates/mimir-slang/src/lib.rs:52` but
used nowhere — likely dead since the `compile` RPC returns `CompileResult`.

### F3. Stale "Stage 3" blanket allow

`crates/mimir-server/src/project.rs:678`: whole-struct
`#[allow(dead_code)]` with comment "Stage 3 hasn't started reading these
fields yet" — Stage 3 shipped; the blanket allow now masks genuinely dead
fields.

### F4. completion_score stale attributes

`crates/mimir-server/src/completion_score.rs:33`: `score()` is
`#[allow(unused)]` yet has 6 call sites in backend.rs — the stale attribute
would hide it actually becoming dead. `_case_marker()`
(`completion_score.rs:69`) is a stub kept only to hold an import.

### F5. Contradictory / dead `allow(dead_code)` sites

- `crates/mimir-server/src/syntax_service.rs:55`: `workspace` field is
  `#[allow(dead_code)]` while its doc claims "Read here for cross-file
  resolution" — either the field is unused (remove it) or the allow is stale.
- `crates/mimir-server/src/workspace_index.rs:192`: `lookup_by_location` is
  dead public API.

### F6. Test-only env mutation hazard

`crates/mimir-server/src/filelist.rs` tests mutate process env via
`std::env::set_var` — flaky under parallel test execution; scope with a
serial guard or unique var names.

---

## Clean bills of health

`mimir-core` (document.rs position math incl. surrogate-pair rejection,
logging, debug_timer), `mimir-ast` types, `mimir-slang` client (NDJSON
framing, stale-response drain, dual-sidecar lifecycle — modulo B1) and
protocol (modulo F2), `format.rs`, `parser.rs` (blank-backtick preprocessing
is byte-preserving and well-pinned), `diagnostics.rs`, `diag_policy.rs`,
`hierarchy_features.rs`, `main.rs`. Zero `TODO`/`FIXME` markers in source;
test co-location and wire-compatibility tests are consistently excellent.

## Suggested priority

1. **A1–A4** — user-visible bugs.
2. **B1** — hang risk (sidecar watchdog).
3. **D1 / D2 / D8** — cheap dedups that shrink backend.rs.
4. **E1** — thin-coordinator refactor series.
