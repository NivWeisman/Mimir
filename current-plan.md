# Plan — `textDocument/foldingRange`

## Goal

Implement `textDocument/foldingRange` as a pure tree-sitter feature: walk
the SV syntax tree and emit one foldable line range per top-level
construct so editors can collapse modules, classes, functions, etc.

## What landed in this commit

- `crates/mimir-syntax/src/folding.rs` — new module. Defines
  `FoldRange { start_line, end_line }` (internal mirror of
  `lsp_types::FoldingRange`), a `FOLDABLE_KINDS` const list of
  tree-sitter node kinds, and `folding_ranges(&SyntaxTree) -> Vec<FoldRange>`
  that walks the tree pre-order and emits one range per matching node.
- `crates/mimir-syntax/src/lib.rs` — registers the module and re-exports
  `FoldRange` at the crate root.
- `crates/mimir-server/src/backend.rs` — adds
  `folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true))`
  to `ServerCapabilities`, the `m_fold_to_lsp` boundary helper, and the
  `folding_range` `LanguageServer` handler. The handler reuses the existing
  lock-then-clone pattern from `syntax_definition`: read the doc text
  under the read lock, parse outside the lock, walk, convert.
- `README.md` — feature checklist flipped from ⬜ to ✅ with a one-line
  description of the kinds covered.
- `Cargo.toml` — workspace version bumped 0.6.12 → 0.6.13.

## Decisions baked in

- **Kinds covered:** `module_declaration`, `class_declaration`,
  `function_body_declaration`, `task_body_declaration`,
  `package_declaration`, `interface_declaration`, `program_declaration`,
  `property_declaration`, `sequence_declaration`, `covergroup_declaration`.
  We match `*_body_declaration` (not the wrapper `*_declaration`) for
  functions and tasks, mirroring the disambiguation already used in
  `symbols.rs` to avoid double-emitting.
- **Comments deliberately skipped.** Tree-sitter strips comments before
  building the tree, so folding them would need a separate text scan.
  Deferred until requested.
- **Whole-line folds.** `start_character` / `end_character` are `None` —
  the editor decides exact column placement.
- **Skip threshold.** `end_line > start_line` only; `module m; endmodule`
  on one line emits nothing.
- **`kind: Region`.** The other LSP folding kinds (`Comment`, `Imports`)
  don't fit SV constructs.

## Verification

- `cargo test -p mimir-syntax folding` — 8 new unit tests pass (single
  module, class+method, package+class+method, body-only-not-wrapper,
  task body, property/sequence/covergroup each fold, single-line skip,
  empty source).
- `cargo test --workspace` — 222 tests pass total, no regressions.
- `cargo clippy --workspace -- -D warnings` — clean.
- `cargo check --workspace` — clean.
