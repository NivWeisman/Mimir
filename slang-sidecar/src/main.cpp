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

#include <cstdint>
#include <exception>
#include <iostream>
#include <memory>
#include <string>
#include <string_view>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

#include <nlohmann/json.hpp>

#include <slang/ast/Compilation.h>
#include <slang/ast/Symbol.h>
#include <slang/ast/symbols/ClassSymbols.h>
#include <slang/ast/symbols/InstanceSymbols.h>
#include <slang/ast/symbols/ParameterSymbols.h>
#include <slang/ast/symbols/SubroutineSymbols.h>
#include <slang/ast/symbols/PortSymbols.h>
#include <slang/ast/symbols/ValueSymbol.h>
#include <slang/ast/types/Type.h>
#include <slang/diagnostics/DiagnosticEngine.h>
#include <slang/diagnostics/Diagnostics.h>
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

// Build the MimirScope for one file by collecting top-level definitions
// (packages + module/interface/program definitions) whose declaration site
// lives in `file_path`.
static json build_file_top_scope(
    const slang::SourceManager& sm,
    slang::ast::Compilation& compilation,
    const std::string& file_path) {

    using namespace slang::ast;

    json decls = json::array();

    // Packages declared in this file.
    for (const auto* pkg : compilation.getPackages()) {
        if (!pkg || pkg->name.empty()) continue;
        if (loc_to_file_path(sm, pkg->location) != file_path) continue;
        auto d = symbol_to_mimir_decl(sm, *pkg, /*recurse=*/true);
        if (!d.is_null()) decls.push_back(std::move(d));
    }

    // Top-level module/interface/program definitions. The root holds one
    // InstanceSymbol per top-level instantiation; we deduplicate via a
    // position key so that a module instantiated several times appears only
    // once.
    std::unordered_set<std::string> seen_defs;
    for (const auto& member : compilation.getRoot().members()) {
        if (member.kind != SymbolKind::Instance) continue;
        auto& inst = member.as<InstanceSymbol>();
        auto& def  = inst.getDefinition();
        if (def.name.empty()) continue;
        if (loc_to_file_path(sm, def.location) != file_path) continue;

        // Dedup key: line:character of the definition name token.
        auto p = to_lsp_pos(sm, def.location);
        std::string key = std::to_string(p.line) + ":" + std::to_string(p.character);
        if (!seen_defs.insert(key).second) continue;

        // Determine the module kind from the syntax node kind.
        const char* kind_str = "module";
        if (auto* syn = def.getSyntax(); syn != nullptr) {
            using SK = slang::syntax::SyntaxKind;
            if (syn->kind == SK::InterfaceDeclaration) kind_str = "interface";
            else if (syn->kind == SK::ProgramDeclaration) kind_str = "program";
        }

        // Body members: ports, local vars, functions, nested classes.
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
        decls.push_back(std::move(decl));
    }

    json scope;
    scope["range"]             = {{"start", {{"line", 0}, {"character", 0}}},
                                  {"end",   {{"line", 999999}, {"character", 0}}}};
    scope["declarations"]      = std::move(decls);
    scope["children"]          = json::array();
    scope["imported_packages"] = json::array();
    return scope;
}

static json handle_compile(const json& params) {
    auto built      = build_compilation(params);
    auto& sm        = *built.sm;
    auto& comp      = *built.compilation;

    json all_diags = diagnostics_for_compilation(sm, comp);

    // Group diagnostics by file path for per-file attachment.
    std::unordered_map<std::string, json> diags_by_file;
    for (const auto& d : all_diags) {
        diags_by_file[d.value("path", std::string{})].push_back(d);
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
            file_json["top_scope"]   = build_file_top_scope(sm, comp, path);
            files.push_back(std::move(file_json));
        }
    }

    json ast;
    ast["files"] = std::move(files);

    json result;
    result["ast"]         = std::move(ast);
    result["diagnostics"] = std::move(all_diags);
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

        std::cout << resp.dump() << '\n';
        std::cout.flush();
    }

    return 0;
}
