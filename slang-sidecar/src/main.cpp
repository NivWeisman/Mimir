// mimir-slang-sidecar — NDJSON-over-stdio elaborator backed by slang.
//
// Reads one JSON request per line from stdin, calls the matching handler,
// writes one JSON response per line to stdout. All logging goes to stderr
// (stdout is reserved for the protocol — same constraint as an LSP server).
//
// Wire shape mirrors `mimir-slang::protocol`. Methods today:
//   * "elaborate" — preprocess + parse + elaborate the supplied files,
//                   return diagnostics in LSP coordinates.
//   * "shutdown"  — reply with null result, exit cleanly.
//
// Anything else is replied to with `error.code = -32601` ("method not
// found") and the loop continues so a misbehaving client doesn't take the
// sidecar down.
//
// The handler is intentionally a free function (not a class) — there's no
// shared state between requests today. When that changes (e.g. caching a
// `Compilation` across edits in Stage 3) we'll fold it into a Server class.

#include <cstdint>
#include <exception>
#include <iostream>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <nlohmann/json.hpp>

#include <slang/ast/Compilation.h>
#include <slang/diagnostics/DiagnosticEngine.h>
#include <slang/diagnostics/Diagnostics.h>
#include <slang/parsing/Preprocessor.h>
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

// --- elaborate handler ---------------------------------------------------

static json handle_elaborate(const json& params) {
    using namespace slang;
    using namespace slang::ast;
    using namespace slang::parsing;
    using namespace slang::syntax;

    auto sm = std::make_shared<SourceManager>();

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

    Compilation compilation;
    if (params.contains("files") && params["files"].is_array()) {
        // Two passes. The first seeds every supplied buffer into the
        // SourceManager so the preprocessor resolves `\`include` against
        // the user's in-memory text (with unsaved edits) instead of
        // going to disk. The second wraps the *compilation-unit* files
        // in their own SyntaxTree — files marked `is_compilation_unit:
        // false` ride along only as include sources, because parsing
        // them standalone (e.g. an UVM agent file meant to live inside
        // `package … endpackage`) produces spurious diagnostics.
        struct PendingUnit {
            std::string path;
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
                buffer = sm->assignText(path, text);
            } catch (const std::exception& e) {
                // Slang refuses duplicate paths. Most often this means
                // the preprocessor already pulled the file in via
                // `\`include` while parsing an earlier unit; the
                // existing buffer is fine, so skip and move on.
                std::cerr << "[mimir-slang-sidecar] skipping duplicate buffer for "
                          << path << ": " << e.what() << '\n';
                continue;
            }

            if (is_cu) {
                units.push_back({std::move(path), buffer});
            }
        }

        for (auto& u : units) {
            auto tree = SyntaxTree::fromBuffer(u.buffer, *sm, options);
            compilation.addSyntaxTree(tree);
        }
    }

    // Force semantic elaboration so we get diagnostics beyond syntax.
    // This is what closes the gap with tree-sitter — slang now sees
    // through `` `include `` and macro expansion.
    (void)compilation.getRoot();

    DiagnosticEngine engine(*sm);

    json diagnostics = json::array();
    for (const auto& diag : compilation.getAllDiagnostics()) {
        auto severity = engine.getSeverity(diag.code, diag.location);
        if (severity == DiagnosticSeverity::Ignored) {
            continue;
        }

        // Default to a zero-width range at the diagnostic's primary
        // location; if slang attached one or more highlight ranges, use
        // the first as our (start, end).
        LspPos start = to_lsp_pos(*sm, diag.location);
        LspPos end = start;
        if (!diag.ranges.empty()) {
            const auto& r = diag.ranges.front();
            start = to_lsp_pos(*sm, r.start());
            end = to_lsp_pos(*sm, r.end());
        }

        // The path we report is the file slang ultimately attributes the
        // diagnostic to (after macro/include unwinding). That matches the
        // path the client originally sent in via `files[].path` for any
        // diagnostic that lives in user source.
        auto orig_loc = sm->getFullyOriginalLoc(diag.location);
        std::string path{sm->getFileName(orig_loc)};

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

    json result;
    result["diagnostics"] = std::move(diagnostics);
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
