# Plan — `textDocument/completion` for mimir

## Context

Mimir already serves `definition`, `typeDefinition`, `implementation`, and
`documentSymbol`. Completion is the next big editor-affordance gap and the
last "must-have" navigation/editing feature for a usable LSP. The shape mirrors
existing handlers: a syntax-only path (tree-sitter symbol index + workspace
index) for cheap, always-on candidates, and a slang-backed path (opt-in via
`MIMIR_SLANG_PATH`) for scope-correct, type-aware results.

The work is split into eight stages, each independently shippable and gated
by a feature-checklist flip in `README.md`. Stages 1–4 are syntax-only and
land without touching the C++ sidecar; stages 5–7 add a single new sidecar
method (`complete`) and extend its dispatch incrementally; stage 8 is polish.

## Reusable building blocks (no new infrastructure needed for stages 1–4)

| Piece | Location | Notes |
|---|---|---|
| Per-file symbol index | `crates/mimir-syntax/src/symbols.rs:112` (`index`) | 17 `SymbolKind`s already mapped |
| Identifier-at-cursor | `crates/mimir-syntax/src/symbols.rs:350` (`identifier_at`) | Reuse for the "what's the user typing" check |
| Workspace index | `crates/mimir-server/src/workspace_index.rs` (`WorkspaceIndex::lookup`) | Exact-match today; needs a `lookup_prefix` for stage 3 |
| Document store | `crates/mimir-server/src/backend.rs:95` (`Arc<RwLock<HashMap<Url, DocumentState>>>`) | `DocumentState.index: Vec<Symbol>` is kept fresh per parse |
| Routing pattern | `crates/mimir-server/src/backend.rs:698` (`goto_definition` + `DefinitionRoute`) | Mirror exactly: try-slang → syntax-fallback on transport error |
| Slang client/protocol | `crates/mimir-slang/src/{client.rs,protocol.rs}` | Generic `request<P,Res>(method, params)` — adding a method is ~20 lines per side |
| Slang scope APIs (C++) | `Scope::lookupName`, `Scope::membersOfType<T>` (slang headers) | Not yet used by sidecar; existing `DefinitionFinder` visitor at `slang-sidecar/src/main.cpp:512` is the place to record enclosing scope |

## Stages

### Stage 1 — Wire the capability (skeleton)

**Goal:** Editors see `completionProvider` and call us; we return an empty
list. End-to-end plumbing only — no candidates yet.

- `crates/mimir-server/src/backend.rs`
  - In `initialize` (around line 562, where other capabilities are set), add:
    ```rust
    completion_provider: Some(CompletionOptions {
        trigger_characters: Some(vec![".".into(), "`".into(), "$".into()]),
        resolve_provider: Some(false),
        ..Default::default()
    }),
    ```
  - Add `async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>>` returning `Ok(None)`.
- README feature checklist: flip `textDocument/completion` from ⬜ to 🚧.
- Test: smoke test that the handler is callable and returns `Ok(None)`.

**Done when:** VS Code shows the completion popup at all (even empty) and no LSP errors are logged.

### Stage 2 — Syntax-only same-file completion

**Goal:** Trigger with any letter; return matching declarations from the current file.

- `crates/mimir-syntax/src/symbols.rs`
  - Add `pub fn prefix_at(rope: &Rope, pos: Position) -> Option<&str>` — read line text up to cursor, take the trailing `[A-Za-z_$][A-Za-z0-9_$]*`. Rope-based, parse-tree-independent (robust against stale trees / parse errors). Co-located test.
  - Add `pub fn symbol_kind_to_completion_kind(k: SymbolKind) -> CompletionItemKind` — pure mapping. (Lives in syntax to keep `mimir-server` thin; uses `tower_lsp::lsp_types::CompletionItemKind` re-exported through a shared types module to avoid pulling tower-lsp into the syntax crate — alternative: do the mapping in `backend.rs` and keep `mimir-syntax` LSP-free. Pick the latter to preserve the dependency rule from `CLAUDE.md`.)
- `crates/mimir-server/src/backend.rs`
  - Implement `Backend::completion`:
    1. Read `DocumentState` for the URI (read-lock the docs map, clone the index).
    2. `prefix = symbols::prefix_at(rope, pos).unwrap_or("")` — empty prefix is fine (returns everything, capped).
    3. Filter `state.index` by case-insensitive `starts_with`, cap at e.g. 200 items, build `CompletionItem { label, kind, ... }`.
  - Add a private `fn syntax_completion(&self, uri, pos) -> Option<CompletionResponse>` to keep the routing future-friendly.
- README: leave at 🚧.

**Done when:** Typing `cl` in a file with `class my_class` offers `my_class`.

### Stage 3 — Workspace-wide candidates

**Goal:** Suggestions span the whole compilation unit, not just the open file.

- `crates/mimir-server/src/workspace_index.rs`
  - Add `pub fn lookup_prefix(&self, prefix: &str, limit: usize) -> Vec<&Entry>` — case-insensitive prefix scan over `by_name` keys. (Linear is fine for ~10K symbols; a trie comes only if profiling demands it.)
  - Co-located test.
- `crates/mimir-server/src/backend.rs`
  - Extend `syntax_completion` to merge `WorkspaceIndex::lookup_prefix` results with same-file results, deduping by `(name, url)` (a symbol declared in the current file shouldn't appear twice). Mark cross-file items with `detail = Some(<url-basename>)` so users can tell.
- README: still 🚧.

**Done when:** A class declared in another file in the filelist appears in completion in the current file.

### Stage 4 — SystemVerilog keywords

**Goal:** `mod` completes to `module`, `alw` to `always_ff` etc.

- `crates/mimir-syntax/src/keywords.rs` (new module)
  - `pub const KEYWORDS: &[&str] = &[...]` — full IEEE 1800-2017 reserved word list (~250). Sourced from the LRM Annex B; a static slice, no runtime allocation. Co-located test asserts at least the canonical 50.
  - `pub fn matches_prefix(prefix: &str) -> impl Iterator<Item = &'static str>` — case-insensitive starts_with iterator.
- `crates/mimir-server/src/backend.rs`
  - In `syntax_completion`, append keyword matches with `CompletionItemKind::Keyword`. Sort: keywords *after* symbols (symbols are usually what the user wants).
- README: still 🚧.

**Done when:** Typing `mod` shows `module` alongside any user `module`s.

### Stage 5 — Member access via slang (`obj.` / `pkg::`) ✅ DONE

**Goal:** When cursor is right after `.` or `::`, list members of the LHS — class fields/methods, struct fields, enum members, package symbols. Slang-only (no syntax fallback — without types we'd guess).

- `crates/mimir-slang/src/protocol.rs`
  - Add `pub const COMPLETE: &str = "complete";`
  - Add request/response types:
    ```rust
    pub struct CompleteParams {
        pub files: Vec<SourceFile>,
        pub include_dirs: Vec<String>,
        pub defines: Vec<MacroDefine>,
        pub top: Option<String>,
        pub target_path: String,
        pub target_position: Position,
        pub kind: CompletionRequestKind, // MemberAccess for stage 5
        pub prefix: Option<String>,
    }
    pub enum CompletionRequestKind { Identifier, MemberAccess, Macro }
    pub struct CompleteResult { pub items: Vec<SlangCompletionItem> }
    pub struct SlangCompletionItem { pub label: String, pub kind: u8, pub detail: Option<String> }
    ```
- `crates/mimir-slang/src/client.rs`
  - Add `Connection::complete` and `Client::complete` mirroring `definition` (lines 230–235, 342–348).
- `slang-sidecar/src/main.cpp`
  - Extend dispatch (around line 1207) with `else if (method == "complete") { ... handle_complete(params) ... }`.
  - New `handle_complete`: build the compilation (reuse `build_compilation`), find the AST node at the cursor (extend `DefinitionFinder` to also record the resolved expression's *type* — for `obj.` we want the type of `obj`), then if it's a `ClassType` / `PackedStruct` / `EnumType` / `Package`, iterate members via `membersOfType<ValueSymbol>()`, etc.
  - Filter by `prefix` server-side to keep wire payload small; cap at ~500.
- `crates/mimir-server/src/backend.rs`
  - In `Backend::completion`, detect "after `.` / `::`" by reading rope chars before cursor: `try_slang_member_completion`. If true and slang configured, build params and call `slang.complete(...)`. On transport error, return `None` (no fallback). Otherwise fall through to the syntax path from stages 2–4.
- README: leave at 🚧.

**Done when:** `my_obj.` shows `my_obj`'s class fields and methods; `my_pkg::` shows package members.

### Stage 6 — Scope-aware identifier completion via slang

**Goal:** Replace stages 2–3's "everything by name" with "what's actually visible at this cursor" when slang is configured.

- `slang-sidecar/src/main.cpp`
  - Extend `handle_complete` to handle `kind == Identifier`: walk to the enclosing `Scope` of the cursor, then walk outward (`Scope::asSymbol().getParentScope()` chain), at each level emit `membersOfType<...>()` for variables, parameters, ports, types, subroutines, packages, classes; dedupe by name with inner scopes shadowing outer.
- `crates/mimir-server/src/backend.rs`
  - Mirror the `definition` routing: try slang `complete(Identifier)` first; on `Resolved`, use those items; on `TransportError` or no slang, fall back to the stages 2–4 syntax+workspace+keywords union. Use a `CompletionRoute` enum.
- README: 🚧 → ✅ pending stage 7 if we want macros gated separately, or flip to ✅ here if we ship 7 separately.

**Done when:** Inside `my_class::method`, completion offers `this`, fields, params, but *not* unrelated module-scope vars; outside, the reverse.

### Stage 7 — Macro completion

**Goal:** `` ` `` triggers ``  `define `` name completion across the compilation unit.

- `slang-sidecar/src/main.cpp`
  - Extend `handle_complete` for `kind == Macro`: query the preprocessor's macro table (slang exposes this — `Preprocessor::getMacros()` or equivalent; need to check the exact API). Return names with `kind = CompletionItemKind::Constant` and `detail = "`define"`.
- `crates/mimir-server/src/backend.rs`
  - Detect "cursor right after `` ` ``" similarly to stage 5; route to slang when configured.
  - Fallback (no slang): mimir-syntax already collects `` `define `` sites for stage 4 of the existing definition feature (commit `107ecc1`); reuse that index for a syntax-only macro completion list.
- README: flip `textDocument/completion` to ✅.

**Done when:** Typing `` `MY_ `` offers all `` `define `` names visible in the compilation unit.

### Stage 8 — Polish (deferred, do separately)

Not part of the initial completion ship. Track as separate items later:

- Snippets — `module … endmodule`, `class … endclass`, `always_ff @(posedge clk) begin … end`. Adds `insert_text_format = Snippet` and `$0` cursor markers.
- Fuzzy matching / scoring (subsequence, BurntSushi/fuzzy-matcher).
- `completionItem/resolve` for lazy `documentation` / `detail` (cheap-payload-first).
- UVM-aware boosters (e.g. inside a `phase()` body, prioritize `super.<phase>(phase)`).
- Signature placeholders for task/function calls (`my_task($1, $2)$0`).

## Cross-cutting concerns

- **Dependency rule** (from `CLAUDE.md`): keep `tower-lsp` / LSP types out of `mimir-syntax` and `mimir-core`. Symbol-kind → LSP-kind mapping lives in `backend.rs`.
- **Logging:** every new path uses `tracing::{debug,info,warn,error}` with structured fields, never `println!`. Mirror the `instrument`/`debug!` style in `goto_definition`.
- **Errors:** any new fallible op extends the relevant `*Error` enum. Slang transport errors fall through to syntax (stages 2–4 path); only stage 5 (member access) returns empty on transport error.
- **Testing:** every new public function gets a co-located `#[cfg(test)] mod tests` block. The backend gets at least one async integration test per stage exercising `Backend::completion` end-to-end with a fixture document.
- **Concurrency:** the `goto_definition` pattern (read-lock docs → clone state → drop lock → parse under separate parser mutex) ports cleanly. No new lock ordering.
- **Versioning:** bump `crates/mimir-server/Cargo.toml` patch version per stage; bump the slang sidecar `CMakeLists.txt` version when the protocol changes (stages 5–7).

## Critical files to be modified

| Stage | Files |
|---|---|
| 1 | `crates/mimir-server/src/backend.rs`, `README.md` |
| 2 | `crates/mimir-syntax/src/symbols.rs`, `crates/mimir-server/src/backend.rs` |
| 3 | `crates/mimir-server/src/workspace_index.rs`, `crates/mimir-server/src/backend.rs` |
| 4 | `crates/mimir-syntax/src/keywords.rs` (new), `crates/mimir-syntax/src/lib.rs`, `crates/mimir-server/src/backend.rs` |
| 5 | `crates/mimir-slang/src/protocol.rs`, `crates/mimir-slang/src/client.rs`, `slang-sidecar/src/main.cpp`, `crates/mimir-server/src/backend.rs` |
| 6 | `slang-sidecar/src/main.cpp`, `crates/mimir-server/src/backend.rs` |
| 7 | `slang-sidecar/src/main.cpp`, `crates/mimir-server/src/backend.rs`, `README.md` (✅) |

## Verification

Per stage:

1. `cargo check --workspace`
2. `cargo test --workspace` — new co-located tests must pass
3. `cargo clippy --workspace -- -D warnings`
4. For stages 5–7, rebuild the sidecar (`cmake --build slang-sidecar/build`)
5. Manual: open `editors/vscode` in an Extension Development Host (F5),
   open a `.sv` file, exercise the trigger for that stage:
   - Stage 1: any keystroke → empty popup, no errors in `Output → Mimir`
   - Stage 2: type a prefix of a same-file symbol → suggestion appears
   - Stage 3: type a prefix of a cross-file symbol (filelist) → suggestion appears
   - Stage 4: type `mod` → `module` appears as a Keyword item
   - Stage 5: type `obj.` (with `obj` of class type) → class members
   - Stage 6: inside a `class`, completion is scope-correct (members visible, unrelated module-scope vars not)
   - Stage 7: type `` `MY_ `` → macro names

End-to-end: each stage commit follows the project rule
("New LSP feature → flip its checklist item in `README.md` in the same
commit"). Final stage flips `textDocument/completion` to ✅.

