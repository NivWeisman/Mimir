//! Hover content builders: declaration lines, signatures, macro bodies,
//! keyword docs, and the macro-expansion footer.
//!
//! Pure functions over symbols, ropes, and the open-document store —
//! the hover handler in [`crate::backend`] resolves *what* to show and
//! delegates the markdown assembly here.

use mimir_core::Position as MPosition;
use mimir_syntax::{Symbol, SymbolKind as MSymbolKind, SyntaxTree};
use ropey::Rope;
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Url};

use crate::backend::DocumentState;

/// Build a `Hover` from a resolved [`Symbol`] by:
///
/// 1. Synthesizing a typed signature for callables (function/task/
///    method/macro) via [`mimir_syntax::signature::signature_for`].
/// 2. For macros, additionally appending the full `define` body
///    captured from the source between `full_range.start` and
///    `full_range.end`.
/// 3. Falling back to the raw declaration line for non-callables
///    (classes, modules, variables, typedefs, parameters, …).
///
/// The line is read from the open-doc store first (the editor's
/// authoritative view of unsaved content), then from disk — mirrors
/// `completionItem/resolve`'s pattern. Returns `None` only when the
/// declaration line genuinely can't be found anywhere.
pub(crate) fn hover_for_symbol(
    sym: &Symbol,
    sym_url: &Url,
    docs: &std::collections::HashMap<Url, DocumentState>,
) -> Option<Hover> {
    let rope_from_doc: Option<Rope> = docs
        .get(sym_url)
        .map(|s| s.document.rope().clone());

    // 1. Callable signatures (function/task/method/macro).
    if let Some(sig) = mimir_syntax::signature::signature_for(sym) {
        if sym.kind == MSymbolKind::Macro {
            // For macros: signature + body.
            let body = read_macro_body(sym, sym_url, rope_from_doc.as_ref());
            let value = match body {
                Some(b) if !b.trim().is_empty() => {
                    format!("```systemverilog\n{}\n{}\n```", sig.label, b)
                }
                _ => format!("```systemverilog\n{}\n```", sig.label),
            };
            return Some(hover_from_markdown(value));
        }
        return Some(hover_from_markdown(
            mimir_syntax::hover_format::format_sv_signature(&sig.label),
        ));
    }

    // 2. Multi-line typedefs (`typedef enum {...} state_e;`): the *name*
    //    token sits on the closing line, so a single-line read would show
    //    a broken `"} state_e;"` snippet. Show the whole declaration block
    //    from `full_range` instead (elided when very long).
    if sym.kind == MSymbolKind::Typedef
        && sym.full_range.end.line > sym.full_range.start.line
    {
        if let Some(block) = read_range_text(sym.full_range, sym_url, rope_from_doc.as_ref()) {
            let block = elide_after_lines(block.trim_end(), 30);
            return Some(hover_from_markdown(format!(
                "```systemverilog\n{block}\n```"
            )));
        }
    }

    // 3. Non-callables: the declaration line.
    let line_no = sym.name_range.start.line;
    let line = rope_from_doc
        .as_ref()
        .and_then(|r| read_line_trimmed(r, line_no))
        .or_else(|| {
            sym_url
                .to_file_path()
                .ok()
                .and_then(|p| std::fs::read_to_string(&p).ok())
                .and_then(|t| read_line_trimmed(&Rope::from_str(&t), line_no))
        })?;

    // 3a. For single-line typedefs, append the expanded base type after the
    //     declaration (multi-line ones already show their full body above).
    if sym.kind == MSymbolKind::Typedef {
        if let Some(base) = typedef_base_from_line(&line, &sym.name) {
            let md = format!(
                "```systemverilog\n{}\n```\n\n**Expands to:** `{}`",
                line, base
            );
            return Some(hover_from_markdown(md));
        }
    }

    Some(hover_markdown(&line))
}

/// Read the source slice covering `range` from the open-doc rope first,
/// then from disk. The raw text, untrimmed — callers decide presentation.
fn read_range_text(
    range: mimir_core::Range,
    sym_url: &Url,
    doc_rope: Option<&Rope>,
) -> Option<String> {
    let slice_from_rope = |rope: &Rope| -> Option<String> {
        let start = range.start.to_byte_offset(rope).ok()?;
        let end = range.end.to_byte_offset(rope).ok()?;
        if end <= start || end > rope.len_bytes() {
            return None;
        }
        Some(rope.byte_slice(start..end).to_string())
    };

    doc_rope.and_then(slice_from_rope).or_else(|| {
        let path = sym_url.to_file_path().ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let rope = Rope::from_str(&text);
        slice_from_rope(&rope)
    })
}

/// Keep the first `max` lines of `text`, replacing the rest with an
/// elision comment. Hover popups for hundred-member enums stay readable.
fn elide_after_lines(text: &str, max: usize) -> String {
    let total = text.lines().count();
    if total <= max {
        return text.to_string();
    }
    let kept: Vec<&str> = text.lines().take(max).collect();
    format!("{}\n// … ({} more lines)", kept.join("\n"), total - max)
}

/// Extract the base type from a typedef declaration line.
///
/// Given `"typedef logic [31:0] addr_t;"` and alias `"addr_t"`, returns
/// `Some("logic [31:0]")`. Returns `None` for forward declarations
/// (`typedef class Foo;`) or malformed input.
pub(crate) fn typedef_base_from_line(line: &str, alias: &str) -> Option<String> {
    // Strip leading whitespace and "typedef" keyword.
    let after = line.trim().strip_prefix("typedef")?.trim_start();
    // Find the alias name from the right so struct/enum field names don't confuse us.
    let alias_pos = after.rfind(alias)?;
    let base = after[..alias_pos].trim_end().trim_end_matches(';').trim();
    // Reject forward declarations: base would be "class" or empty.
    if base.is_empty() || base == "class" {
        return None;
    }
    Some(base.to_string())
}

/// Read the source slice covering `sym.full_range` from the open-doc
/// rope first, then from disk. Returns the trimmed body.
///
/// Used by hover on a macro reference to show the full `\`define`
/// expansion, including multi-line `\\`-continued bodies. Returns
/// `None` if neither source is readable; the caller drops to showing
/// just the signature in that case.
pub(crate) fn read_macro_body(sym: &Symbol, sym_url: &Url, doc_rope: Option<&Rope>) -> Option<String> {
    let raw = read_range_text(sym.full_range, sym_url, doc_rope)?;

    // Strip the leading `\`define MACRO_NAME(...)`-or-`\`define MACRO_NAME`
    // header. Everything after the first `)` (for parametrised macros) or
    // after the macro name (for bare ones) up to the end-of-define is the
    // body. We keep this conservative: skip the first source line up to
    // and including the closing paren of the params; if there's no `(`
    // skip past the name.
    let after_name = raw.find(&sym.name).map(|i| i + sym.name.len()).unwrap_or(0);
    let after_params = if let Some(rest) = raw.get(after_name..) {
        if rest.trim_start().starts_with('(') {
            // Skip to the matching `)`.
            rest.find(')')
                .map(|idx| after_name + idx + 1)
                .unwrap_or(after_name)
        } else {
            after_name
        }
    } else {
        after_name
    };

    let body = raw
        .get(after_params..)
        .unwrap_or("")
        .trim_matches(|c: char| c == ' ' || c == '\t' || c == '\\' || c == '\r' || c == '\n');
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
}

/// Wrap a single line as a SystemVerilog markdown fenced block — the
/// same format `completionItem/resolve` uses, so hover and resolve
/// docstrings look identical to the user.
pub(crate) fn hover_markdown(line: &str) -> Hover {
    hover_from_markdown(format!("```systemverilog\n{line}\n```"))
}

/// Final hover fallback: if the cursor sits on a reserved keyword or
/// `$system_task` for which the curated table in
/// [`mimir_syntax::keywords::doc_for`] has a description, build a
/// markdown popup. Returns `None` for unknown words, whitespace, or
/// punctuation — the caller treats that as "no hover".
///
/// Hover for IEEE 1800-2017 built-in methods (`push_back`, `rand_mode`,
/// `len`, `toupper`, `exists`, …).
///
/// Runs after `hover_via_tree_sitter` returns `None` so any user-defined
/// method with the same name shadows the built-in entry. The fallback chain:
///
/// * `this` / `super` receiver → universal methods only.
/// * `obj.method` → type-aware lookup for the receiver's declared type
///   (accurate for `string`), then universal table (accurate for
///   `rand_mode` / `constraint_mode` on any class).  When the type cannot
///   be resolved, falls to name-only.
/// * No receiver → name-only scan across all tables (hover is UX, not
///   correctness — better to show something than nothing).
pub(crate) fn builtin_method_hover_at(tree: &SyntaxTree, rope: &Rope, target: MPosition) -> Option<Hover> {
    use mimir_syntax::symbols::{
        find_variable_type_info_at, hover_receiver_at, identifier_at, normalize_type_name,
        HoverReceiver,
    };

    let name = identifier_at(tree, rope, target)?;
    let receiver = hover_receiver_at(tree, rope, target);

    let m: &mimir_syntax::builtin_methods::BuiltinMethod = match &receiver {
        Some(HoverReceiver::This) | Some(HoverReceiver::Super) => {
            mimir_syntax::builtin_methods::find_universal(name)?
        }
        Some(HoverReceiver::Object(recv)) => {
            let type_info = find_variable_type_info_at(tree, rope, target, recv);
            let cls = type_info.as_ref().and_then(|t| normalize_type_name(&t.base));
            if let Some(cls) = cls {
                // Try type-specific then universal (class receiver).
                mimir_syntax::builtin_methods::find_method(&cls, name)
                    .or_else(|| mimir_syntax::builtin_methods::find_universal(name))
                    .or_else(|| {
                        // Class lookup missed — fall back to dimension-suffix
                        // table (e.g. `int q[$]` → QUEUE_METHODS).
                        type_info
                            .as_ref()
                            .and_then(|t| t.suffix.as_deref())
                            .and_then(|sfx| {
                                mimir_syntax::builtin_methods::methods_for_suffix(sfx)
                                    .iter()
                                    .find(|m| m.name == name)
                            })
                    })?
            } else if let Some(sfx) = type_info.as_ref().and_then(|t| t.suffix.as_deref()) {
                // No class name at all (e.g. bare `int q[$]`) — go straight
                // to the dimension-suffix table.
                mimir_syntax::builtin_methods::methods_for_suffix(sfx)
                    .iter()
                    .find(|m| m.name == name)?
            } else {
                mimir_syntax::builtin_methods::find_method_by_name(name)?
            }
        }
        None => mimir_syntax::builtin_methods::find_method_by_name(name)?,
    };

    Some(hover_from_markdown(format!(
        "{}\n\n{}",
        mimir_syntax::hover_format::format_sv_signature(m.signature),
        m.doc
    )))
}

/// The popup format mirrors [`hover_for_symbol`] so keyword help looks
/// the same as symbol help: the word itself in a `systemverilog`
/// fenced block, then the one-line description as a separate markdown
/// paragraph below.
pub(crate) fn keyword_hover_at(tree: &SyntaxTree, rope: &Rope, target: MPosition) -> Option<Hover> {
    let word = mimir_syntax::symbols::word_at(tree, rope, target)?;
    let doc = mimir_syntax::keywords::doc_for(word)?;
    Some(hover_from_markdown(format!(
        "```systemverilog\n{word}\n```\n\n{doc}"
    )))
}

/// Build a `Hover` from an already-formatted markdown blob. Always
/// emits `MarkupKind::Markdown`; LSP clients that prefer plain text
/// degrade gracefully on their end.
pub(crate) fn hover_from_markdown(markdown: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    }
}

// --------------------------------------------------------------------------
// Macro-expansion: custom-request response + hover-footer helpers
// --------------------------------------------------------------------------

/// Response payload for the custom `mimir/expandMacro` request. Serialised
/// camelCase so the VS Code extension reads `name` / `expansion` /
/// `lineCount` directly.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExpandMacroResponse {
    /// The expanded macro name (without the leading backtick).
    pub name: String,
    /// The fully-recursive expansion text.
    pub expansion: String,
    /// Number of lines in `expansion`.
    pub line_count: u32,
    /// Human-readable reason the expansion could not be produced (e.g.
    /// "undefined macro"). When set, `expansion` is empty and the extension
    /// shows this message instead of opening an expansion tab. Omitted from
    /// the wire on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Extract the macro name the cursor is sitting on (`` `uvm_info `` →
/// `"uvm_info"`), or `None` when the identifier under the cursor isn't
/// backtick-prefixed. Cheap textual gate doubling as a name extractor: the
/// hover path uses it both to decide *whether* to attempt an expansion and
/// to answer "is this macro even defined?" before paying for a sidecar
/// round-trip.
pub(crate) fn macro_name_at_cursor(rope: &Rope, pos: MPosition) -> Option<String> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line = rope.line(pos.line as usize);

    // Collect the line and locate the cursor column in UTF-16 units
    // (matching LSP coordinates).
    let chars: Vec<char> = line.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let mut idx = 0usize;
    let mut utf16 = 0u32;
    for (i, c) in chars.iter().enumerate() {
        if utf16 >= pos.character {
            idx = i;
            break;
        }
        utf16 += c.len_utf16() as u32;
        idx = i + 1;
    }
    if idx > chars.len() {
        return None;
    }

    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    // Walk left over identifier characters from the cursor.
    let mut start = idx.min(chars.len());
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    // A macro usage is `<ident> preceded by a backtick.
    if start == 0 || chars[start - 1] != '`' {
        return None;
    }
    // Walk right to capture the full name (the cursor may sit mid-name).
    let mut end = start;
    while end < chars.len() && is_ident(chars[end]) {
        end += 1;
    }
    if end == start {
        return None; // a bare backtick with no identifier after it
    }
    Some(chars[start..end].iter().collect())
}

/// Hover footer for a backtick identifier with no `` `define `` anywhere in
/// the indexed project (and no `+define+` in the config). Shown *instead of*
/// a sidecar round-trip — an undefined macro can't expand, so the answer is
/// immediate rather than a multi-second "Loading…" that ends in nothing.
pub(crate) fn undefined_macro_footer(name: &str) -> String {
    format!(
        "\n\n---\n\n▶ `` `{name} `` — **undefined macro**: no `` `define `` found in the \
         project or its include chain, and no `+define+{name}` in the configuration"
    )
}

/// Build the hover footer markdown for an expansion result. Shows the line
/// count, a short fenced preview (first few lines), and the command CTA.
///
/// When `stale` is set the expansion came from the cache while the sidecar was
/// busy/unresponsive (see [`crate::slang_adapter::SlangAdapter::stale_expansion`]); the footer says
/// so, since the macro may have changed since it was last expanded.
pub(crate) fn macro_footer_markdown(r: &mimir_slang::ExpandMacroResult, stale: bool) -> String {
    // A defined macro that expanded to nothing — a conditional `` `ifdef ``
    // branch compiled the body away (e.g. `+define+UVM_EMPTY_MACROS` blanks
    // every `` `uvm_field_* ``). Report it honestly instead of "expands to
    // **0 lines**" followed by an empty code fence.
    if r.expanded_text.trim().is_empty() {
        let note = if stale {
            " _(cached — may be stale while slang re-elaborates)_"
        } else {
            ""
        };
        return format!(
            "\n\n---\n\n▶ `` `{name} `` is **defined but expands to nothing**{note} — its \
             body is empty in the active configuration (a compiled-out `` `ifdef `` branch, \
             e.g. `+define+UVM_EMPTY_MACROS`)",
            name = r.macro_name,
        );
    }
    const PREVIEW_LINES: usize = 6;
    let total = r.line_count;
    let preview: Vec<&str> = r.expanded_text.lines().take(PREVIEW_LINES).collect();
    let truncated = (total as usize) > preview.len();
    let mut body = preview.join("\n");
    if truncated {
        body.push_str("\n…");
    }
    let note = if stale {
        " _(cached — may be stale while slang re-elaborates)_"
    } else {
        ""
    };
    format!(
        "\n\n---\n\n▶ `` `{name} `` expands to **{total} line{plural}**{note} — \
         run **Mimir: Expand Macro** for the full expansion\n\n\
         ```systemverilog\n{body}\n```",
        name = r.macro_name,
        plural = if total == 1 { "" } else { "s" },
    )
}

/// Append the macro-expansion `footer` to an existing hover. When `base` is
/// `None` (the base hover didn't resolve the macro — common for UVM macros
/// the workspace index never indexed) a fresh markdown hover carrying just
/// the footer is returned. When `base` is a markdown hover the footer is
/// concatenated; otherwise the base is returned unchanged (we don't rewrite
/// scalar/plaintext hovers).
pub(crate) fn append_hover_footer(base: Option<Hover>, footer: String) -> Hover {
    match base {
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
            range,
        }) => Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("{value}{footer}"),
            }),
            range,
        },
        Some(other) => other,
        None => hover_from_markdown(footer.trim_start().to_string()),
    }
}

/// Read a line from `rope`, dropping any trailing CR/LF and the
/// surrounding whitespace. Returns `None` for an out-of-bounds line so
/// the resolve path can degrade gracefully if the rope drifted.
pub(crate) fn read_line_trimmed(rope: &Rope, line: u32) -> Option<String> {
    let idx = line as usize;
    if idx >= rope.len_lines() {
        return None;
    }
    let raw: String = rope.line(idx).chars().collect();
    Some(raw.trim_end_matches(['\r', '\n']).trim().to_owned())
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::{Range as MRange, TextDocument};
    use crate::backend::DocumentState;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Helper: a `found` expansion result with the given name/text.
    fn expansion(name: &str, text: &str) -> mimir_slang::ExpandMacroResult {
        mimir_slang::ExpandMacroResult {
            found: true,
            expanded_text: text.to_string(),
            macro_name: name.to_string(),
            usage_range: None,
            line_count: text.lines().count().max(1) as u32,
            diagnostics: Vec::new(),
        }
    }


    /// A fresh footer shows the macro name and a preview but carries no
    /// staleness note.
    #[test]
    fn macro_footer_fresh_has_no_stale_note() {
        let md = macro_footer_markdown(&expansion("A", "(((k)+1)*2)"), false);
        assert!(md.contains("`A `"), "footer should name the macro: {md}");
        assert!(md.contains("(((k)+1)*2)"), "footer should preview the expansion");
        assert!(!md.contains("may be stale"), "fresh footer must not be marked stale");
    }


    /// A defined-but-empty expansion (a compiled-out `` `ifdef `` branch, e.g.
    /// `UVM_EMPTY_MACROS`) is reported as such — never as "expands to 0 lines"
    /// with an empty code fence.
    #[test]
    fn macro_footer_empty_expansion_says_nothing() {
        let md = macro_footer_markdown(&expansion("uvm_field_int", ""), false);
        assert!(md.contains("`uvm_field_int `"), "footer should name the macro: {md}");
        assert!(
            md.contains("defined but expands to nothing"),
            "empty expansion must be reported honestly: {md}"
        );
        assert!(!md.contains("0 line"), "must not say '0 lines': {md}");
        assert!(!md.contains("```"), "must not emit an empty code fence: {md}");
    }

    /// The empty-expansion footer is still marked stale when served from the
    /// cache during a busy elaborate.
    #[test]
    fn macro_footer_empty_expansion_can_be_stale() {
        let md = macro_footer_markdown(&expansion("uvm_field_int", "   \n  "), true);
        assert!(
            md.contains("defined but expands to nothing"),
            "whitespace-only expansion counts as empty: {md}"
        );
        assert!(md.contains("may be stale"), "stale note must survive: {md}");
    }

    /// A stale footer (served from cache while slang is busy) is explicitly
    /// marked so the user knows it may be out of date.
    #[test]
    fn macro_footer_stale_is_marked() {
        let md = macro_footer_markdown(&expansion("A", "(((k)+1)*2)"), true);
        assert!(md.contains("may be stale"), "stale footer must be marked: {md}");
        assert!(md.contains("(((k)+1)*2)"), "stale footer still previews the expansion");
    }

    /// The undefined-macro footer names the macro and says "undefined".
    #[test]
    fn undefined_footer_names_the_macro() {
        let md = undefined_macro_footer("uvm_tpyo");
        assert!(md.contains("`uvm_tpyo `"), "footer should name the macro: {md}");
        assert!(md.contains("undefined macro"), "footer should say undefined: {md}");
        assert!(md.contains("+define+uvm_tpyo"), "footer should mention the config knob: {md}");
    }

    /// `macro_name_at_cursor` returns the full name with the cursor anywhere
    /// on it, and `None` off a backtick word.
    #[test]
    fn macro_name_at_cursor_extracts_name() {
        let rope = ropey::Rope::from_str("  `uvm_info(\"T\", \"hi\", UVM_LOW)\n");
        // Cursor mid-name.
        assert_eq!(
            macro_name_at_cursor(&rope, MPosition::new(0, 6)).as_deref(),
            Some("uvm_info"),
        );
        // Cursor at the very start of the name (right after the backtick).
        assert_eq!(
            macro_name_at_cursor(&rope, MPosition::new(0, 3)).as_deref(),
            Some("uvm_info"),
        );
        // Cursor on a plain identifier — no backtick.
        assert_eq!(macro_name_at_cursor(&rope, MPosition::new(0, 25)), None);
        // Out-of-bounds line.
        assert_eq!(macro_name_at_cursor(&rope, MPosition::new(9, 0)), None);
    }

    /// The extractor is also the boolean gate: backtick word → Some,
    /// plain word → None.
    #[test]
    fn macro_name_at_cursor_is_the_usage_gate() {
        let rope = ropey::Rope::from_str("`MY_MACRO\nplain_word\n");
        assert!(macro_name_at_cursor(&rope, MPosition::new(0, 4)).is_some());
        assert!(macro_name_at_cursor(&rope, MPosition::new(1, 4)).is_none());
    }


    /// `read_line_trimmed` returns the line text minus surrounding
    /// whitespace and the trailing newline.
    #[test]
    fn read_line_trimmed_strips_whitespace_and_newline() {
        let rope = ropey::Rope::from_str("module foo;\n  class bar;\nendmodule\n");
        assert_eq!(read_line_trimmed(&rope, 0).as_deref(), Some("module foo;"));
        assert_eq!(read_line_trimmed(&rope, 1).as_deref(), Some("class bar;"));
        assert_eq!(read_line_trimmed(&rope, 2).as_deref(), Some("endmodule"));
    }


    /// Out-of-bounds line returns `None`, not a panic.
    #[test]
    fn read_line_trimmed_oob_returns_none() {
        let rope = ropey::Rope::from_str("only one line\n");
        assert_eq!(read_line_trimmed(&rope, 99), None);
    }


    // ----------------------------------------------------------------------
    // hover — hover_for_symbol + read_macro_body
    // ----------------------------------------------------------------------

    /// Build a `DocumentState` for tests with the given text. The parsed
    /// `tree`/`index` are left empty — the hover helpers don't read them.
    fn doc_state(text: &str) -> DocumentState {
        DocumentState {
            document: TextDocument::new(text, 1),
            language_id: "systemverilog".to_string(),
            index: Vec::new(),
            tree: None,
            index_version: 0,
        }
    }


    /// Extract the markdown payload from a `Hover` — every hover we emit
    /// is `HoverContents::Markup`, so the match is total.
    fn hover_markdown_value(h: &Hover) -> &str {
        match &h.contents {
            HoverContents::Markup(MarkupContent { value, .. }) => value.as_str(),
            _ => panic!("expected MarkupContent, got {:?}", h.contents),
        }
    }


    /// Multi-line typedef: the name token lives on the *closing* line
    /// (`} state_e;`), so a single-line read would show a broken snippet.
    /// The hover must show the whole declaration block instead.
    #[test]
    fn hover_for_multiline_typedef_shows_full_block() {
        let url = url("file:///a.sv");
        let text = "typedef enum logic [1:0] {\n  IDLE,\n  BUSY\n} state_e;\n";
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(text));

        // Ranges as the indexer reports them: name on line 3, full block 0..=3.
        let s = Symbol {
            name: "state_e".to_string(),
            kind: MSymbolKind::Typedef,
            name_range: MRange::new(MPosition::new(3, 2), MPosition::new(3, 9)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(3, 10)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        let md = hover_markdown_value(&h);
        assert!(md.contains("typedef enum logic [1:0] {"), "block start missing: {md}");
        assert!(md.contains("IDLE"), "enum member missing: {md}");
        assert!(md.contains("} state_e;"), "block end missing: {md}");
    }

    /// Very long multi-line typedefs are elided, not dumped wholesale.
    #[test]
    fn hover_for_long_typedef_is_elided() {
        let url = url("file:///a.sv");
        let members: Vec<String> = (0..60).map(|i| format!("  M{i},")).collect();
        let text = format!("typedef enum {{\n{}\n  LAST\n}} big_e;\n", members.join("\n"));
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(&text));

        let last_line = text.lines().count() as u32 - 1;
        let s = Symbol {
            name: "big_e".to_string(),
            kind: MSymbolKind::Typedef,
            name_range: MRange::new(MPosition::new(last_line, 2), MPosition::new(last_line, 7)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(last_line, 8)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        let md = hover_markdown_value(&h);
        assert!(md.contains("more lines)"), "long block should be elided: {md}");
        assert!(!md.contains("LAST"), "elided tail should be dropped: {md}");
    }

    /// Bare non-callable symbol (class) → fenced declaration line.
    #[test]
    fn hover_for_class_returns_declaration_line() {
        let url = url("file:///a.sv");
        let text = "class apb_monitor extends uvm_monitor;\n  int x;\nendclass\n";
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(text));

        let s = Symbol {
            name: "apb_monitor".to_string(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(0, 6), MPosition::new(0, 17)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 8)),
            params: None,
            parent_class_name: Some("uvm_monitor".to_string()),
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        assert_eq!(
            hover_markdown_value(&h),
            "```systemverilog\nclass apb_monitor extends uvm_monitor;\n```",
        );
    }


    /// Callable symbol (function with params) → formatted markdown signature.
    #[test]
    fn hover_for_function_emits_signature() {
        let url = url("file:///a.sv");
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(""));

        let s = Symbol {
            name: "add".to_string(),
            kind: MSymbolKind::Function,
            name_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 3)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 0)),
            params: Some(vec![
                mimir_syntax::Param {
                    name: "a".into(),
                    ty: Some("int".into()),
                },
                mimir_syntax::Param {
                    name: "b".into(),
                    ty: Some("int".into()),
                },
            ]),
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        let v = hover_markdown_value(&h);
        // Signature is now rich markdown rather than a fenced code block.
        assert!(v.contains("**function**"), "keyword not bolded: {v:?}");
        assert!(v.contains("`add`"), "name not inline-coded: {v:?}");
        assert!(v.contains("*int*"), "type not italicized: {v:?}");
        assert!(!v.contains("```"), "no code fence expected: {v:?}");
    }


    /// Macro → `define` header + multi-line body.
    #[test]
    fn hover_for_macro_includes_body() {
        let url = url("file:///a.sv");
        let text = "`define MY_MACRO(x) \\\n    $display(\"hi: %0d\", x);\n";
        let mut docs = std::collections::HashMap::new();
        docs.insert(url.clone(), doc_state(text));

        // Line 1 has 27 chars (4 spaces + 23 chars of `$display(...);`); use
        // exactly that as `end.character` — the position just before `\n`.
        let s = Symbol {
            name: "MY_MACRO".to_string(),
            kind: MSymbolKind::Macro,
            name_range: MRange::new(MPosition::new(0, 8), MPosition::new(0, 16)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(1, 27)),
            params: Some(vec![mimir_syntax::Param {
                name: "x".into(),
                ty: None,
            }]),
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let h = hover_for_symbol(&s, &url, &docs).expect("hover content");
        let v = hover_markdown_value(&h);
        // Header is the synthesized signature; body is the trimmed body.
        assert!(
            v.starts_with("```systemverilog\n`define MY_MACRO(x)"),
            "got {v:?}"
        );
        assert!(
            v.contains("$display"),
            "expected body to include $display, got {v:?}"
        );
    }


    /// Module-kind symbol → fenced declaration line, falls back to disk
    /// when the open-doc store has no entry for the URL. We assert the
    /// open-doc path here; the disk path is exercised by integration
    /// tests.
    #[test]
    fn hover_for_unknown_url_returns_none_when_doc_absent() {
        let url = url("file:///never-opened.sv");
        let docs = std::collections::HashMap::new();
        let s = Symbol {
            name: "x".to_string(),
            kind: MSymbolKind::Variable,
            name_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 10)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        // No doc, and the path doesn't exist on disk either → None.
        assert!(hover_for_symbol(&s, &url, &docs).is_none());
    }


    // ----------------------------------------------------------------------
    // keyword / system-task hover help — `keyword_hover_at`
    // ----------------------------------------------------------------------

    /// Build (tree, rope) from `src` for the hover-help tests.
    fn parse_for_hover(src: &str) -> (mimir_syntax::SyntaxTree, Rope) {
        mimir_core::logging::init_for_tests();
        let mut parser = mimir_syntax::SyntaxParser::new().expect("parser");
        let tree = parser.parse(src, None).expect("parse");
        (tree, Rope::from_str(src))
    }


    /// Cursor on `always_ff` returns the curated doc popup with the
    /// keyword in a fenced block and the description below.
    #[test]
    fn hover_on_always_ff_returns_doc() {
        let src = "module m;\n  always_ff @(posedge clk) q <= d;\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        let h = keyword_hover_at(&tree, &rope, MPosition::new(1, 2)).expect("hover");
        let v = hover_markdown_value(&h);
        assert!(v.starts_with("```systemverilog\nalways_ff\n```"), "got {v:?}");
        assert!(v.contains("Edge-sensitive sequential always block"), "got {v:?}");
        assert!(v.contains("§9.2.2.4"), "expected LRM ref, got {v:?}");
    }


    /// Cursor on `$display` resolves through the `$…` table.
    #[test]
    fn hover_on_dollar_display_returns_doc() {
        let src = "module m;\ninitial $display(\"hi\");\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        // Line 1, column 8 is the `$`.
        let h = keyword_hover_at(&tree, &rope, MPosition::new(1, 8)).expect("hover");
        let v = hover_markdown_value(&h);
        assert!(v.starts_with("```systemverilog\n$display\n```"), "got {v:?}");
        assert!(v.contains("Print arguments followed by a newline"), "got {v:?}");
    }


    /// A keyword we deliberately don't document (`endmodule` — structural
    /// noise) returns `None`. Guards against the fallback ever emitting an
    /// empty / surprising popup.
    #[test]
    fn hover_on_undocumented_keyword_returns_none() {
        let src = "module m;\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        // Line 1, column 0 is the 'e' of "endmodule".
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(1, 0)).is_none());
    }


    /// Punctuation / whitespace / off-the-end positions return `None`.
    #[test]
    fn hover_on_non_word_returns_none() {
        let src = "module m;\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        // Column 6 is the space between "module" and "m".
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(0, 6)).is_none());
        // Way off the end of the document.
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(99, 0)).is_none());
    }


    /// `$DISPLAY` (uppercase) is *not* in the system-task table — the LRM
    /// treats system tasks as case-sensitive. The fallback must not paper
    /// over that and return `$display`'s doc.
    #[test]
    fn hover_on_uppercase_system_task_returns_none() {
        let src = "module m;\ninitial $DISPLAY(\"hi\");\nendmodule\n";
        let (tree, rope) = parse_for_hover(src);
        assert!(keyword_hover_at(&tree, &rope, MPosition::new(1, 8)).is_none());
    }


    /// `read_macro_body` strips the `\`define NAME(args)` header.
    #[test]
    fn read_macro_body_strips_define_header() {
        let url = url("file:///a.sv");
        let text = "`define FOO(a, b) a + b\n";
        let docs_state = doc_state(text);
        let rope = Rope::from_str(&docs_state.document.text());

        let s = Symbol {
            name: "FOO".to_string(),
            kind: MSymbolKind::Macro,
            name_range: MRange::new(MPosition::new(0, 8), MPosition::new(0, 11)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 23)),
            params: Some(vec![
                mimir_syntax::Param {
                    name: "a".into(),
                    ty: None,
                },
                mimir_syntax::Param {
                    name: "b".into(),
                    ty: None,
                },
            ]),
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let body = read_macro_body(&s, &url, Some(&rope)).expect("body extracted");
        assert_eq!(body, "a + b");
    }


    // typedef_base_from_line

    #[test]
    fn typedef_base_logic_vector() {
        assert_eq!(
            typedef_base_from_line("typedef logic [31:0] addr_t;", "addr_t"),
            Some("logic [31:0]".to_string())
        );
    }


    #[test]
    fn typedef_base_enum() {
        assert_eq!(
            typedef_base_from_line("typedef enum logic { A, B } my_e;", "my_e"),
            Some("enum logic { A, B }".to_string())
        );
    }


    #[test]
    fn typedef_base_struct() {
        assert_eq!(
            typedef_base_from_line("typedef struct { int x; int y; } point_t;", "point_t"),
            Some("struct { int x; int y; }".to_string())
        );
    }


    #[test]
    fn typedef_base_forward_class_returns_none() {
        assert_eq!(
            typedef_base_from_line("typedef class MyClass;", "MyClass"),
            None
        );
    }


    #[test]
    fn typedef_base_simple_alias() {
        assert_eq!(
            typedef_base_from_line("typedef int my_int_t;", "my_int_t"),
            Some("int".to_string())
        );
    }
}
