# current-plan.md — `textDocument/definition` (go to definition)

Live working plan. Updated as stages land. The README feature checklist
remains canonical for shipped status; this file tracks the multi-stage
work the checklist boxes don't surface yet.

Previous slice (slang sidecar Stages 0–3.1) is shipped and lives in git
history. This plan replaces that file in scope.

---

## Goal

Wire the LSP `textDocument/definition` request so an editor's
"Go to definition" jumps from a SystemVerilog identifier reference to its
declaration. Honour the existing two-backend model: tree-sitter is
always-on, slang is the authoritative resolver when the sidecar is
configured.

Free side-effect: the symbol index built for navigation also satisfies
`textDocument/documentSymbol`, so Stage 1 ships that too.

---

## What "definition" means here

In priority order (covers ~90% of real verification code):

1. Module / interface / program instance → declaration
2. Class type reference → `class` declaration
3. Task / function call → `task` / `function` declaration
4. Typedef use → `typedef` declaration
5. Package-qualified `pkg::sym` → package + member declaration
6. Variable / parameter / port reference → declaration
7. SVA `property` / `sequence` reference → declaration
8. Macro reference (`` `MY_MACRO ``) → `` `define `` site (slang only)

**Out of scope for v1**: hierarchical references (`u_dut.fsm.state`),
virtual interface chasing, generate-block scoping nuances. Stage 3
inherits these from slang.

---

## Architecture

Two resolution paths, picked at request time:

```
goto_definition(uri, pos)
    ├── slang configured + project loaded?  →  slang.definition(uri, pos)
    └── otherwise                           →  syntax index (per-doc + workspace)
```

Tree-sitter path is the floor — always works, single-file precision, no
scope rules. Slang path is the ceiling — full semantic resolution,
cross-file, scope-aware.

---

## Stage 1 — tree-sitter, single-file (confidence check)

Smallest useful slice. Ships go-to-def for any declaration in the same
file as the cursor, plus `documentSymbol` as a freebie.

### `mimir-syntax`: new module `symbols.rs`

- `Symbol { name, kind, name_range, full_range }`. Mirrors LSP shapes
  but lives in this crate (pattern #5 in
  [`.claude/docs/architectural_patterns.md`](./.claude/docs/architectural_patterns.md)
  — don't leak `lsp_types` into `mimir-syntax`).
- `SymbolKind` enum: `Module`, `Interface`, `Program`, `Package`,
  `Class`, `Task`, `Function`, `Typedef`, `Parameter`, `Variable`,
  `Port`, `Property`, `Sequence`, `Covergroup`.
- `pub fn index(tree: &SyntaxTree, rope: &Rope) -> Vec<Symbol>` — walk
  the tree-sitter tree and pull declarations using node kinds
  (`module_declaration`, `class_declaration`, `function_declaration`,
  `task_declaration`, `parameter_declaration`, `data_declaration`,
  `type_declaration`, `package_declaration`, …). Find the name child
  via `child_by_field_name("name")` or kind-specific lookup; the LSP
  range is the *name token*, not the whole decl.
- `pub fn identifier_at(tree, rope, pos) -> Option<&str>` — find the
  leaf `simple_identifier` (or `system_tf_identifier`) covering a byte
  offset. Returns the identifier text borrowed from `tree.source()`.
- Re-export `Symbol`, `SymbolKind` from `lib.rs` (pattern #6).
- Co-located tests (pattern #2): per-kind extraction, identifier-at
  boundaries (start/middle/end of a token, between tokens, in
  whitespace), `pkg::sym` qualifier handling.

### `mimir-server`: cache + handler

- Add `index: Vec<Symbol>` field to `DocumentState` in
  [`crates/mimir-server/src/backend.rs`](./crates/mimir-server/src/backend.rs).
  Populate it inside `reparse_and_publish` right after `parser.parse`
  succeeds. No extra tree walk — the tree is already in hand and
  `mimir_syntax::diagnostics::collect` is already walking it.
- `async fn goto_definition(&self, params: GotoDefinitionParams) -> LspResult<Option<GotoDefinitionResponse>>`
  on `Backend`. Flow:
  1. Read-lock the document store, clone the indexed `Vec<Symbol>` and
     the rope (lock-then-clone, pattern #8).
  2. `mimir_syntax::symbols::identifier_at(tree, rope, pos)` to get the
     name under the cursor.
  3. Linear scan the doc's `Vec<Symbol>` for matches by name (Stage 1
     is single-file; multiple matches all returned).
  4. Convert each `Symbol::name_range` to `lsp_types::Location` via the
     existing `Position` shape — pattern #7 says go through
     `Position::from_byte_offset` once at the boundary.
- `#[instrument(level = "debug", skip_all, fields(uri = %...))]` on the
  handler (pattern #9).
- `async fn document_symbol(&self, params: DocumentSymbolParams) -> LspResult<Option<DocumentSymbolResponse>>`
  using the same cached index. Two-for-one.
- `ServerCapabilities` advertises:
  - `definition_provider: Some(OneOf::Left(true))`
  - `document_symbol_provider: Some(OneOf::Left(true))`

### Tests

- `mimir-syntax/src/symbols.rs`:
  - One test per `SymbolKind` (module, class, task, function, typedef,
    parameter, package, property, sequence, covergroup).
  - `identifier_at`: hits at column 0, last column of identifier, just
    past the identifier (returns `None`), inside whitespace
    (`None`), inside a comment (`None`).
  - Multi-symbol files: returned in source order.
- `mimir-server/src/backend.rs`:
  - Pure helper `resolve_in_index(name, &[Symbol]) -> Vec<Symbol>` so we
    can unit-test without spinning up `tower-lsp`. Cover: no match,
    single match, multiple matches, empty index.
  - `goto_definition` happy path via the same helper plus a constructed
    `GotoDefinitionParams`.

### Stage 1 exit criteria

- `cargo test --workspace` green; ~10–12 new unit tests.
- `cargo clippy --workspace -- -D warnings` clean.
- Manual smoke against the VS Code extension: F12 on a class name in
  the same file jumps to the class declaration's name token.
- README feature checklist: `textDocument/definition` ⬜ → 🚧,
  `textDocument/documentSymbol` ⬜ → ✅.
- `current-plan.md` updated with Stage 1 done; commit lands per
  CLAUDE.md.

---

## Stage 2 — tree-sitter, workspace-wide (filelist + open docs)

Builds on Stage 1's per-doc symbol index by aggregating across files.
Resolution order: same-file index first, then workspace index. Multiple
matches return all `Location`s — VS Code's peek list is the right UX
for a syntactic resolver.

### `mimir-server`: workspace index

- New struct `WorkspaceIndex { by_name: HashMap<String, Vec<(Url, Symbol)>> }`
  on `Backend`, behind `Arc<RwLock<WorkspaceIndex>>`.
- Two population sources, **both used together** (per the user's
  decision):
  1. **Open documents.** Already indexed in Stage 1 — re-aggregate into
     the workspace index whenever `reparse_and_publish` updates a
     `DocumentState`.
  2. **Project filelist files** when `.mimir.toml` is present. Read the
     file from disk on first need, parse with `SyntaxParser`, run
     `mimir_syntax::symbols::index`, cache. Files that are also open
     in-memory take precedence (the open buffer is fresher than disk).
- New module `crates/mimir-server/src/workspace_index.rs` for the
  index struct + population logic. Keeps `backend.rs` from growing
  another responsibility.

### Cache invalidation

- On `did_change` / `did_save` for an open file: re-index that file,
  replace its entries in the workspace index.
- On `did_close`: keep the index entry but mark it as
  "back to disk-sourced" — the on-disk version reappears.
- `workspace/didChangeWatchedFiles`: not implemented yet (it's a
  separate checklist item). Until it is, filelist files that change on
  disk while not open don't refresh until the server restarts. Document
  this limitation explicitly in the rustdoc.

### Resolution

- `goto_definition` flow becomes:
  1. Try same-file index (Stage 1 path).
  2. If no match, query the workspace index.
  3. Return all matching `Location`s, deduplicated by URL+range.
- Pure helper `resolve_definition(name, &doc_index, &workspace_index) -> Vec<(Url, Symbol)>`
  for unit testing.

### Tests

- `workspace_index.rs`:
  - Building from a list of `(Url, Vec<Symbol>)` produces the right
    `by_name` map.
  - Replace-on-update: re-indexing a URL drops its old entries.
  - On-disk fallback: a path not in open docs reads disk via an
    injectable `FnMut(&Path) -> Option<String>` (same seam pattern as
    `assemble_elaborate_params`).
- `backend.rs`:
  - `resolve_definition`: same-file beats workspace; workspace match
    when same-file empty; both empty → `None`.
  - Multi-file case: a `class` declared in one open doc is found from a
    reference in another open doc.

### Stage 2 exit criteria

- F12 on a UVM class name in the apb example resolves across files.
- Workspace index hydrates from `.mimir.toml`'s filelist on `initialize`
  and grows as the user opens additional files.
- README feature checklist: `textDocument/definition` 🚧 → ✅
  (tree-sitter coverage; semantic via slang stays 🚧 pending Stage 3).

---

## Stage 3 — slang-backed, semantic (after Stage 2 ships)

Slang is the authoritative resolver when configured. Falls back to the
syntax index on transport error or empty result.

### `mimir-slang`: extend protocol

In [`crates/mimir-slang/src/protocol.rs`](./crates/mimir-slang/src/protocol.rs):

- New constant `methods::DEFINITION = "definition"`.
- `DefinitionParams { files, include_dirs, defines, top, target_path, target_position }`.
  First four mirror `ElaborateParams` so the sidecar can reuse its
  compilation cache (or recompile, same code path).
- `DefinitionResult { locations: Vec<DefinitionLocation> }` where
  `DefinitionLocation { path: String, range: Range }`.
- `Client::definition(&self, params: &DefinitionParams) -> Result<DefinitionResult>`
  on the Rust side, parallel to `Client::elaborate`.
- Round-trip + defaults tests, identical pattern to `ElaborateParams` /
  `ElaborateResult`.

### Sidecar (C++)

Tracked separately from this repo's tree, but the protocol shape and
the sidecar binary are co-owned. Implementation uses slang's symbol
lookup at a `SourceLocation` to find the declaration. Hierarchical
names work for free — slang already resolves them.

### `mimir-server`: routing

- `goto_definition` flow becomes:
  1. If `slang.is_some()` and project loaded: call
     `slang.definition(...)` with the same `ElaborateParams` shape used
     for diagnostics, plus `target_path` + `target_position`.
  2. On success with non-empty result: return those locations.
  3. On transport error or empty result: fall back to the syntax index
     (Stage 1 + Stage 2 path).
- Lock-then-clone for the document snapshot, identical to
  `schedule_elaborate`.
- No debounce — `definition` is request/response, not a push.

### Tests

- Routing logic in a pure helper:
  `select_definition_backend(slang_active, project_loaded) -> Backend`.
- Fallback path: simulated `Err(...)` from the slang client returns the
  syntax index's locations.
- Empty-result path: `Ok(DefinitionResult { locations: vec![] })` →
  fall back, not return empty.

### Stage 3 exit criteria

- F12 on a hierarchical reference (`u_dut.fsm.state`) in the apb
  example lands on the right register declaration.
- F12 inside a generate block respects scope (a same-named symbol in a
  different generate doesn't show as a candidate).
- README feature checklist: `textDocument/definition` ✅ stays ✅; note
  in the surrounding text that semantic resolution is now active when
  slang is configured.

---

## Open questions (resolved)

1. **Land Stage 1 first as confidence check, then Stage 2 in the same
   week. Stage 3 is a follow-up after the C++ sidecar protocol grows
   the new method.**
2. **Workspace index pulls from filelist files *plus* all open
   documents.** Open buffers take precedence over disk for files in
   both sets.
3. **`documentSymbol` ships in Stage 1** — same data, free checkbox.

---

## Out of scope (deliberately, for this slice)

- `textDocument/declaration`, `textDocument/typeDefinition`,
  `textDocument/implementation`. Same machinery, separate slices once
  go-to-def proves the architecture.
- `textDocument/references` (find-usages). Reverse direction; needs a
  second index keyed by reference-site, not by declaration name.
- `workspace/symbol`. Same symbol index as `documentSymbol` but
  cross-file with fuzzy matching; defer until Stage 2 lands the
  workspace index.
- `workspace/didChangeWatchedFiles` integration. Filelist files that
  change while not open don't refresh until restart; live with this for
  now.
- Macro definitions (`` `define ``) — preprocessor-level, slang-only,
  picked up "for free" once Stage 3 routes to slang.

---

## Status table

| Stage                                   | State    | Notes                                                  |
| --------------------------------------- | -------- | ------------------------------------------------------ |
| 1. Tree-sitter, single-file + docSymbol | done     | shipped in commit `def6099`; `documentSymbol` came along for the ride |
| 2. Tree-sitter, workspace (filelist+open) | done     | `WorkspaceIndex` + eager hydration on `initialize`; same-file precedence preserved |
| 3. Slang-backed semantic resolution     | done     | `definition` method + `Client::definition`; trust-slang-on-empty, syntax fallback only on transport error. Sidecar implementation tracked separately. |
