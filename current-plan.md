# Plan — sidecar member-completion fallback for bad-statement contexts

## Bug

Triggering completion at `obj.|` returned 0 items whenever the *enclosing
statement* failed to bind — i.e. exactly the case the recent
`8287f01` "completion sentinel" commit was meant to fix. Reproduced
end-to-end in `examples/uvm-1.2/examples/integrated/apb` with a buffer like
`uvm_pkg::uvm_object a; a.` (cursor at the dot): popup empty, even though
slang's elaborator clearly resolves `a` to `uvm_object` (it emits a
`UnknownMember` diagnostic on the spliced sentinel).

## Root cause

The sentinel splice (`a.` → `a.__mimir_complete__`) gets the *parser* past
the bare dot, but slang's *binder* still rejects the resulting expression
statement (a member access alone isn't a valid statement, the unknown
member adds a hard error, etc.). When that happens slang collapses
`ProceduralBlockSymbol::getBody()` to a `Statement` of kind `Invalid`
(`bad()==1`) with no children. The bound `MemberAccessExpression` and
`NamedValueExpression(a)` exist transiently during diagnostic emission
but are not preserved as the procedural body — `TypeAtCursorFinder`
walks the AST and never reaches them, so `found_type` stays null and
`enumerate_scope_members` is never called.

`VisitBad=true` on the visitor doesn't help: the InvalidStatement has no
descendants to visit.

## Fix

`slang-sidecar/src/main.cpp::handle_complete` (memberAccess branch):

1. **Fast path (unchanged):** run `TypeAtCursorFinder` against the bound
   AST. Wins for valid expressions like `obj.method().|` inside an
   assignment.
2. **New fallback** (only when the fast path returns null): extract the
   LHS identifier name by walking back from the dot over identifier
   chars in the buffer text; find the innermost lexical scope at the
   cursor with the existing `EnclosingScopeFinder`; resolve the name via
   `slang::ast::Lookup::unqualified`; if the resolved symbol is a
   `ValueSymbol`, take its type and enumerate its scope members.

Single-identifier LHS only (`a.`, `this.` later — not yet). Chained access
(`obj.field.subfield.|`) still requires the AST-bound finder to win;
that's the same coverage we had before, just no longer dropping the
single-ident case on the floor.

## Verification

| Case | Before | After |
|---|---|---|
| End-to-end LSP, `uvm_pkg::uvm_object a; a.` in apb workspace | 0 items | 52 items (`get_name`, `print`, `compare`, `pack`, …) |
| Direct sidecar, `a.__mimir_complete__()` against `my_class` | 0 items | 11 items |
| Direct sidecar, `a.compute(7)` (valid expression — fast path) | 12 items | 12 items |
| `cargo test --workspace` | 214 pass | 214 pass |

## Files touched

- `slang-sidecar/src/main.cpp` — added `Lookup.h` include; rewired
  `handle_complete`'s memberAccess branch to fall back to lexical
  lookup when the bound AST has no expression at the cursor.

No protocol change → no `crates/mimir-slang` or `crates/mimir-server`
changes; no sidecar version bump.

## Out of scope (follow-ups)

- Chained LHS (`a.b.|`) when the surrounding statement is bad.
- `this.|` / `super.|` in the fallback path.
- Same recovery for `goto_definition` on the LHS identifier (its finder
  pattern has the same blind spot — the user just hasn't hit it yet).
