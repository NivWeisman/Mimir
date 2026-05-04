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

## Stage 4 — Macro `` `define `` resolution (slang only)

The last user-visible F12 gap. F12 on `` `MY_MACRO `` returns nothing
in either backend today: tree-sitter doesn't index `` `define `` sites
(preprocessor directives are below the grammar's interest), and slang's
AST contains the *expanded* tokens — the `` `MY_MACRO `` reference is
gone before the AST is built. Slang does retain everything we need
elsewhere: macro references survive as directive trivia on tokens
(`MacroUsageSyntax`), and `Preprocessor::getDefinedMacros()` returns
every `DefineDirectiveSyntax*` it has seen.

Tree-sitter fallback for macros stays out-of-scope (see
[Out of scope](#out-of-scope-deliberately-for-this-slice)) — "macros
require slang" is consistent with the existing semantic story and
tree-sitter-verilog models `` `define `` weakly anyway.

### Sidecar — cursor-on-macro detection

In [`slang-sidecar/src/main.cpp::handle_definition`](./slang-sidecar/src/main.cpp),
**before** invoking `DefinitionFinder.visit(...)`:

1. Walk every token in the cursor's `SyntaxTree` (the one whose buffer
   matches `target_path`'s `BufferID`). For each token, scan its
   leading and trailing trivia.
2. If a trivia entry's `directive()` is a `MacroUsageSyntax` whose
   `sourceRange()` covers the cursor, capture the macro name from the
   trivia's `directive` token text (strip the leading `` ` ``).
3. Look the name up in `Preprocessor::getDefinedMacros()`. Find the
   `DefineDirectiveSyntax*` whose `name.valueText()` matches.
4. Return its `name.range()` as a single `DefinitionLocation`. **Skip
   the AST visitor entirely** when a macro reference is hit — we have
   the answer; running `DefinitionFinder` would either find nothing
   (the reference isn't in the AST) or, worse, find a same-named symbol
   inside the macro's expansion site.
5. If no macro reference covers the cursor, fall through to
   `DefinitionFinder` exactly as today. (`\`MY_MACRO.field` keeps
   working — only the macro-name span hits step 4; the `.field` part
   misses and falls through.)

### Helper shape

```cpp
// Returns the `DefineDirectiveSyntax*` for the macro whose reference
// spans `cursor`, or nullptr if the cursor isn't on a macro reference.
//
// Walks tokens in source order and inspects directive trivia. Cheap —
// O(tokens) with early exit on first hit; the cursor's compilation
// unit is typically a single file.
const slang::syntax::DefineDirectiveSyntax*
find_macro_at_cursor(const slang::syntax::SyntaxTree& tree,
                     const slang::ast::Compilation& compilation,
                     slang::SourceLocation cursor);
```

The trivia walk is the right shape for hover-show-expansion and a
future "expand macro" code action too — build the helper once.

### Why not a text scan?

A "scan backwards from cursor for backtick-then-identifier" is shorter
to write but mishandles:

- Backticks inside string literals (`"foo `bar"`) — slang's tokeniser
  already disambiguates this; the trivia walk inherits the answer.
- Macro references inside expanded macros — `MacroUsageSyntax::sourceRange`
  is in the *original* buffer, exactly where the user's cursor lives.

### What stays the same

- **`crates/mimir-slang/src/protocol.rs`** — wire shape unchanged. The
  result is just another `DefinitionLocation`.
- **`crates/mimir-server/src/backend.rs`** — no Rust logic changes.
  `try_slang_definition` already returns whatever locations the sidecar
  emits; `route_definition`'s outcomes already cover this.
- **Tests** — no new Rust unit tests; the routing layer hasn't
  changed. Manual sidecar smoke is the validation (consistent with
  this slice's "pure-Rust tests only" rule).

### Open questions

1. **Cross-file macros.** Macros defined in `defines.svh` and
   referenced in `dut.sv` need both files in the request's `files`
   list. `assemble_elaborate_params` already pulls everything from the
   filelist, so this should "just work" — verify with a UVM project
   that splits defines into a header.
2. **`+define+NAME=VALUE` from `.mimir.toml`.** No
   `DefineDirectiveSyntax` source location. The lookup returns nothing
   → empty `locations` array → trust-slang-on-empty short-circuits to
   "no definition." That matches user expectation: there's nowhere
   meaningful to jump to.
3. **Built-in macros (`` `__FILE__ ``, `` `__LINE__ ``).** Same — no
   syntax, return empty.
4. **Where to source `getDefinedMacros()`.** The `Compilation`
   accumulates trees; each tree has its own `Preprocessor`. For lookup
   we want the *cursor's* tree's preprocessor — that's the one that
   saw all the `` `define ``s reachable from the file being edited
   (preceding files in the filelist contribute their definitions via
   `inheritedMacros`). Resolve by matching `cursor.buffer()` against
   each tree's root buffer, then call `tree.getMetadata().defines`
   (TBC — verify exact API surface during implementation).

### Stage 4 exit criteria

- F12 on `` `MY_MACRO `` lands on the `define` site, including
  cross-file (define in a header pulled in via the filelist).
- F12 on `\`MY_MACRO.field`'s `.field` part still resolves through the
  AST path (sanity check the fall-through).
- README's `textDocument/definition` ✅ stays ✅; the surrounding
  sentence drops "macro `` `define `` resolution still deferred."
- `current-plan.md` Status table flips Stage 4 to `done`.

### Estimated size

~50–80 lines C++ (one helper + two-line splice into `handle_definition`),
no Rust changes, no protocol changes. Smaller than Stage 3.2.

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
- Tree-sitter coverage of `` `define `` macros. Macro resolution is
  slang-only by design (see Stage 4); the syntactic backend doesn't
  index preprocessor directives, and no fallback is planned.

---

## Status table

| Stage                                   | State    | Notes                                                  |
| --------------------------------------- | -------- | ------------------------------------------------------ |
| 1. Tree-sitter, single-file + docSymbol | done     | shipped in commit `def6099`; `documentSymbol` came along for the ride |
| 2. Tree-sitter, workspace (filelist+open) | done     | `WorkspaceIndex` + eager hydration on `initialize`; same-file precedence preserved |
| 3. Slang-backed semantic resolution     | done     | `definition` method + `Client::definition`; trust-slang-on-empty, syntax fallback only on transport error. Sidecar handler ships the same change. |
| 3-sidecar. C++ sidecar `definition` handler | done | `slang-sidecar/src/main.cpp` `handle_definition` — `ASTVisitor` over `NamedValue` / `HierarchicalValue` / `MemberAccess` expressions; declaration site via `Symbol::getSyntax()->sourceRange()`; path round-trip preserved by reverse-lookup against the request's `files`. Type-references and module-instantiations deferred. |
| 3.2. Finishing slice                    | done     | Tree-sitter symbol kinds extended (`EnumMember`, `Method`, `Constraint`); class-internal `function`/`task` retagged as `Method` via inside-class flag in the walker. `documentSymbol` now uses the nested form (class methods nest under their class). `goto_definition` routing factored into pure `route_definition` with all four outcomes covered by unit tests. C++ `DefinitionFinder` extended for `CallExpression` (subroutine calls), `InstanceSymbol` (cursor on the type token of `m u_inst()`), and `ValueSymbol`-derived type references (`my_t x;` → typedef/class). |
| 3.3. apb-fixture F12 fix                | done     | Two real bugs surfaced when smoke-testing F12 against `examples/uvm-1.2/.../apb`: (a) the fixture's `apb.f` hardcoded paths under `~/Downloads/uvm-1.2/`, so `target_path` never matched any compilation buffer — switched to repo-relative paths (`../../../src/uvm.sv`, `+incdir+.`); (b) more critically, even with the right paths slang ends up with **two buffers per file** when the open editor buffer is also reachable via `` `include `` (one from `assignText`, one from the preprocessor's disk read), and `DefinitionFinder::covers_target` keyed on `BufferID` so the cursor (in the seeded buffer) and the AST nodes (in the include'd buffer) never met. Switched cursor identity to **path + byte-offset** using `SourceManager::getFullPath` (not `getFileName`, which proximises `` `include ``'d files to bare filenames). Same `getFullPath` fallback added to `symbol_to_definition_location` for declaration sites in `` `include ``'d files. Added `handle(const ClassType&)` for base-class references in `extends` clauses. F12 now resolves on `apb_sequencer`, `apb_master`, `apb_monitor`, `apb_vif`, and `uvm_agent` from `apb_agent.sv`. |
| 4. Macro `` `define `` resolution        | done     | `MacroWalker` (SyntaxVisitor over directive trivia) + `find_macro_at_cursor` helper added before `DefinitionFinder` in `handle_definition`. Strips backtick from `MacroUsageSyntax::directive.rawText()` to key into `SyntaxTree::getDefinedMacros()`. Cross-file defines handled by searching all trees. No Rust changes; no protocol changes. |
