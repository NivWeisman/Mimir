# current-plan.md — `textDocument/typeDefinition` + `textDocument/implementation`

Two new navigation features built on top of the shipped `definition` pipeline.
Both are slang-only (no tree-sitter fallback — tree-sitter has no semantic type
info). If `MIMIR_SLANG_PATH` is not set the server returns `None`; same as the
trust-slang-on-empty policy already in place.

---

## Stage 1 — `textDocument/typeDefinition` ✅

**User-visible**: cursor on a variable / port / parameter / class-field
reference → jump to the *type's* declaration (typedef, class, enum, struct,
packed union). Cursor on something without a meaningful type (module name,
macro) → returns `None`.

### What changed

| Layer | Change |
| ----- | ------ |
| `slang-sidecar/src/main.cpp` | `TypeDefinitionFinder` struct + `handle_type_definition` + `"typeDefinition"` branch in main loop |
| `crates/mimir-slang/src/protocol.rs` | `methods::TYPE_DEFINITION`, `TypeDefinitionParams/Result/Location` types + tests |
| `crates/mimir-slang/src/client.rs` | `Connection::type_definition`, `Client::type_definition` |
| `crates/mimir-slang/src/lib.rs` | Re-export new types |
| `crates/mimir-server/src/backend.rs` | `type_definition_provider` capability, `SlangTypeDefinitionOutcome`, `TypeDefinitionRoute`, `route_type_definition`, `try_slang_type_definition`, `goto_type_definition`, routing tests |
| `Cargo.toml` | `0.4.1` → `0.5.0` workspace; `mimir-slang` + `mimir-server` `0.4.0` → `0.5.0` |
| `README.md` | `⬜ textDocument/typeDefinition` → ✅ |

---

## Stage 2 — `textDocument/implementation` ✅

**User-visible**, in priority order:
1. Cursor on a `virtual` / `pure virtual` method → all overrides in subclasses
2. Cursor on a class name → all direct subclasses
3. Non-virtual methods / leaf classes → returns `None`

### What changed

| Layer | Change |
| ----- | ------ |
| `slang-sidecar/src/main.cpp` | `collect_all_class_types`, `find_subroutine_at_cursor`, `handle_implementation` + `"implementation"` branch |
| `crates/mimir-slang/src/protocol.rs` | `methods::IMPLEMENTATION`, `ImplementationParams/Result/Location` + tests |
| `crates/mimir-slang/src/client.rs` | `Connection::implementation`, `Client::implementation` |
| `crates/mimir-slang/src/lib.rs` | Re-export new types |
| `crates/mimir-server/src/backend.rs` | `implementation_provider` capability, routing types + functions + handler + tests |
| `Cargo.toml` | `0.5.0` → `0.6.0` workspace; `mimir-slang` + `mimir-server` `0.5.0` → `0.6.0` |
| `README.md` | `⬜ textDocument/implementation` → ✅ |

---

## Status

| Stage | State |
| ----- | ----- |
| 1. typeDefinition | done |
| 2. implementation | done |
