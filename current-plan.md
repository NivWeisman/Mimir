# Plan — `textDocument/documentHighlight`

## Goal

Highlight every occurrence of the identifier under the cursor in the same
file. v1 is text-based: token-level equality, no scoping. Powers the
"select-name → all uses light up" UX in editors.

## What landed in this commit

- `crates/mimir-syntax/src/symbols.rs` — adds
  `pub fn occurrences_of(tree: &SyntaxTree, rope: &Rope, name: &str) -> Vec<Range>`
  alongside `identifier_at`. Walks the tree pre-order; for every node of
  kind `"simple_identifier"` or `"system_tf_identifier"`, slices the source
  with `tree.source().get(node.byte_range())` and pushes `node_range(...)`
  on full-string match. Identifier nodes are leaves, so the walk doesn't
  descend through them. Returns empty for an empty `name`.
- `crates/mimir-server/src/backend.rs` — adds
  `document_highlight_provider: Some(OneOf::Left(true))` to
  `ServerCapabilities` (next to `implementation_provider`) and the
  `document_highlight` `LanguageServer` handler. The handler clones the
  document text under the read lock, parses outside the lock, calls
  `identifier_at` to get the cursor name, owns the name (the `&str`
  borrows the tree, and we need to call `occurrences_of` which also
  borrows it), then maps each internal `Range` to `DocumentHighlight {
  kind: TEXT }`.
- `README.md` — feature checklist flipped from ⬜ to ✅.
- `Cargo.toml` — workspace version bumped 0.6.13 → 0.6.14.

## Decisions baked in

- **Token-level matching.** Only `simple_identifier` and
  `system_tf_identifier` nodes are considered. Comments are invisible to
  the tree (so they can't false-match) and string literals are their own
  node kind (so neither do they).
- **Full-string equality.** A query for `foo` does *not* match `foo_bar`
  or `my_foo`. This is the user's expected behaviour for "highlight all
  uses of this exact name."
- **`$display` highlights.** `system_tf_identifier` is included so
  `$display`, `$random`, etc. light up like any other identifier.
- **No scoping yet.** Variables named `x` declared in two different
  functions both come back. Semantic-aware scoping is future work atop
  slang; the `occurrences_in_different_scopes_all_match` test pins this
  v1 contract so the next contributor knows what to revise.
- **`kind: TEXT`.** No read/write distinction in v1.

## Verification

- `cargo test -p mimir-syntax occurrences` — 6 new unit tests pass:
  `finds_all_uses`, `full_token_only`, `unknown_returns_empty`,
  `empty_name_returns_empty`, `in_different_scopes_all_match`,
  `system_tf_identifier`.
- `cargo test --workspace` — 228 tests pass total, no regressions.
- `cargo clippy --workspace -- -D warnings` — clean.
- `cargo check --workspace` — clean.
