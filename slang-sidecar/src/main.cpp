// mimir-slang-sidecar — NDJSON-over-stdio elaborator backed by slang.
//
// Reads one JSON request per line from stdin, calls the matching handler,
// writes one JSON response per line to stdout. All logging goes to stderr
// (stdout is reserved for the protocol — same constraint as an LSP server).
//
// Wire shape mirrors `mimir-slang::protocol`. Methods today:
//   * "elaborate"  — preprocess + parse + elaborate the supplied files,
//                    return diagnostics in LSP coordinates.
//   * "definition" — same compile front-end, plus a cursor (file + LSP
//                    position); returns the declaration site of the
//                    symbol referenced under the cursor.
//   * "shutdown"   — reply with null result, exit cleanly.
//
// Anything else is replied to with `error.code = -32601` ("method not
// found") and the loop continues so a misbehaving client doesn't take the
// sidecar down.
//
// The handler is intentionally a free function (not a class) — there's no
// shared state between requests today. When that changes (e.g. caching a
// `Compilation` across edits) we'll fold it into a Server class.

#include <cstdint>
#include <exception>
#include <iostream>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <utility>
#include <vector>

#include <nlohmann/json.hpp>

#include <slang/ast/ASTVisitor.h>
#include <slang/ast/Compilation.h>
#include <slang/ast/Symbol.h>
#include <slang/ast/expressions/CallExpression.h>
#include <slang/ast/expressions/MiscExpressions.h>
#include <slang/ast/expressions/SelectExpressions.h>
#include <slang/ast/symbols/ClassSymbols.h>
#include <slang/ast/symbols/InstanceSymbols.h>
#include <slang/ast/symbols/SubroutineSymbols.h>
#include <slang/ast/symbols/ValueSymbol.h>
#include <slang/ast/types/DeclaredType.h>
#include <slang/ast/types/Type.h>
#include <slang/diagnostics/DiagnosticEngine.h>
#include <slang/diagnostics/Diagnostics.h>
#include <slang/parsing/Preprocessor.h>
#include <slang/syntax/AllSyntax.h>
#include <slang/syntax/SyntaxKind.h>
#include <slang/syntax/SyntaxNode.h>
#include <slang/syntax/SyntaxTree.h>
#include <slang/syntax/SyntaxVisitor.h>
#include <slang/text/SourceLocation.h>
#include <slang/text/SourceManager.h>
#include <slang/util/Bag.h>

using json = nlohmann::json;

// --- UTF-16 column conversion --------------------------------------------
//
// LSP positions count UTF-16 code units. Slang reports byte columns. For
// ASCII-only SystemVerilog (the overwhelming common case in verification
// code) the two are identical, but identifiers and string literals can
// contain non-ASCII, and getting this wrong shifts diagnostic squiggles.
//
// We slice the prefix of the line up to slang's byte column, then count
// how many UTF-16 code units it would encode to. BMP characters are 1
// code unit; anything above U+FFFF is a surrogate pair (2 code units).
static uint32_t utf8_prefix_to_utf16_units(std::string_view prefix) {
    uint32_t units = 0;
    size_t i = 0;
    while (i < prefix.size()) {
        unsigned char b = static_cast<unsigned char>(prefix[i]);
        if (b < 0x80) {
            units += 1;
            i += 1;
        } else if ((b & 0xE0) == 0xC0) {
            units += 1;
            i += 2;
        } else if ((b & 0xF0) == 0xE0) {
            units += 1;
            i += 3;
        } else if ((b & 0xF8) == 0xF0) {
            // 4-byte UTF-8 → outside the BMP → surrogate pair → 2 units.
            units += 2;
            i += 4;
        } else {
            // Invalid lead byte; skip one byte to make progress. Counts
            // as one unit so positions don't go backwards.
            units += 1;
            i += 1;
        }
    }
    return units;
}

// Inverse of `utf8_prefix_to_utf16_units`. Given a single line of UTF-8
// text and an LSP `character` (UTF-16 code unit count), return the byte
// offset within `line`. A `utf16_char` past the line end clamps to
// `line.size()` — matches the LSP spec where a position can sit one past
// the last character of a line.
//
// Multi-byte sequence boundaries: LSP positions live *between* code
// points, not bytes. A client never points at the middle of a UTF-8
// sequence; if it did, we round forward to the start of the next
// codepoint, which is the same boundary `utf8_prefix_to_utf16_units`
// would treat as one code unit.
static size_t utf16_to_byte_offset(std::string_view line, uint32_t utf16_char) {
    uint32_t units = 0;
    size_t i = 0;
    while (i < line.size() && units < utf16_char) {
        unsigned char b = static_cast<unsigned char>(line[i]);
        if (b < 0x80) {
            units += 1;
            i += 1;
        } else if ((b & 0xE0) == 0xC0) {
            units += 1;
            i += 2;
        } else if ((b & 0xF0) == 0xE0) {
            units += 1;
            i += 3;
        } else if ((b & 0xF8) == 0xF0) {
            units += 2;
            i += 4;
        } else {
            units += 1;
            i += 1;
        }
    }
    return i;
}

// --- LSP position from a slang SourceLocation ----------------------------

struct LspPos {
    uint32_t line;       // 0-based
    uint32_t character;  // 0-based, UTF-16 code units
};

// Convert a slang SourceLocation to an LSP (line, character) pair.
// Macro-expansion locations are mapped back to their originating file
// location — the editor needs to underline the user's source, not slang's
// synthesised expansion buffer.
static LspPos to_lsp_pos(const slang::SourceManager& sm, slang::SourceLocation loc) {
    if (!loc.valid()) {
        return {0, 0};
    }
    auto orig = sm.getFullyOriginalLoc(loc);
    auto line_1based = static_cast<uint32_t>(sm.getLineNumber(orig));
    auto byte_col_1based = static_cast<uint32_t>(sm.getColumnNumber(orig));
    if (line_1based == 0) {
        return {0, 0};
    }

    // Walk the buffer to find the start byte of this line, then count
    // UTF-16 units in [line_start, line_start + byte_col - 1).
    auto buffer_text = sm.getSourceText(orig.buffer());
    size_t cursor = 0;
    uint32_t cur_line = 1;
    while (cur_line < line_1based && cursor < buffer_text.size()) {
        if (buffer_text[cursor] == '\n') {
            ++cur_line;
        }
        ++cursor;
    }

    size_t prefix_bytes = byte_col_1based - 1;
    if (cursor + prefix_bytes > buffer_text.size()) {
        prefix_bytes = buffer_text.size() - cursor;
    }
    auto utf16 = utf8_prefix_to_utf16_units(buffer_text.substr(cursor, prefix_bytes));

    return {line_1based - 1, utf16};
}

// LSP `(line, character)` → byte offset within a slang `SourceBuffer`.
//
// Walks the buffer linewise to find the start of `lsp_line`, then
// converts the UTF-16 `lsp_char` to a byte offset within that line.
// Returns `nullopt` when the line index runs off the end of the buffer.
static std::optional<size_t> lsp_position_to_byte_offset(
    const slang::SourceManager& sm,
    slang::BufferID buffer,
    uint32_t lsp_line,
    uint32_t lsp_char) {
    auto text = sm.getSourceText(buffer);
    size_t cursor = 0;
    uint32_t cur_line = 0;
    while (cur_line < lsp_line && cursor < text.size()) {
        if (text[cursor] == '\n') {
            ++cur_line;
        }
        ++cursor;
    }
    if (cur_line < lsp_line) {
        return std::nullopt;  // line is past EOF
    }

    // Slice the line out (without trailing newline) so the UTF-16 walk
    // doesn't run into the next line's bytes.
    size_t line_end = cursor;
    while (line_end < text.size() && text[line_end] != '\n') {
        ++line_end;
    }
    auto line_view = text.substr(cursor, line_end - cursor);
    return cursor + utf16_to_byte_offset(line_view, lsp_char);
}

// --- Severity mapping ----------------------------------------------------
//
// Slang's enum is { Ignored, Note, Warning, Error, Fatal }. The wire
// protocol uses LSP's four levels. `Ignored` is filtered out by the
// caller before we ever get here.
static std::string_view severity_str(slang::DiagnosticSeverity s) {
    using S = slang::DiagnosticSeverity;
    switch (s) {
        case S::Note:    return "information";
        case S::Warning: return "warning";
        case S::Error:   return "error";
        case S::Fatal:   return "error";
        case S::Ignored: return "hint";  // shouldn't reach here
    }
    return "hint";
}

// --- Compilation builder (shared by elaborate + definition) --------------

// One-shot output of `build_compilation`. The `SourceManager` is shared
// (slang's `SyntaxTree::fromBuffer` takes a reference and the AST holds
// pointers into it), so we pin it behind a `shared_ptr`. The compilation
// is unique to this request.
struct BuildCompilationResult {
    std::shared_ptr<slang::SourceManager> sm;
    std::unique_ptr<slang::ast::Compilation> compilation;
    // Path-as-sent → buffer id, so callers can locate a file's buffer
    // by the same path string they put on the wire. Definition lookup
    // needs this to translate `target_path` into a `SourceLocation`.
    std::unordered_map<std::string, slang::BufferID> buffers_by_path;
};

// Parse the request's `files`, `include_dirs`, `defines`, `top` and
// build a slang `Compilation`. The two-pass file seeding (every file
// into `SourceManager`, then `is_compilation_unit: true` files into
// `Compilation`) is the same pattern `elaborate` used pre-refactor; it
// keeps unsaved buffer text reachable by `\`include` while not parsing
// includee files standalone (which produces spurious diagnostics).
static BuildCompilationResult build_compilation(const json& params) {
    using namespace slang;
    using namespace slang::ast;
    using namespace slang::parsing;
    using namespace slang::syntax;

    BuildCompilationResult out;
    out.sm = std::make_shared<SourceManager>();
    out.compilation = std::make_unique<Compilation>();

    PreprocessorOptions pp_opts;
    if (params.contains("defines") && params["defines"].is_array()) {
        for (const auto& d : params["defines"]) {
            std::string entry = d.value("name", std::string{});
            if (entry.empty()) continue;
            if (d.contains("value") && d["value"].is_string()) {
                entry += '=';
                entry += d["value"].get<std::string>();
            }
            pp_opts.predefines.push_back(std::move(entry));
        }
    }
    if (params.contains("include_dirs") && params["include_dirs"].is_array()) {
        for (const auto& dir : params["include_dirs"]) {
            if (dir.is_string()) {
                pp_opts.additionalIncludePaths.emplace_back(dir.get<std::string>());
            }
        }
    }

    Bag options;
    options.set(pp_opts);

    if (params.contains("files") && params["files"].is_array()) {
        struct PendingUnit {
            slang::SourceBuffer buffer;
        };
        std::vector<PendingUnit> units;
        units.reserve(params["files"].size());

        for (const auto& f : params["files"]) {
            auto path = f.value("path", std::string{"<unknown>"});
            auto text = f.value("text", std::string{});
            // `is_compilation_unit` defaults to true — that matches the
            // pre-flag wire format and keeps single-file requests working.
            bool is_cu = f.value("is_compilation_unit", true);

            slang::SourceBuffer buffer;
            try {
                buffer = out.sm->assignText(path, text);
            } catch (const std::exception& e) {
                // Slang refuses duplicate paths. Most often this means
                // the preprocessor already pulled the file in via
                // `\`include` while parsing an earlier unit; the
                // existing buffer is fine, so skip and move on.
                std::cerr << "[mimir-slang-sidecar] skipping duplicate buffer for "
                          << path << ": " << e.what() << '\n';
                continue;
            }

            // Record path → buffer for the definition handler. We index
            // by the exact string the client sent so a F12 request whose
            // `target_path` matches one of the `files[].path` entries
            // resolves cleanly without canonicalisation surprises.
            out.buffers_by_path.emplace(path, buffer.id);

            if (is_cu) {
                units.push_back({buffer});
            }
        }

        for (auto& u : units) {
            auto tree = SyntaxTree::fromBuffer(u.buffer, *out.sm, options);
            out.compilation->addSyntaxTree(tree);
        }
    }

    // Force semantic elaboration so we get diagnostics beyond syntax.
    // This is what closes the gap with tree-sitter — slang now sees
    // through `` `include `` and macro expansion.
    (void)out.compilation->getRoot();

    return out;
}

// Walk the compilation's diagnostics and emit a JSON array in the same
// shape `elaborate`'s response uses. Lifted out of `handle_elaborate` so
// the same function services both the elaborate path and any future
// "definition + diagnostics in one round trip" optimisation.
static json diagnostics_for_compilation(const slang::SourceManager& sm,
                                        slang::ast::Compilation& compilation) {
    using namespace slang;
    using namespace slang::ast;

    DiagnosticEngine engine(sm);

    json diagnostics = json::array();
    for (const auto& diag : compilation.getAllDiagnostics()) {
        auto severity = engine.getSeverity(diag.code, diag.location);
        if (severity == DiagnosticSeverity::Ignored) {
            continue;
        }

        // Default to a zero-width range at the diagnostic's primary
        // location; if slang attached one or more highlight ranges, use
        // the first as our (start, end).
        LspPos start = to_lsp_pos(sm, diag.location);
        LspPos end = start;
        if (!diag.ranges.empty()) {
            const auto& r = diag.ranges.front();
            start = to_lsp_pos(sm, r.start());
            end = to_lsp_pos(sm, r.end());
        }

        // The path we report is the file slang ultimately attributes the
        // diagnostic to (after macro/include unwinding). That matches the
        // path the client originally sent in via `files[].path` for any
        // diagnostic that lives in user source.
        auto orig_loc = sm.getFullyOriginalLoc(diag.location);
        std::string path{sm.getFileName(orig_loc)};

        json d;
        d["path"] = std::move(path);
        d["range"] = {
            {"start", {{"line", start.line}, {"character", start.character}}},
            {"end",   {{"line", end.line},   {"character", end.character}}},
        };
        d["severity"] = severity_str(severity);
        d["code"] = std::string(toString(diag.code));
        d["message"] = engine.formatMessage(diag);
        diagnostics.push_back(std::move(d));
    }
    return diagnostics;
}

// --- elaborate handler ---------------------------------------------------

static json handle_elaborate(const json& params) {
    auto built = build_compilation(params);
    json result;
    result["diagnostics"] = diagnostics_for_compilation(*built.sm, *built.compilation);
    return result;
}

// --- Stage 4: macro `define resolution -----------------------------------
//
// Macro invocations (`MY_MACRO) are preprocessed away before the AST is
// built, so DefinitionFinder never sees them. They survive as
// TriviaKind::Directive trivia (containing a MacroUsageSyntax) on the
// first token of each macro's expansion. We scan those trivia entries
// looking for one whose source range covers the cursor, then look the name
// up in the compilation's DefineDirectiveSyntax list.
//
// This visitor must run BEFORE DefinitionFinder: if the cursor is on a
// macro name, DefinitionFinder returns nothing (or, worse, resolves a
// same-named identifier inside the expansion body).

struct MacroWalker : public slang::syntax::SyntaxVisitor<MacroWalker> {
    const slang::SourceManager* sm;
    const std::string& target_path;
    uint32_t target_offset;
    const slang::syntax::MacroUsageSyntax* found = nullptr;

    MacroWalker(const slang::SourceManager* sm_,
                const std::string& path,
                uint32_t offset)
        : sm(sm_), target_path(path), target_offset(offset) {}

    void visitToken(slang::parsing::Token t) {
        if (found) return;
        for (const auto& tr : t.trivia()) {
            if (tr.kind != slang::parsing::TriviaKind::Directive) continue;
            auto* s = tr.syntax();
            if (!s || s->kind != slang::syntax::SyntaxKind::MacroUsage) continue;
            auto* u = static_cast<const slang::syntax::MacroUsageSyntax*>(s);
            auto r = u->directive.range();
            if (!r.start().valid() || !r.end().valid()) continue;
            auto orig_s = sm->getFullyOriginalLoc(r.start());
            if (sm->getFullPath(orig_s.buffer()).string() != target_path) continue;
            auto orig_e = sm->getFullyOriginalLoc(r.end());
            if (orig_s.offset() <= target_offset && target_offset < orig_e.offset()) {
                found = u;
                return;
            }
        }
    }
};

// Returns the DefineDirectiveSyntax* for the macro whose `reference spans
// cursor, or nullptr. Walks directive trivia across all compiled trees;
// looks up the define name in all trees' getDefinedMacros() lists.
//
// O(tokens + defines) with early exit on first cursor hit. Cheap enough
// for interactive latency — the cursor's compilation unit is typically
// one file.
static const slang::syntax::DefineDirectiveSyntax*
find_macro_at_cursor(
    const slang::ast::Compilation& compilation,
    const slang::SourceManager& sm,
    const std::string& target_path,
    uint32_t target_offset) {

    MacroWalker walker(&sm, target_path, target_offset);
    for (const auto& tree : compilation.getSyntaxTrees()) {
        tree->root().visit(walker);
        if (walker.found) break;
    }
    if (!walker.found) return nullptr;

    // Strip the leading backtick from the directive token's raw text to
    // get the plain name ("MY_MACRO") used as the key in getDefinedMacros().
    std::string_view macro_name = walker.found->directive.rawText();
    if (!macro_name.empty() && macro_name[0] == '`')
        macro_name = macro_name.substr(1);

    // Search all trees for the matching define. Headers pulled in first
    // in the filelist typically hold the define; searching all trees
    // handles the cross-file case transparently.
    for (const auto& tree : compilation.getSyntaxTrees()) {
        for (const auto* def : tree->getDefinedMacros()) {
            if (def && def->name.valueText() == macro_name)
                return def;
        }
    }
    return nullptr;
}

// --- definition handler --------------------------------------------------
//
// AST visitor that finds the `ValueExpressionBase` (i.e. an identifier
// reference whose AST-resolution slang has already done) whose source
// range covers the cursor. We record the deepest such expression — for
// `pkg::sym` and `u_dut.fsm.state`, slang lowers the dotted path to a
// `HierarchicalValueExpression` whose `symbol` is the final declaration,
// which is exactly the F12 target.
//
// What we cover today:
// * variable / parameter / port / class-field references
//   (`NamedValueExpression`)
// * hierarchical paths like `u_dut.fsm.state`
//   (`HierarchicalValueExpression`)
// * `obj.member` access (`MemberAccessExpression`)
// * subroutine calls — `f(x)`, `obj.method()` (`CallExpression`)
// * type references in declarations — cursor on the type token of
//   `my_class c;` resolves to `class my_class` (`ValueSymbol` + its
//   declared type's syntax range)
// * module / interface instantiations — cursor on the type token of
//   `apb_master u_dut(...)` resolves to `module apb_master`
//   (`InstanceSymbol`)
// * base-class references in `extends` clauses — cursor on `uvm_agent`
//   in `class apb_agent extends uvm_agent;` resolves to the base
//   class declaration (`ClassType` + its extendsClause syntax range)
//
// Still deferred (separate slices):
// * macro expansions (`` `MY_MACRO ``) — the slang preprocessor's
//   macro-definition map is exposed but stitching its locations back
//   through `SourceManager::getOriginalLoc()` to a `` `define `` site
//   needs more care than this slice spends.
struct DefinitionFinder : public slang::ast::ASTVisitor<DefinitionFinder,
                                                       /*VisitStatements=*/true,
                                                       /*VisitExpressions=*/true> {
    // SourceManager pointer so `covers_target` can resolve a symbol's
    // buffer back to a filename. Set in `handle_definition`.
    const slang::SourceManager* sm = nullptr;
    // Cursor identity: path + byte-offset. We deliberately *don't* key
    // on `BufferID` — slang regularly ends up with two buffers for the
    // same file (e.g. one we seeded via `assignText` for the open
    // editor buffer, plus a second one the preprocessor loaded from
    // disk while resolving `` `include `` in another compilation
    // unit). The AST attaches to the preprocessor's buffer; the cursor
    // lives in ours; matching by path makes them meet.
    std::string target_path;
    uint32_t target_offset = 0;
    const slang::ast::Symbol* found = nullptr;
    // Track the smallest containing range so an outer
    // `MemberAccessExpression` wrapping a `NamedValueExpression`
    // doesn't shadow the inner one when the cursor is on the inner.
    uint32_t best_width = UINT32_MAX;

    bool covers_target(slang::SourceRange r) const {
        if (!r.start().valid() || !r.end().valid() || sm == nullptr) return false;
        auto orig_start = sm->getFullyOriginalLoc(r.start());
        // `getFullPath` returns the absolute filesystem path slang
        // resolved the buffer to. `getFileName` is "proximised"
        // (relativised against CWD), which collapses `\`include`d
        // files to their bare filename and breaks the comparison
        // against our absolute `target_path`.
        auto full = sm->getFullPath(orig_start.buffer()).string();
        if (full != target_path) return false;
        return orig_start.offset() <= target_offset
            && target_offset < sm->getFullyOriginalLoc(r.end()).offset();
    }

    void record(slang::SourceRange r, const slang::ast::Symbol& sym) {
        auto width = static_cast<uint32_t>(r.end().offset() - r.start().offset());
        if (width <= best_width) {
            best_width = width;
            found = &sym;
        }
    }

    void handle(const slang::ast::NamedValueExpression& e) {
        if (covers_target(e.sourceRange)) record(e.sourceRange, e.symbol);
        visitDefault(e);
    }

    void handle(const slang::ast::HierarchicalValueExpression& e) {
        if (covers_target(e.sourceRange)) record(e.sourceRange, e.symbol);
        visitDefault(e);
    }

    void handle(const slang::ast::MemberAccessExpression& e) {
        // The member-access expression's `sourceRange` covers the whole
        // `obj.member` span. Cursor on the *member* name resolves to
        // the field; cursor on `obj` is caught by the inner
        // NamedValueExpression visit (visitDefault below).
        if (covers_target(e.sourceRange)) record(e.sourceRange, e.member);
        visitDefault(e);
    }

    // `f(x)` / `obj.method()`. For user-defined subroutines we record
    // the resolved `SubroutineSymbol`. System calls (`$display`, ...)
    // have no user declaration to jump to and are skipped.
    void handle(const slang::ast::CallExpression& e) {
        if (!e.isSystemCall() && covers_target(e.sourceRange)) {
            // The variant holds `const SubroutineSymbol*`; `get_if`
            // returns a `const SubroutineSymbol* const*`.
            if (auto* sub_ptr = std::get_if<const slang::ast::SubroutineSymbol*>(&e.subroutine);
                sub_ptr != nullptr && *sub_ptr != nullptr) {
                record(e.sourceRange, **sub_ptr);
            }
        }
        visitDefault(e);
    }

    // `apb_master u_dut(...)`. Each `InstanceSymbol` corresponds to one
    // elaborated instance; an array `m u_arr [3:0] (...)` produces four
    // sibling `InstanceSymbol`s sharing one `HierarchyInstantiationSyntax`
    // parent. The cursor-on-type case fires on whichever sibling we
    // visit first; the smallest-width tie-break keeps the result stable.
    void handle(const slang::ast::InstanceSymbol& s) {
        if (auto* inst_syn = s.getSyntax(); inst_syn != nullptr) {
            auto* parent = inst_syn->parent;
            if (parent != nullptr &&
                parent->kind == slang::syntax::SyntaxKind::HierarchyInstantiation) {
                auto& hi =
                    *static_cast<const slang::syntax::HierarchyInstantiationSyntax*>(parent);
                auto type_range = hi.type.range();
                if (covers_target(type_range)) {
                    record(type_range, s.getDefinition());
                }
            }
        }
        visitDefault(s);
    }

    // Type references in value declarations: `my_t x;`, `my_class c;`,
    // `parameter T p = ...`, ANSI port `input my_t a`. The variable's
    // `DeclaredType` carries the syntax range covering the type token;
    // when the cursor is in that range we resolve to the type symbol
    // (typedef alias, class type, struct type, …). Built-in scalars
    // (`logic`, `bit`) have no `Type::getSyntax()` and produce no
    // location — `symbol_to_definition_location` already filters those.
    //
    // Constrained-auto template so the visitor's static dispatch picks
    // up every concrete `ValueSymbol` subclass (VariableSymbol,
    // ParameterSymbol, NetSymbol, FieldSymbol, FormalArgumentSymbol,
    // PortSymbol, …) without us listing each one. The slang
    // `visitDefault` static-asserts non-final classes, so we must
    // accept the most-derived type, not the base.
    void handle(std::derived_from<slang::ast::ValueSymbol> auto& v) {
        if (auto* type_syn = v.getDeclaredType()->getTypeSyntax(); type_syn != nullptr) {
            auto range = type_syn->sourceRange();
            if (covers_target(range)) {
                record(range, v.getDeclaredType()->getType());
            }
        }
        visitDefault(v);
    }

    // `class apb_agent extends uvm_agent;` — cursor on the base class
    // name in the `extends` clause. Resolves to the base `ClassType`,
    // whose `getSyntax()` is the base class's declaration. Without
    // this the visitor only sees the class's own decl + members and
    // F12 on the base name returns nothing.
    void handle(const slang::ast::ClassType& c) {
        if (auto* syn = c.getSyntax();
            syn != nullptr
            && syn->kind == slang::syntax::SyntaxKind::ClassDeclaration) {
            auto& class_syn =
                *static_cast<const slang::syntax::ClassDeclarationSyntax*>(syn);
            if (class_syn.extendsClause != nullptr) {
                auto range = class_syn.extendsClause->baseName->sourceRange();
                if (covers_target(range)) {
                    if (auto* base = c.getBaseClass(); base != nullptr) {
                        record(range, *base);
                    }
                }
            }
        }
        visitDefault(c);
    }
};

// Convert a found `Symbol`'s declaration site to a JSON
// `DefinitionLocation`. Prefers `getSyntax()->sourceRange()` (the entire
// declaration token, used as the highlight range) and falls back to
// `Symbol::location` as a zero-width point when there's no syntax.
//
// Path resolution is two-tier:
//   1. Reverse-search `buffers_by_path` so files the client sent in
//      `files[]` round-trip their exact path string back. This keeps
//      the editor's URL matching deterministic for the open buffer and
//      any explicit filelist entry.
//   2. Fall back to `sm.getFileName(orig_start)` for buffers slang
//      loaded itself via `` `include `` resolution. UVM-style projects
//      list a single top in the filelist (e.g. `apb.sv`) and pull every
//      class through `` `include `` from the package wrapper, so this
//      fallback is the *common* case, not an edge. The path slang
//      returns is the absolute filesystem path it resolved through the
//      request's `+incdir+` — directly usable by `Url::from_file_path`
//      on the Rust side.
//
// Returns `nullopt` when the symbol's location can't be resolved at
// all — built-in symbols, synthesised library cells, slang-internal
// pseudo-buffers — so the caller turns that into an empty `locations`
// array.
static std::optional<json> symbol_to_definition_location(
    const slang::SourceManager& sm,
    const slang::ast::Symbol& sym,
    const std::unordered_map<std::string, slang::BufferID>& buffers_by_path) {
    using namespace slang;

    SourceLocation loc_start = sym.location;
    SourceLocation loc_end = sym.location;

    if (auto* syn = sym.getSyntax(); syn != nullptr) {
        auto r = syn->sourceRange();
        if (r.start().valid() && r.end().valid()) {
            loc_start = r.start();
            loc_end = r.end();
        }
    }

    if (!loc_start.valid()) {
        return std::nullopt;
    }

    // Resolve the declaration's buffer back to a path the client can
    // navigate to. First try the forward map for an exact-string
    // round-trip; fall back to slang's own filename for `` `include ``'d
    // buffers the client never sent.
    auto orig_start = sm.getFullyOriginalLoc(loc_start);
    auto target_buffer = orig_start.buffer();
    std::string path_out;
    for (const auto& [path, buf_id] : buffers_by_path) {
        if (buf_id == target_buffer) {
            path_out = path;
            break;
        }
    }
    if (path_out.empty()) {
        // `getFullPath` returns the absolute filesystem path slang
        // resolved the buffer to (via `+incdir+` for `` `include ``s).
        // Prefer it over `getFileName`, which proximises to a bare
        // filename and isn't navigable by `Url::from_file_path` on the
        // Rust side.
        path_out = sm.getFullPath(orig_start.buffer()).string();
    }
    if (path_out.empty()) {
        // Buffer has no filename (slang-internal pseudo-buffer) — no
        // file for the editor to open.
        return std::nullopt;
    }

    auto start = to_lsp_pos(sm, loc_start);
    auto end = to_lsp_pos(sm, loc_end);

    json out;
    out["path"] = std::move(path_out);
    out["range"] = {
        {"start", {{"line", start.line}, {"character", start.character}}},
        {"end",   {{"line", end.line},   {"character", end.character}}},
    };
    return out;
}

static json handle_definition(const json& params) {
    auto built = build_compilation(params);
    auto& sm = *built.sm;
    auto& compilation = *built.compilation;

    json result;
    result["locations"] = json::array();

    const auto target_path = params.value("target_path", std::string{});
    auto buf_it = built.buffers_by_path.find(target_path);
    if (buf_it == built.buffers_by_path.end()) {
        // The cursor's file isn't part of this request. Nothing the
        // sidecar can resolve; the server's trust-slang-on-empty rule
        // means the editor sees no result.
        std::cerr << "[mimir-slang-sidecar] definition: target_path not in request: "
                  << target_path << '\n';
        return result;
    }

    json pos = params.value("target_position", json::object());
    uint32_t lsp_line = pos.value("line", 0u);
    uint32_t lsp_char = pos.value("character", 0u);

    auto byte_offset = lsp_position_to_byte_offset(sm, buf_it->second, lsp_line, lsp_char);
    if (!byte_offset) {
        return result;
    }

    // Stage 4: cursor on a macro reference (`MY_MACRO) — check trivia
    // before running the AST visitor. Macro usages are expanded away
    // from the AST; DefinitionFinder would find nothing (or the wrong
    // symbol inside the expansion body).
    auto cursor_offset = static_cast<uint32_t>(*byte_offset);
    if (const auto* macro_def =
            find_macro_at_cursor(compilation, sm, target_path, cursor_offset)) {
        // Report the `define name token's range as the jump target.
        auto name_range = macro_def->name.range();
        auto orig_start = sm.getFullyOriginalLoc(name_range.start());
        std::string def_path;
        for (const auto& [path, buf_id] : built.buffers_by_path) {
            if (buf_id == orig_start.buffer()) {
                def_path = path;
                break;
            }
        }
        if (def_path.empty())
            def_path = sm.getFullPath(orig_start.buffer()).string();
        if (!def_path.empty()) {
            auto start = to_lsp_pos(sm, name_range.start());
            auto end   = to_lsp_pos(sm, name_range.end());
            json loc;
            loc["path"] = std::move(def_path);
            loc["range"] = {
                {"start", {{"line", start.line}, {"character", start.character}}},
                {"end",   {{"line", end.line},   {"character", end.character}}},
            };
            result["locations"].push_back(std::move(loc));
            return result;
        }
    }

    DefinitionFinder finder;
    finder.sm = &sm;
    finder.target_path = target_path;
    finder.target_offset = cursor_offset;
    compilation.getRoot().visit(finder);

    if (finder.found == nullptr) {
        return result;
    }

    if (auto loc = symbol_to_definition_location(sm, *finder.found, built.buffers_by_path)) {
        result["locations"].push_back(std::move(*loc));
    }
    return result;
}

// --- main loop -----------------------------------------------------------

int main() {
    // Don't sync C++ streams with C stdio — measurable speedup on chatty
    // wire traffic. We don't mix `printf` with `std::cout`.
    std::ios::sync_with_stdio(false);
    std::cin.tie(nullptr);

    std::string line;
    while (std::getline(std::cin, line)) {
        json req;
        try {
            req = json::parse(line);
        } catch (const std::exception& e) {
            // A malformed line is logged and skipped — we don't have an
            // `id` to attach a response to, so silence on the wire is
            // correct. The client will time out or get a later response.
            std::cerr << "[mimir-slang-sidecar] parse error: " << e.what() << '\n';
            continue;
        }

        json resp;
        resp["id"] = req.value("id", static_cast<uint64_t>(0));

        const auto method = req.value("method", std::string{});
        try {
            if (method == "elaborate") {
                resp["result"] = handle_elaborate(req.value("params", json::object()));
            } else if (method == "definition") {
                resp["result"] = handle_definition(req.value("params", json::object()));
            } else if (method == "shutdown") {
                // Acknowledge, flush, exit. Keeps the client from seeing
                // a "Closed" before its shutdown response lands.
                resp["result"] = nullptr;
                std::cout << resp.dump() << '\n';
                std::cout.flush();
                return 0;
            } else {
                resp["error"] = {
                    {"code", -32601},
                    {"message", "method not found: " + method},
                };
            }
        } catch (const std::exception& e) {
            // Any unhandled slang exception (bad input, OOM, internal
            // assertion) becomes an error reply rather than a sidecar
            // crash. Keeping the process alive lets the editor recover
            // by sending the next edit.
            resp["error"] = {
                {"code", -1},
                {"message", std::string{"sidecar exception: "} + e.what()},
            };
        }

        std::cout << resp.dump() << '\n';
        std::cout.flush();
    }

    return 0;
}
