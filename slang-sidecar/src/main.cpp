// mimir-slang-sidecar — NDJSON-over-stdio elaborator backed by slang.
//
// Reads one JSON request per line from stdin, calls the matching handler,
// writes one JSON response per line to stdout. All logging goes to stderr
// (stdout is reserved for the protocol — same constraint as an LSP server).
//
// Wire shape mirrors `mimir-slang::protocol`. Methods:
//   * "compile"   — elaborate the supplied files and return the full
//                   MimirAst JSON (schema: mimir-ast/src/types.rs) plus
//                   diagnostics.
//   * "shutdown"  — reply with null result, exit cleanly.
//
// Anything else is replied to with `error.code = -32601` ("method not
// found") and the loop continues so a misbehaving client doesn't take the
// sidecar down.

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <exception>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <iterator>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <unordered_set>
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
#include <slang/ast/symbols/CompilationUnitSymbols.h>
#include <slang/ast/symbols/InstanceSymbols.h>
#include <slang/ast/symbols/ParameterSymbols.h>
#include <slang/ast/symbols/SubroutineSymbols.h>
#include <slang/ast/symbols/PortSymbols.h>
#include <slang/ast/symbols/ValueSymbol.h>
#include <slang/ast/types/Type.h>
#include <slang/diagnostics/DiagnosticEngine.h>
#include <slang/diagnostics/Diagnostics.h>
#include <slang/driver/Driver.h>
#include <slang/numeric/Time.h>
#include <slang/parsing/Preprocessor.h>
#include <slang/syntax/AllSyntax.h>
#include <slang/syntax/SyntaxKind.h>
#include <slang/syntax/SyntaxTree.h>
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

    // The line's start byte offset is the location's own byte offset
    // minus its (1-based) byte column. slang already maintains a
    // line-start table internally, so both `getLineNumber` and
    // `getColumnNumber` are cheap lookups; combining them with
    // `SourceLocation::offset()` gives the line start in O(1).
    //
    // The previous implementation linearly scanned the buffer from
    // byte 0 counting newlines on *every* call — O(line_depth) per
    // call, ~4 calls per ref, which on a large compilation (ibex:
    // ~72k refs, files thousands of lines deep) dominated the whole
    // sidecar compile (~6s of a ~6.7s total). This is the hot path.
    auto buffer_text = sm.getSourceText(orig.buffer());
    size_t prefix_bytes = byte_col_1based - 1;
    size_t loc_offset = orig.offset();
    size_t line_start = (loc_offset >= prefix_bytes) ? loc_offset - prefix_bytes : 0;

    // Clamp against the buffer in case slang's column ran past the
    // stored text (shouldn't happen for valid locs, but a bad column
    // must not index out of bounds).
    if (line_start > buffer_text.size()) {
        return {line_1based - 1, 0};
    }
    if (line_start + prefix_bytes > buffer_text.size()) {
        prefix_bytes = buffer_text.size() - line_start;
    }
    auto utf16 = utf8_prefix_to_utf16_units(buffer_text.substr(line_start, prefix_bytes));

    return {line_1based - 1, utf16};
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

    // ── Step 1: baseline options from `extra_args` (parsed via slang's own
    //   Driver CLI parser so we don't catalog flags ourselves) ─────────────
    PreprocessorOptions pp_opts;
    ast::CompilationOptions comp_opts;
    if (params.contains("extra_args") && params["extra_args"].is_array()
        && !params["extra_args"].empty()) {
        slang::driver::Driver driver;
        driver.addStandardArgs();
        std::vector<std::string> owned{"mimir-slang-sidecar"};
        owned.reserve(owned.size() + params["extra_args"].size());
        for (const auto& a : params["extra_args"]) {
            if (a.is_string()) owned.push_back(a.get<std::string>());
        }
        std::vector<const char*> argv;
        argv.reserve(owned.size());
        for (const auto& s : owned) argv.push_back(s.c_str());
        if (driver.parseCommandLine(static_cast<int>(argv.size()), argv.data())) {
            // Pull the parsed option structs into our locals. We deliberately
            // skip `driver.processOptions()` because it tries to load files
            // through Driver's own SourceLoader — we manage the SourceManager
            // ourselves and only want the parsed flag values.
            Bag driver_bag = driver.createOptionBag();
            if (auto* po = driver_bag.get<PreprocessorOptions>()) pp_opts = *po;
            if (auto* co = driver_bag.get<ast::CompilationOptions>()) comp_opts = *co;
        } else {
            std::cerr << "[mimir-slang-sidecar] extra_args did not parse cleanly; "
                         "applying typed fields without them\n";
        }
    }

    // ── Step 2: typed `defines`/`include_dirs` extend whatever extra_args set ──
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

    // ── Step 3: typed `timescale` overrides any --timescale in extra_args ──
    if (auto ts_str = params.value("timescale", std::string{}); !ts_str.empty()) {
        if (auto parsed = TimeScale::fromString(ts_str)) {
            comp_opts.defaultTimeScale = *parsed;
        } else {
            std::cerr << "[mimir-slang-sidecar] invalid timescale '" << ts_str
                      << "'; ignoring\n";
        }
    }

    // ── Step 4: assemble the Bag and the Compilation ─────────────────────
    Bag options;
    options.set(pp_opts);
    options.set(comp_opts);
    out.compilation = std::make_unique<Compilation>(comp_opts);

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

        // `single_unit: true` parses every CU into one shared SyntaxTree so
        // `` `define`` macros leak across files in the order they were sent —
        // mirrors slang's `--single-unit` CLI flag. Per-file mode (the
        // historical default) is what slang's CLI gives you without the flag.
        bool single_unit = params.value("single_unit", false);
        if (single_unit) {
            std::vector<slang::SourceBuffer> cu_buffers;
            cu_buffers.reserve(units.size());
            for (auto& u : units) cu_buffers.push_back(u.buffer);
            if (!cu_buffers.empty()) {
                auto tree = SyntaxTree::fromBuffers(cu_buffers, *out.sm, options);
                out.compilation->addSyntaxTree(tree);
            }
        } else {
            for (auto& u : units) {
                auto tree = SyntaxTree::fromBuffer(u.buffer, *out.sm, options);
                out.compilation->addSyntaxTree(tree);
            }
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

// --- compile handler (MimirAst export) -----------------------------------
//
// Exports the elaborated symbol table as a MimirAst JSON document matching
// the schema in `crates/mimir-ast/src/types.rs`. Called by the "compile"
// wire method. Produces:
//   { "ast": { "files": [...] }, "diagnostics": [...] }
//
// All positions are UTF-16 code units (same as every other handler).

static json make_mimir_range(const slang::SourceManager& sm, slang::SourceRange r) {
    auto s = to_lsp_pos(sm, r.start());
    auto e = to_lsp_pos(sm, r.end());
    return {{"start", {{"line", s.line}, {"character", s.character}}},
            {"end",   {{"line", e.line}, {"character", e.character}}}};
}

static json make_mimir_point_range(const slang::SourceManager& sm,
                                   slang::SourceLocation loc) {
    if (!loc.valid()) {
        return {{"start", {{"line", 0}, {"character", 0}}},
                {"end",   {{"line", 0}, {"character", 0}}}};
    }
    auto p = to_lsp_pos(sm, loc);
    return {{"start", {{"line", p.line}, {"character", p.character}}},
            {"end",   {{"line", p.line}, {"character", p.character}}}};
}

static std::string loc_to_file_path(const slang::SourceManager& sm,
                                    slang::SourceLocation loc) {
    if (!loc.valid()) return {};
    auto orig = sm.getFullyOriginalLoc(loc);
    if (!orig.valid()) return {};
    return sm.getFullPath(orig.buffer()).string();
}

// Forward declaration — symbol_to_mimir_decl and extract_members are
// mutually recursive for nested types (class-inside-class, etc.).
static json symbol_to_mimir_decl(const slang::SourceManager& sm,
                                  const slang::ast::Symbol& sym,
                                  bool recurse_members);

static json extract_members(const slang::SourceManager& sm,
                             const slang::ast::Scope& scope,
                             bool recurse_members) {
    using SK = slang::ast::SymbolKind;
    // Slang creates both a PortSymbol and an internal Variable/Net for the
    // same port. Collect port names first so the duplicate variable is skipped.
    std::unordered_set<std::string> port_names;
    for (const auto& member : scope.members()) {
        if (member.kind == SK::Port) port_names.insert(std::string(member.name));
    }
    json arr = json::array();
    for (const auto& member : scope.members()) {
        if (member.name.empty()) continue;
        if ((member.kind == SK::Variable || member.kind == SK::Net)
            && port_names.count(std::string(member.name))) continue;
        auto d = symbol_to_mimir_decl(sm, member, recurse_members);
        if (!d.is_null()) arr.push_back(std::move(d));
    }
    return arr;
}

static json symbol_to_mimir_decl(const slang::SourceManager& sm,
                                  const slang::ast::Symbol& sym,
                                  bool recurse_members) {
    using namespace slang::ast;
    if (sym.name.empty()) return nullptr;

    const char* kind_str  = nullptr;
    json members          = json::array();
    json type_str_val     = nullptr;
    json parent_class_val = nullptr;

    switch (sym.kind) {
        case SymbolKind::Package:
            kind_str = "package";
            if (recurse_members)
                members = extract_members(sm, sym.as<PackageSymbol>(), true);
            break;

        case SymbolKind::ClassType: {
            kind_str    = "class";
            auto& cls   = sym.as<ClassType>();
            if (recurse_members) members = extract_members(sm, cls, true);
            if (auto* base = cls.getBaseClass())
                parent_class_val = std::string(base->name);
            break;
        }

        case SymbolKind::Subroutine: {
            auto& sub = sym.as<SubroutineSymbol>();
            kind_str  = (sub.subroutineKind == SubroutineKind::Task) ? "task" : "function";
            // Depth-1 members for subroutines (formal ports only, no bodies).
            members   = extract_members(sm, sub, false);
            if (sub.subroutineKind == SubroutineKind::Function)
                type_str_val = std::string(sub.getReturnType().toString());
            break;
        }

        case SymbolKind::Variable:
        case SymbolKind::Net:
            kind_str     = "variable";
            type_str_val = std::string(sym.as<ValueSymbol>().getType().toString());
            break;

        case SymbolKind::Field:
            kind_str     = "field";
            type_str_val = std::string(sym.as<ValueSymbol>().getType().toString());
            break;

        case SymbolKind::Parameter:
        case SymbolKind::TypeParameter:
            kind_str = "parameter";
            if (sym.kind == SymbolKind::Parameter)
                type_str_val = std::string(sym.as<ParameterSymbol>().getType().toString());
            break;

        case SymbolKind::Port:
            kind_str = "port";
            type_str_val = std::string(sym.as<PortSymbol>().getType().toString());
            break;

        case SymbolKind::EnumValue:
            kind_str = "enumMember";
            break;

        case SymbolKind::TypeAlias:
            kind_str = "typedef";
            break;

        default:
            return nullptr;
    }

    json name_range, full_range;
    if (auto* syn = sym.getSyntax(); syn != nullptr) {
        full_range = make_mimir_range(sm, syn->sourceRange());
        name_range = full_range;
    } else {
        name_range = make_mimir_point_range(sm, sym.location);
        full_range = name_range;
    }

    json decl;
    decl["name"]         = std::string(sym.name);
    decl["kind"]         = kind_str;
    decl["range"]        = std::move(name_range);
    decl["full_range"]   = std::move(full_range);
    decl["type_str"]     = std::move(type_str_val);
    decl["members"]      = std::move(members);
    decl["parent_class"] = std::move(parent_class_val);
    decl["visibility"]   = "public";
    decl["doc"]          = nullptr;
    return decl;
}

// --- reference map -------------------------------------------------------
//
// Walk the elaborated AST and collect resolved (use-site → target-decl)
// links. The Rust side consumes these via `MimirFile::references` to
// answer goto-definition in O(log n) by use-site range, bypassing the
// name-based lookup that can't disambiguate inherited methods named the
// same as the receiver type's own (e.g. UVM `configure`).
//
// Disambiguation rule for overlapping ranges: every emitted use_range
// must be the **name token** of the use site, not a composite
// expression. For `obj.method(args)` we narrow:
//   * `MemberAccessExpression` → the `name` token in the syntax
//   * Free `CallExpression`    → the left expression's identifier
// `NamedValueExpression`'s `sourceRange` already spans just the name, so
// no narrowing is needed there.
//
// For methods accessed via `.`, the SubroutineSymbol is reachable both
// from the `CallExpression.subroutine` and from the inner
// `MemberAccessExpression.member`. The CallExpression handler skips
// these to avoid emitting a duplicate ref.

// Populate `target_type_str`, `target_params`, `target_parent_class` on
// a ref so hover/inlay-hint/signature-help can render entirely from the
// ref — without needing to find the target's `MimirDecl` in `MimirAst`
// (which is impossible for targets in files the client didn't send,
// e.g. inherited methods on UVM base classes). All three fields are
// optional on the wire (skip_serializing_if), so emitting nothing here
// for an unsupported kind degrades cleanly: the consumer falls back to
// its name-based path.
static void enrich_ref_with_target_info(const slang::ast::Symbol& target, json& entry) {
    using namespace slang::ast;
    switch (target.kind) {
        case SymbolKind::Subroutine: {
            auto& sub = target.as<SubroutineSymbol>();
            // Tasks return void → skip the field entirely (decoded as None).
            if (sub.subroutineKind == SubroutineKind::Function) {
                entry["target_type_str"] =
                    std::string(sub.getReturnType().toString());
            }
            json params = json::array();
            for (const auto* arg : sub.getArguments()) {
                if (arg == nullptr) continue;
                json p;
                p["name"] = std::string(arg->name);
                p["type_str"] = std::string(arg->getType().toString());
                params.push_back(std::move(p));
            }
            if (!params.empty()) {
                entry["target_params"] = std::move(params);
            }
            break;
        }
        case SymbolKind::Variable:
        case SymbolKind::Net:
        case SymbolKind::Field:
        case SymbolKind::Port:
            entry["target_type_str"] =
                std::string(target.as<ValueSymbol>().getType().toString());
            break;
        case SymbolKind::Parameter:
            entry["target_type_str"] =
                std::string(target.as<ParameterSymbol>().getType().toString());
            break;
        default:
            // Other kinds (Class, Package, Typedef, …) carry their meaning
            // entirely in `target_kind`; no extra metadata to denormalise.
            break;
    }
    // Parent class is only meaningful for members of a class scope.
    if (const auto* parent_scope = target.getParentScope()) {
        const auto& parent_sym = parent_scope->asSymbol();
        if (parent_sym.kind == SymbolKind::ClassType && !parent_sym.name.empty()) {
            entry["target_parent_class"] = std::string(parent_sym.name);
        }
    }
}

static const char* symbol_kind_to_decl_kind(const slang::ast::Symbol& sym) {
    using SK = slang::ast::SymbolKind;
    switch (sym.kind) {
        case SK::Package:       return "package";
        case SK::ClassType:     return "class";
        case SK::Subroutine: {
            auto& sub = sym.as<slang::ast::SubroutineSymbol>();
            return (sub.subroutineKind == slang::ast::SubroutineKind::Task)
                ? "task" : "function";
        }
        case SK::Variable:
        case SK::Net:           return "variable";
        case SK::Field:         return "field";
        case SK::Parameter:
        case SK::TypeParameter: return "parameter";
        case SK::Port:          return "port";
        case SK::EnumValue:     return "enumMember";
        case SK::TypeAlias:     return "typedef";
        default:                return nullptr;
    }
}

// Pull the right-hand identifier's token range from a member-access
// expression so we don't shadow the inner receiver's `NamedValueExpression`.
static slang::SourceRange narrow_member_access_range(
    const slang::ast::MemberAccessExpression& expr) {
    using namespace slang::syntax;
    if (expr.syntax != nullptr
        && expr.syntax->kind == SyntaxKind::MemberAccessExpression) {
        return expr.syntax->as<MemberAccessExpressionSyntax>().name.range();
    }
    return expr.sourceRange;
}

// For a free call `foo(args)`, narrow the use_range to just the callee's
// name token. For class-method calls via `obj.method(args)` the
// MemberAccessExpression handler already emits the right ref, so this
// helper is only used when no thisClass is present.
static slang::SourceRange narrow_call_range(
    const slang::ast::CallExpression& expr) {
    using namespace slang::syntax;
    if (expr.syntax == nullptr) return expr.sourceRange;
    if (expr.syntax->kind != SyntaxKind::InvocationExpression) {
        return expr.sourceRange;
    }
    const auto* left = expr.syntax->as<InvocationExpressionSyntax>().left.get();
    if (left == nullptr) return expr.sourceRange;
    // The callee is a name: a free `foo`, a dotted `obj.method` (which
    // slang parses as a ScopedName with a `.` separator, *not* a
    // MemberAccessExpression), or a `::`-scoped `pkg::cls::method`. Narrow
    // to the final segment's token so the use_range covers just the method
    // name and never the receiver chain — that keeps it from overlapping
    // the receiver's own ref.
    switch (left->kind) {
        case SyntaxKind::ScopedName:
            return left->as<ScopedNameSyntax>().right->sourceRange();
        case SyntaxKind::MemberAccessExpression:
            return left->as<MemberAccessExpressionSyntax>().name.range();
        default:
            return left->sourceRange();
    }
}

// Maps a slang SourceBuffer to the path the *client* sent for that file
// (the editor's URI), with the same matching rules `handle_compile` used
// to do post-hoc on every collected ref: direct string match against the
// sent set, falling back to `std::filesystem::canonical` against a
// canonical → sent index for symlink / `+incdir+`-resolution mismatch.
//
// Caching by `BufferID` makes the per-ref lookup O(1) after the first
// hit — the visitor sees the same buffer many times. `std::nullopt`
// is cached for buffers that don't map to any sent path so we don't
// retry the canonical resolution on every expression in a UVM header.
class ScopeAttach {
public:
    ScopeAttach(const slang::SourceManager& sm,
                const std::unordered_set<std::string>& sent_paths,
                const std::unordered_map<std::string, std::string>& canonical_to_sent)
        : sm_(sm), sent_paths_(sent_paths), canonical_to_sent_(canonical_to_sent) {}

    // Returns the sent-path string to attach a ref to, or `nullptr` if
    // the buffer's underlying file isn't in any sent path. The returned
    // pointer is stable for the lifetime of this ScopeAttach (string
    // lives in the cache).
    const std::string* lookup(slang::BufferID bid) {
        if (auto it = cache_.find(bid); it != cache_.end()) {
            return it->second.has_value() ? &*it->second : nullptr;
        }
        std::optional<std::string> result;
        const auto& full = sm_.getFullPath(bid);
        if (!full.empty()) {
            std::string slang_path = full.string();
            if (sent_paths_.count(slang_path)) {
                result = std::move(slang_path);
            } else {
                try {
                    auto c = std::filesystem::canonical(slang_path).string();
                    if (auto it2 = canonical_to_sent_.find(c);
                        it2 != canonical_to_sent_.end()) {
                        result = it2->second;
                    }
                } catch (const std::exception&) {
                    // Buffer's path doesn't resolve on disk — out of scope.
                }
            }
        }
        auto [it, _] = cache_.emplace(bid, std::move(result));
        return it->second.has_value() ? &*it->second : nullptr;
    }

private:
    const slang::SourceManager& sm_;
    const std::unordered_set<std::string>& sent_paths_;
    const std::unordered_map<std::string, std::string>& canonical_to_sent_;
    std::unordered_map<slang::BufferID, std::optional<std::string>> cache_;
};

struct RefCollector
    : public slang::ast::ASTVisitor<RefCollector, slang::ast::VisitFlags::AllCanonical> {
    const slang::SourceManager& sm;
    ScopeAttach& scope;
    // sent-path → vector of JSON ref entries. RefCollector writes
    // directly under the editor's URI (resolved via the ScopeAttach
    // cache), so the per-file emit loop is a straight hash lookup —
    // no post-hoc remap needed.
    std::unordered_map<std::string, std::vector<json>>& refs_by_sent;
    // Per-target enrichment memoisation. `enrich_ref_with_target_info`
    // dives into slang's lazy evaluators (`getReturnType`, `getType`,
    // `getArguments`, `getParentScope`) which dominate `record` cost.
    // 72k refs in an ibex compile point at ~hundreds of unique
    // symbols, so caching by raw pointer collapses ~72k slang lookups
    // to ~hundreds.
    //
    // Keyed on raw pointer: `Symbol*` identity is stable inside one
    // `Compilation` and that's the only thing alive while a
    // `RefCollector` exists. The cached JSON holds 0–3 string keys
    // and is copied (cheap) into each emitted entry.
    std::unordered_map<const slang::ast::Symbol*, json> enrich_cache;

    // Instance-body dedup. With `VisitFlags::Canonical` (see the
    // ASTVisitor base above), every instance of a module/interface/
    // program whose body is an exact duplicate of another resolves to
    // the *same* canonical `InstanceBodySymbol`. The visitor still
    // calls `body.visit()` once per instance, so without this set a
    // module instantiated N times would have its body walked N times,
    // producing N identical refs that the post-hoc remap dedup then
    // throws away — pure wasted work.
    //
    // Tracking the canonical body pointers we've already descended
    // into lets us walk each unique body exactly once. This is the
    // record-time equivalent of the refs_remap dedup, but it avoids
    // building the duplicate refs in the first place.
    std::unordered_set<const slang::ast::InstanceBodySymbol*> visited_bodies;

    RefCollector(const slang::SourceManager& sm_, ScopeAttach& scope_,
                 std::unordered_map<std::string, std::vector<json>>& refs_by_sent_)
        : sm(sm_), scope(scope_), refs_by_sent(refs_by_sent_) {}

    // Look up (or compute + cache) the enrichment JSON object for
    // `target`. Returned reference is stable for the rest of this
    // RefCollector's lifetime — callers must copy values out, not
    // mutate.
    const json& cached_enrichment(const slang::ast::Symbol& target) {
        auto [it, inserted] = enrich_cache.try_emplace(&target);
        if (inserted) {
            it->second = json::object();
            enrich_ref_with_target_info(target, it->second);
        }
        return it->second;
    }

    void record(slang::SourceRange use_range, const slang::ast::Symbol* target) {
        if (target == nullptr) return;
        if (target->name.empty()) return;
        if (!use_range.start().valid() || !use_range.end().valid()) return;

        // Macro-aware: the user's editor lives in the original source
        // buffer, so map both endpoints back through any macro expansion.
        auto use_start = sm.getFullyOriginalLoc(use_range.start());
        if (!use_start.valid()) return;

        // Early filter: if this buffer doesn't map to any path the
        // client sent, skip the ref entirely. Drops ~76–100% of work
        // on UVM-style projects where most elaborated expressions live
        // in included headers the editor isn't aware of.
        const std::string* attach = scope.lookup(use_start.buffer());
        if (attach == nullptr) return;

        auto use_end = sm.getFullyOriginalLoc(use_range.end());

        std::string target_path = loc_to_file_path(sm, target->location);
        if (target_path.empty()) return;

        const char* kind = symbol_kind_to_decl_kind(*target);
        if (kind == nullptr) return;

        json target_range_json;
        if (auto* syn = target->getSyntax(); syn != nullptr) {
            target_range_json = make_mimir_range(sm, syn->sourceRange());
        } else {
            target_range_json = make_mimir_point_range(sm, target->location);
        }

        slang::SourceRange orig_use{use_start, use_end};
        json entry;
        entry["use_range"]    = make_mimir_range(sm, orig_use);
        entry["target_path"]  = std::move(target_path);
        entry["target_range"] = std::move(target_range_json);
        entry["target_kind"]  = kind;
        // Merge memoised enrichment (0–3 string fields). The cache
        // saves the slang lazy-eval cost on repeat targets — the same
        // method is referenced thousands of times in a UVM project.
        const json& enrichment = cached_enrichment(*target);
        for (auto it_e = enrichment.begin(); it_e != enrichment.end(); ++it_e) {
            entry[it_e.key()] = it_e.value();
        }
        refs_by_sent[*attach].push_back(std::move(entry));
    }

    void handle(const slang::ast::NamedValueExpression& expr) {
        record(expr.sourceRange, &expr.symbol);
        visitDefault(expr);
    }
    void handle(const slang::ast::HierarchicalValueExpression& expr) {
        record(expr.sourceRange, &expr.symbol);
        visitDefault(expr);
    }
    void handle(const slang::ast::MemberAccessExpression& expr) {
        record(narrow_member_access_range(expr), &expr.member);
        visitDefault(expr);
    }
    void handle(const slang::ast::CallExpression& expr) {
        // System calls (`$display`, `$cast`, …) have no user-source target.
        // Every other call — free `foo(args)` and method `obj.method(args)`
        // alike — gets a ref at its callee/method name token. slang fuses
        // the method name into `subroutine` and never revisits it as a
        // MemberAccessExpression (CallExpression::visitExprs only descends
        // into thisClass + arguments), so this is the *only* place a
        // method-name token gets referenced. `narrow_call_range` returns
        // the `.name` token for a member-access callee and the bare
        // identifier for a free call, so there is no overlap with the
        // MemberAccessExpression handler (which fires only for non-call
        // member access like `obj.field`).
        if (!expr.isSystemCall()) {
            const auto* sub =
                std::get<const slang::ast::SubroutineSymbol*>(expr.subroutine);
            record(narrow_call_range(expr), sub);
        }
        visitDefault(expr);
    }

    // ── Body-level subtree pruning ────────────────────────────────────
    //
    // The per-expression scope check in `record()` already drops refs
    // whose source location is in a buffer the editor didn't send
    // (UVM, vendor IP, third-party packages, etc.). That filter is
    // necessary but late — we still paid the cost of slang's
    // recursion into the entire subtree, plus a `record()` call per
    // expression inside it.
    //
    // The handlers below short-circuit that recursion at the
    // **scope-bearing** symbol level. When the body's declaring
    // location is in an out-of-scope buffer, we return without
    // calling `visitDefault`, so slang never recurses into the body's
    // members. The savings are proportional to how much of the
    // elaborated tree lives outside the editor's filelist — typically
    // most of it in any project that uses UVM or vendor IP.
    //
    // Correctness: refs inside an out-of-scope body all have source
    // locations in that same out-of-scope buffer (by definition of
    // "the body lives there") and would be dropped by the
    // per-expression filter anyway. Macro expansions are unaffected
    // because their *invocation* site is in whatever body invoked the
    // macro — that body's scope is what matters, not the macro
    // definition's. So pruning here is speed-only, not behaviour.
    //
    // Symbols we prune at:
    //   * `InstanceBodySymbol` — the elaborated body of a
    //     module/interface/program instance. The big one for RTL.
    //   * `PackageSymbol`      — a `package … endpackage` scope.
    //     Catches `uvm_pkg` and similar.
    //   * `ClassType`          — a `class … endclass` scope.
    //     Catches top-level classes declared outside packages.
    //
    // Bodies whose `location` is invalid (synthetic / built-in) get
    // visited normally — we'd rather pay traversal cost than risk
    // dropping a real ref.
    bool body_in_scope(const slang::ast::Symbol& body) {
        auto loc = body.location;
        if (!loc.valid()) return true;
        return scope.lookup(loc.buffer()) != nullptr;
    }

    void handle(const slang::ast::InstanceBodySymbol& body) {
        if (!body_in_scope(body)) return;
        // Walk each canonical body once. Repeated instances of the same
        // module resolve here to the same pointer (via the Canonical
        // visit flag); skip the body after the first descent.
        if (!visited_bodies.insert(&body).second) return;
        visitDefault(body);
    }
    void handle(const slang::ast::PackageSymbol& body) {
        if (!body_in_scope(body)) return;
        visitDefault(body);
    }
    void handle(const slang::ast::ClassType& body) {
        if (!body_in_scope(body)) return;
        visitDefault(body);
    }
};

// Returns true unless the user explicitly disabled ref emission via
// `MIMIR_SLANG_EMIT_REFS=0`. Read once per `handle_compile` so a flipped
// env var on a long-lived sidecar takes effect on the next compile.
static bool refs_emission_enabled() {
    const char* env = std::getenv("MIMIR_SLANG_EMIT_REFS");
    if (env == nullptr) return true;
    return std::string_view{env} != "0";
}

// EXPERIMENTAL / UNSAFE: parallelise the RefCollector visit across
// (packages... + root) when `MIMIR_SLANG_PARALLEL=1`.
//
// **Known to crash the sidecar** on UVM-style projects (ibex was the
// reproducer): slang's lazy evaluators (`getReturnType`, `getType`,
// `getArguments`, `getParentScope`) mutate internal Compilation state
// on first call, and concurrent calls race on that mutation. The
// failure mode is "stdout closes before response" — slang aborts
// mid-write.
//
// Kept behind an opt-in env var for future experimentation (e.g. a
// two-phase split where enrichment is pre-warmed sequentially and only
// the JSON build runs in parallel). For now, leave it OFF.
//
// Each worker owns its own `RefCollector`, its own `ScopeAttach`
// cache, and its own `refs_by_sent` map; the only shared reads are
// the `SourceManager` and the const sent_paths / canonical map.
//
// Worker count is `min(hardware_concurrency, unit_count)` so single-
// package / single-instance projects don't oversubscribe.
static bool parallel_enabled() {
    const char* env = std::getenv("MIMIR_SLANG_PARALLEL");
    return env != nullptr && std::string_view{env} != "0";
}

// Decide how many worker threads to use for the parallel visit, given
// `unit_count` independent work items (root + each package).
//
// Returns 0 when parallelism is disabled — the caller takes the
// sequential single-collector path. Returns ≥1 otherwise; never more
// than `unit_count` (extra workers would just spin waiting for work
// that never arrives).
//
// Precedence:
//   * `MIMIR_SLANG_PARALLEL` unset / "0"   → 0 (sequential).
//   * `MIMIR_SLANG_PARALLEL_THREADS=N`     → clamp(N, 1, unit_count).
//   * Otherwise (parallel on, threads unset) → min(hardware_concurrency,
//     unit_count). hardware_concurrency() can return 0 on weird
//     systems; we floor it at 1 so we don't degenerate to "no workers".
//
// The threads cap matters for two scenarios:
//   1. Running under a constrained CI box where you want to leave
//      cores for other jobs.
//   2. Bisecting the crash signature — `THREADS=2` reproduces faster
//      than `THREADS=hwconc` on machines with many cores.
static size_t parallel_worker_count(size_t unit_count) {
    if (!parallel_enabled() || unit_count <= 1) return 0;
    const unsigned hwconc = std::max(1u, std::thread::hardware_concurrency());
    size_t desired = std::min<size_t>(hwconc, unit_count);
    if (const char* env = std::getenv("MIMIR_SLANG_PARALLEL_THREADS")) {
        try {
            int parsed = std::stoi(env);
            if (parsed >= 1) {
                desired = std::min<size_t>(static_cast<size_t>(parsed), unit_count);
            }
        } catch (const std::exception&) {
            // Ignore parse errors — fall back to hwconc default.
        }
    }
    return desired;
}

// Phase timing for `handle_compile`. Off by default; gated by
// `MIMIR_SLANG_TIMING=1`. Emits a single newline-terminated line to
// stderr per compile so it can't fill the pipe buffer in mimir-server
// environments where stderr isn't drained (a single line is tens of
// bytes, well below any plausible pipe capacity).
static bool timing_enabled() {
    const char* env = std::getenv("MIMIR_SLANG_TIMING");
    return env != nullptr && std::string_view{env} != "0";
}

// Captures elapsed milliseconds between successive `lap()` calls.
class Stopwatch {
    using clock = std::chrono::steady_clock;
    clock::time_point start_ = clock::now();
public:
    int64_t lap() {
        auto now = clock::now();
        auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                      now - start_).count();
        start_ = now;
        return ms;
    }
};

// Drop-in scope timer for ad-hoc bottleneck hunts. Logs the scope's
// elapsed milliseconds on destruction when `MIMIR_DEBUG_TIMING=1` is
// set in the sidecar environment; no-op otherwise (one cached bool
// load, no formatting). Pairs with the Rust-side `time_scope!` macro
// in `mimir-core::debug_timer` so both halves of the pipeline share
// one knob.
class ScopedTimer {
    const char* name_;
    std::chrono::steady_clock::time_point start_;

    static bool enabled() {
        static const bool v = []{
            const char* e = std::getenv("MIMIR_DEBUG_TIMING");
            return e != nullptr && std::string_view{e} != "0";
        }();
        return v;
    }

public:
    explicit ScopedTimer(const char* name)
        : name_(name), start_(std::chrono::steady_clock::now()) {}

    ~ScopedTimer() {
        if (!enabled()) return;
        auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                      std::chrono::steady_clock::now() - start_).count();
        std::cerr << "[mimir-slang-sidecar] debug_timer scope=" << name_
                  << " ms=" << ms << "\n";
    }

    ScopedTimer(const ScopedTimer&) = delete;
    ScopedTimer& operator=(const ScopedTimer&) = delete;
};

#define MIMIR_SCOPED_TIMER_CAT_IMPL(a, b) a##b
#define MIMIR_SCOPED_TIMER_CAT(a, b)      MIMIR_SCOPED_TIMER_CAT_IMPL(a, b)
#define MIMIR_SCOPED_TIMER(name) \
    ::ScopedTimer MIMIR_SCOPED_TIMER_CAT(_mimir_scope_timer_, __LINE__){name}

// When `MIMIR_SLANG_DUMP_BUFFERS=1`, dump every buffer slang's
// SourceManager has loaded after compilation finishes — both the buffers
// we registered via `assignText` and any the preprocessor opened on its
// own via `` `include `` resolution. The output is the decisive answer to
// "did slang ever see this file, and under what path?" and isolates
// include-path divergence between editor URIs and slang's `+incdir+`
// resolution.
//
// Destination:
//   * `MIMIR_SLANG_DUMP_FILE=/some/path` → write to that file, truncated
//     each compile so the user sees only the latest run. Use this when
//     the sidecar runs under mimir-server, which pipes stderr but does
//     not drain it (large dumps to stderr can block the sidecar).
//   * Otherwise → write to stderr (fine for standalone probes).
static void maybe_dump_source_manager_buffers(const slang::SourceManager& sm) {
    const char* env = std::getenv("MIMIR_SLANG_DUMP_BUFFERS");
    if (env == nullptr || std::string_view{env} == "0") return;

    auto buffers = sm.getAllBuffers();
    auto write_dump = [&](std::ostream& os) {
        os << "[mimir-slang-sidecar] SourceManager has " << buffers.size()
           << " buffer(s):\n";
        for (auto bid : buffers) {
            const auto& full = sm.getFullPath(bid);
            os << "  " << (full.empty() ? std::string{sm.getRawFileName(bid)}
                                        : full.string())
               << '\n';
        }
    };

    if (const char* path = std::getenv("MIMIR_SLANG_DUMP_FILE");
        path != nullptr && *path != '\0') {
        std::ofstream f(path, std::ios::trunc);
        if (f.is_open()) {
            write_dump(f);
            return;
        }
        std::cerr << "[mimir-slang-sidecar] MIMIR_SLANG_DUMP_FILE='" << path
                  << "' could not be opened for writing; falling back to stderr\n";
    }
    write_dump(std::cerr);
}

static json handle_compile(const json& params) {
    const bool tm = timing_enabled();
    Stopwatch sw;
    int64_t t_build=0, t_diags=0, t_visit=0, t_refs_remap=0,
            t_decls=0, t_emit=0;
    size_t n_refs_raw = 0, n_decls_extracted = 0;

    auto built      = build_compilation(params);
    auto& sm        = *built.sm;
    auto& comp      = *built.compilation;
    if (tm) t_build = sw.lap();

    json all_diags = diagnostics_for_compilation(sm, comp);
    if (tm) t_diags = sw.lap();

    // Group diagnostics by file path for per-file attachment.
    std::unordered_map<std::string, json> diags_by_file;
    for (const auto& d : all_diags) {
        diags_by_file[d.value("path", std::string{})].push_back(d);
    }

    // Build the scope-attach index ONCE, up front, so the ref visitor
    // can filter at record-time instead of building refs we'd throw
    // away post-hoc. `sent_path_set` is the exact-string match;
    // `canonical_to_sent` is the symlink / `+incdir+`-resolved
    // fallback. Both consumed by ScopeAttach below.
    std::unordered_map<std::string, std::string> canonical_to_sent;
    std::unordered_set<std::string> sent_path_set;
    if (params.contains("files") && params["files"].is_array()) {
        for (const auto& f : params["files"]) {
            std::string p = f.value("path", std::string{});
            if (p.empty()) continue;
            sent_path_set.insert(p);
            try {
                auto c = std::filesystem::canonical(p).string();
                canonical_to_sent.emplace(std::move(c), p);
            } catch (const std::exception&) {
                // canonical throws when the path doesn't exist on disk
                // (e.g. for an unsaved in-memory buffer). Skip — the
                // direct-string-match fast path still works for these.
            }
        }
    }
    ScopeAttach scope{sm, sent_path_set, canonical_to_sent};

    // Walk every elaborated expression in the compilation and emit refs
    // **directly under sent-path keys** — the ScopeAttach buffer cache
    // drops refs from out-of-scope buffers (UVM headers, vendor source,
    // anything the editor didn't put in params["files"]) without paying
    // the cost of building their JSON.
    //
    // Two execution modes:
    //   * Sequential (default): single `RefCollector` walks root + each
    //     package in turn.
    //   * Parallel (`MIMIR_SLANG_PARALLEL=1`): each `(package..., root)`
    //     entry is one work unit, distributed across worker threads
    //     with an atomic next-unit counter. Per-worker `ScopeAttach`
    //     and refs map are merged under a mutex at the end. See
    //     `parallel_enabled()` for safety notes.
    std::unordered_map<std::string, std::vector<json>> refs_by_sent;
    if (refs_emission_enabled()) {
        using slang::ast::Symbol;
        std::vector<const Symbol*> units;
        units.reserve(comp.getPackages().size() + 1);
        for (const auto* pkg : comp.getPackages()) {
            if (pkg != nullptr) units.push_back(pkg);
        }
        units.push_back(&comp.getRoot());

        const size_t worker_n = parallel_worker_count(units.size());

        if (worker_n == 0) {
            RefCollector collector{sm, scope, refs_by_sent};
            for (const auto* u : units) u->visit(collector);
        } else {
            std::atomic<size_t> next{0};
            std::mutex          merge_mtx;
            auto worker = [&]() {
                ScopeAttach local_scope{sm, sent_path_set, canonical_to_sent};
                std::unordered_map<std::string, std::vector<json>> local_refs;
                RefCollector local_collector{sm, local_scope, local_refs};
                while (true) {
                    size_t i = next.fetch_add(1, std::memory_order_relaxed);
                    if (i >= units.size()) break;
                    units[i]->visit(local_collector);
                }
                std::lock_guard<std::mutex> lk(merge_mtx);
                for (auto& [k, v] : local_refs) {
                    auto& dst = refs_by_sent[k];
                    dst.reserve(dst.size() + v.size());
                    dst.insert(dst.end(),
                               std::make_move_iterator(v.begin()),
                               std::make_move_iterator(v.end()));
                }
            };
            std::vector<std::thread> workers;
            workers.reserve(worker_n);
            for (size_t i = 0; i < worker_n; ++i) workers.emplace_back(worker);
            for (auto& t : workers) t.join();
        }
    }
    if (tm) {
        for (auto& [_, v] : refs_by_sent) n_refs_raw += v.size();
        t_visit = sw.lap();
    }

    // Sort each sent-file's refs by use_range.start so the Rust side can
    // binary-search at the cursor, then dedupe adjacent identical entries.
    //
    // Dedup is load-bearing for method calls: slang represents
    // `obj.method(args)` such that *both* the MemberAccessExpression
    // handler (visiting the callee) and the CallExpression handler
    // (covering the method name in InvocationExpression.left) fire on the
    // same use, with `narrow_member_access_range` and `narrow_call_range`
    // returning the same `.method` token range and the same resolved
    // SubroutineSymbol. Without dedup every method-call ref is emitted
    // twice, bloating the wire payload (a UVM project has tens of
    // thousands of method calls) and forcing the Rust binary search to
    // discard duplicates at lookup time.
    for (auto& [_, vec] : refs_by_sent) {
        std::sort(vec.begin(), vec.end(), [](const json& a, const json& b) {
            const auto& as = a["use_range"]["start"];
            const auto& bs = b["use_range"]["start"];
            auto al = as["line"].get<uint32_t>();
            auto bl = bs["line"].get<uint32_t>();
            if (al != bl) return al < bl;
            return as["character"].get<uint32_t>() < bs["character"].get<uint32_t>();
        });
        vec.erase(std::unique(vec.begin(), vec.end()), vec.end());
    }
    if (tm) t_refs_remap = sw.lap();

    // ── Single-pass top-level decl extraction ─────────────────────────────
    //
    // Build a `slang_path → top-level decls` index once. The per-file emit
    // loop below then does an O(1) hash lookup instead of re-walking
    // `compilation.getPackages()` and `compilation.getRoot().members()` per
    // sent file. On a project with N sent files and P+I top-level symbols
    // the old shape was O(N × (P + I)); the new shape is O(P + I + N).
    //
    // After collection, remap onto sent paths via the same
    // `canonical_to_sent` index the refs path uses, so symlinked or
    // `+incdir+ -resolved` paths land in the editor-facing bucket.
    std::unordered_map<std::string, std::vector<json>> decls_by_slang_path;
    {
        for (const auto* pkg : comp.getPackages()) {
            if (pkg == nullptr || pkg->name.empty()) continue;
            std::string fp = loc_to_file_path(sm, pkg->location);
            if (fp.empty()) continue;
            auto d = symbol_to_mimir_decl(sm, *pkg, /*recurse=*/true);
            if (!d.is_null()) decls_by_slang_path[fp].push_back(std::move(d));
        }

        // Global dedup across all top-level instances (multiple
        // instantiations of the same module share a definition).
        std::unordered_set<std::string> seen_inst_keys;
        for (const auto& member : comp.getRoot().members()) {
            if (member.kind != slang::ast::SymbolKind::Instance) continue;
            auto& inst = member.as<slang::ast::InstanceSymbol>();
            auto& def  = inst.getDefinition();
            if (def.name.empty()) continue;
            std::string fp = loc_to_file_path(sm, def.location);
            if (fp.empty()) continue;

            auto p = to_lsp_pos(sm, def.location);
            std::string key = fp + ":" + std::to_string(p.line)
                                 + ":" + std::to_string(p.character);
            if (!seen_inst_keys.insert(std::move(key)).second) continue;

            const char* kind_str = "module";
            if (auto* syn = def.getSyntax(); syn != nullptr) {
                using SK = slang::syntax::SyntaxKind;
                if (syn->kind == SK::InterfaceDeclaration)      kind_str = "interface";
                else if (syn->kind == SK::ProgramDeclaration)   kind_str = "program";
            }
            json members = extract_members(sm, inst.body, /*recurse=*/true);

            json name_range, full_range;
            if (auto* syn = def.getSyntax(); syn != nullptr) {
                full_range = make_mimir_range(sm, syn->sourceRange());
                name_range = full_range;
            } else {
                name_range = make_mimir_point_range(sm, def.location);
                full_range = name_range;
            }

            json decl;
            decl["name"]         = std::string(def.name);
            decl["kind"]         = kind_str;
            decl["range"]        = std::move(name_range);
            decl["full_range"]   = std::move(full_range);
            decl["type_str"]     = nullptr;
            decl["members"]      = std::move(members);
            decl["parent_class"] = nullptr;
            decl["visibility"]   = "public";
            decl["doc"]          = nullptr;
            decls_by_slang_path[fp].push_back(std::move(decl));
        }
    }

    std::unordered_map<std::string, std::vector<json>> decls_by_sent;
    for (auto& [slang_path, decls] : decls_by_slang_path) {
        if (decls.empty()) continue;
        std::string attach_to;
        if (sent_path_set.count(slang_path)) {
            attach_to = slang_path;
        } else {
            try {
                auto c = std::filesystem::canonical(slang_path).string();
                if (auto it = canonical_to_sent.find(c);
                    it != canonical_to_sent.end()) {
                    attach_to = it->second;
                }
            } catch (const std::exception&) {
                // Slang path doesn't exist on disk; drop (rare).
            }
        }
        if (!attach_to.empty()) {
            auto& dst = decls_by_sent[attach_to];
            dst.insert(dst.end(),
                       std::make_move_iterator(decls.begin()),
                       std::make_move_iterator(decls.end()));
        }
    }
    if (tm) {
        for (auto& [_, v] : decls_by_sent) n_decls_extracted += v.size();
        t_decls = sw.lap();
    }

    json files = json::array();
    if (params.contains("files") && params["files"].is_array()) {
        for (const auto& f : params["files"]) {
            std::string path = f.value("path", std::string{});
            json file_json;
            file_json["uri"] = path;

            // Per-file diagnostics — strip the "path" key since it's already
            // present in file_json["uri"].
            json file_diags = json::array();
            if (auto it = diags_by_file.find(path); it != diags_by_file.end()) {
                for (auto d : it->second) {
                    d.erase("path");
                    file_diags.push_back(std::move(d));
                }
            }
            file_json["diagnostics"] = std::move(file_diags);

            // Top-scope decls: O(1) lookup into the pre-built index
            // (replaces the per-file traversal of getRoot()/getPackages()).
            json scope;
            scope["range"]             = {{"start", {{"line", 0}, {"character", 0}}},
                                          {"end",   {{"line", 999999}, {"character", 0}}}};
            if (auto it = decls_by_sent.find(path); it != decls_by_sent.end()) {
                json arr = json::array();
                for (auto& d : it->second) arr.push_back(std::move(d));
                scope["declarations"] = std::move(arr);
            } else {
                scope["declarations"] = json::array();
            }
            scope["children"]          = json::array();
            scope["imported_packages"] = json::array();
            file_json["top_scope"]     = std::move(scope);

            // Attach refs that were remapped to this sent path (covers
            // both direct slang_path == sent_path hits and canonical
            // equivalents like symlinks).
            json refs_json = json::array();
            if (auto it = refs_by_sent.find(path); it != refs_by_sent.end()) {
                for (auto& r : it->second) refs_json.push_back(std::move(r));
            }
            file_json["references"] = std::move(refs_json);

            files.push_back(std::move(file_json));
        }
    }

    json ast;
    ast["files"] = std::move(files);

    maybe_dump_source_manager_buffers(sm);

    json result;
    result["ast"]         = std::move(ast);
    result["diagnostics"] = std::move(all_diags);

    if (tm) {
        t_emit = sw.lap();
        int64_t total = t_build + t_diags + t_visit + t_refs_remap
                      + t_decls + t_emit;
        std::cerr << "[mimir-slang-sidecar] timing"
                  << " build="       << t_build       << "ms"
                  << " diags="       << t_diags       << "ms"
                  << " visit="       << t_visit       << "ms"
                  << " refs_raw="    << n_refs_raw
                  << " refs_remap="  << t_refs_remap  << "ms"
                  << " decls="       << t_decls       << "ms"
                  << " decls_n="     << n_decls_extracted
                  << " emit="        << t_emit        << "ms"
                  << " total="       << total         << "ms"
                  << " (json_dump not included; measured in main)\n";
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
            if (method == "compile") {
                resp["result"] = handle_compile(req.value("params", json::object()));
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

        // For `compile` responses, time the JSON->string serialisation
        // separately so the user can compare against the in-handle_compile
        // breakdown. nlohmann_json::dump is the suspect when a payload
        // serialises slowly (allocation-heavy on deep AST trees).
        const bool tm_dump = timing_enabled() && method == "compile";
        Stopwatch sw_dump;
        std::string dumped = resp.dump();
        if (tm_dump) {
            int64_t ms = sw_dump.lap();
            std::cerr << "[mimir-slang-sidecar] timing json_dump=" << ms
                      << "ms bytes=" << dumped.size() << "\n";
        }
        std::cout << dumped << '\n';
        std::cout.flush();
    }

    return 0;
}
