//! Pure converters between mimir's internal types and the LSP wire shapes.
//!
//! Everything here is a stateless function (or small payload struct): edits
//! for the formatting handlers, fold/selection/semantic-token encodings,
//! symbol-kind maps, document-symbol nesting, completion-item builders, and
//! diagnostic conversion. No locks, no I/O — the handlers in
//! [`crate::backend`] call these to translate results onto the wire.

use mimir_core::Range as MRange;
use mimir_syntax::{Diagnostic as MDiagnostic, Symbol, SymbolKind as MSymbolKind};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionResponse, Diagnostic,
    DocumentSymbol, FoldingRange, FoldingRangeKind, InlayHint, InlayHintKind, InlayHintLabel,
    InsertTextFormat, Position, Range, SelectionRange, SemanticToken, SemanticTokenModifier,
    SemanticTokenType, SemanticTokensLegend, SymbolKind, TextEdit, Url,
};

use crate::slang_service::m_range_to_lsp;

/// Build a single whole-file [`TextEdit`] that replaces the current document
/// text with `new_text`.
///
/// Verible always emits the complete file even when `--lines` constrains which
/// lines it rewrites, so both `formatting` and `range_formatting` use this
/// helper to return one encompassing edit. The end position is computed in
/// UTF-16 code units (the LSP wire format) to satisfy clients that validate
/// positions against the advertised offset encoding.
pub(crate) fn whole_file_edit(rope: &ropey::Rope, new_text: &str) -> Vec<TextEdit> {
    let total_lines = rope.len_lines();
    let last_line_idx = total_lines.saturating_sub(1);
    let last_line = rope.line(last_line_idx);
    let last_col_utf16: u32 = last_line.chars().map(|c| c.len_utf16() as u32).sum();
    vec![TextEdit {
        range: Range {
            start: Position { line: 0, character: 0 },
            end: Position {
                line: last_line_idx as u32,
                character: last_col_utf16,
            },
        },
        new_text: new_text.to_owned(),
    }]
}

/// Build the [`TextEdit`] that turns `original` into `formatted_text` by
/// replacing only the lines that actually differ.
///
/// Used by `range_formatting`: Verible emits the whole file but (with
/// `--lines`) only rewrites the requested lines, so everything outside them
/// is a common prefix / suffix. We strip the identical leading and trailing
/// lines and replace the block in between — whole lines, from the start of
/// the first changed line to the start of the first unchanged line after it.
///
/// Pairing "lines s..=e of the original" with "lines s..=e of the output"
/// does **not** work: the formatter is free to join or split lines, so the
/// two blocks have different line counts — code below the range got
/// duplicated, or the tail of the reformatted block was dropped. Diffing
/// makes the edit correct by construction, whatever the formatter did.
///
/// `lsp_start` / `lsp_end` (the requested 0-based inclusive line range) are
/// only used to flag a formatter that strayed outside the selection.
///
/// Returns `None` when the two texts are identical (no change needed).
pub(crate) fn range_lines_edit(
    original: &ropey::Rope,
    formatted_text: &str,
    lsp_start: u32,
    lsp_end: u32,
) -> Option<Vec<TextEdit>> {
    let original_text = original.to_string();
    if original_text == formatted_text {
        return None;
    }

    // Lines *with* their terminators, so concatenating a run of them
    // reproduces the text byte-for-byte.
    let orig_lines: Vec<&str> = original_text.split_inclusive('\n').collect();
    let fmt_lines: Vec<&str> = formatted_text.split_inclusive('\n').collect();

    let max_common = orig_lines.len().min(fmt_lines.len());
    let prefix = orig_lines
        .iter()
        .zip(&fmt_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = orig_lines
        .iter()
        .rev()
        .zip(fmt_lines.iter().rev())
        .take(max_common - prefix) // never let the suffix overlap the prefix
        .take_while(|(a, b)| a == b)
        .count();

    let orig_changed = &orig_lines[prefix..orig_lines.len() - suffix];
    let fmt_changed = &fmt_lines[prefix..fmt_lines.len() - suffix];

    let start_byte: usize = orig_lines[..prefix].iter().map(|l| l.len()).sum();
    let end_byte: usize = start_byte + orig_changed.iter().map(|l| l.len()).sum::<usize>();
    let start = mimir_core::Position::from_byte_offset(original, start_byte);
    let end = mimir_core::Position::from_byte_offset(original, end_byte);

    if start.line < lsp_start || end.line > lsp_end.saturating_add(1) {
        tracing::debug!(
            requested_start = lsp_start,
            requested_end = lsp_end,
            edit_start = start.line,
            edit_end = end.line,
            "range format: formatter changed lines outside the requested range",
        );
    }

    Some(vec![TextEdit {
        range: m_range_to_lsp(MRange::new(start, end)),
        new_text: fmt_changed.concat(),
    }])
}

/// Convert a `mimir_syntax::FoldRange` into the `lsp_types` shape.
///
/// Whole-line folds — `start_character` / `end_character` are `None` so the
/// editor decides the exact column. `kind: Region` is the closest LSP fit
/// for SV constructs; the other choices (`Comment`, `Imports`) don't apply.
pub(crate) fn m_fold_to_lsp(f: mimir_syntax::FoldRange) -> FoldingRange {
    FoldingRange {
        start_line: f.start_line,
        start_character: None,
        end_line: f.end_line,
        end_character: None,
        kind: Some(FoldingRangeKind::Region),
        collapsed_text: None,
    }
}

/// Upper bound on the number of links in a `SelectionRange` chain we send.
///
/// The LSP type is a linked list (`parent: Box<SelectionRange>`), one link
/// per enclosing syntax node. Nobody presses "expand selection" hundreds of
/// times, but a cursor inside a generated `a | b | c | …` expression has
/// tens of thousands of enclosing nodes — and building, serialising, and
/// dropping a list that long each recurse once per link. That overflows the
/// stack, which aborts the whole server.
pub(crate) const MAX_SELECTION_DEPTH: usize = 256;

/// Link an innermost-first range chain into an `lsp_types::SelectionRange`,
/// where each entry's `parent` is the next-larger range. The returned value
/// is the innermost range with its parent chain attached. An empty chain
/// (cursor out of bounds) degenerates to a zero-width range at the chain's
/// implied position — but since we can't recover the position here, callers
/// pass at least the leaf range; an empty chain yields a single empty range
/// at (0,0) which the editor harmlessly ignores.
///
/// Chains longer than [`MAX_SELECTION_DEPTH`] are thinned from the middle:
/// the innermost steps (token → expression → statement, what the first few
/// keypresses select) and the outermost ones (…→ function → class → file)
/// are kept, the run of near-identical intermediate expression levels is
/// dropped.
pub(crate) fn build_selection_range(chain: &[MRange]) -> SelectionRange {
    // How many of the kept links come from the outer end.
    const OUTER_KEPT: usize = MAX_SELECTION_DEPTH / 4;
    let (inner, outer): (&[MRange], &[MRange]) = if chain.len() > MAX_SELECTION_DEPTH {
        (
            &chain[..MAX_SELECTION_DEPTH - OUTER_KEPT],
            &chain[chain.len() - OUTER_KEPT..],
        )
    } else {
        (chain, &[])
    };

    let mut acc: Option<SelectionRange> = None;
    // Walk outermost → innermost so each inner range points at the outer one.
    for r in outer.iter().rev().chain(inner.iter().rev()) {
        acc = Some(SelectionRange {
            range: m_range_to_lsp(*r),
            parent: acc.map(Box::new),
        });
    }
    acc.unwrap_or_else(|| SelectionRange {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        parent: None,
    })
}

/// Build the LSP semantic-tokens legend from
/// [`mimir_syntax::semantic_tokens::TokenType`]'s static list. Ordinals
/// here pin the wire format — must stay in lockstep with the enum.
pub(crate) fn semantic_tokens_legend() -> SemanticTokensLegend {
    use mimir_syntax::semantic_tokens::{TokenModifier, TokenType};
    SemanticTokensLegend {
        token_types: TokenType::legend()
            .iter()
            .map(|t| SemanticTokenType::new(t.name()))
            .collect(),
        token_modifiers: TokenModifier::legend_names()
            .iter()
            .map(|n| SemanticTokenModifier::new(n))
            .collect(),
    }
}

/// Convert source-order [`mimir_syntax::semantic_tokens::RawToken`]s into
/// LSP's delta-encoded `SemanticToken` records. Each record is a delta
/// from the previous token: `delta_line` is the row delta;
/// `delta_start` is the column delta within the same row, or the
/// absolute column when `delta_line > 0`.
///
/// Input must already be sorted by `(line, start_col)` — the classifier
/// guarantees this and a unit test asserts it.
pub(crate) fn encode_semantic_tokens(
    raw: &[mimir_syntax::semantic_tokens::RawToken],
) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(raw.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;
    for t in raw {
        let delta_line = t.line - prev_line;
        let delta_start = if delta_line == 0 {
            t.start_col - prev_col
        } else {
            t.start_col
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: t.length,
            token_type: t.token_type,
            token_modifiers_bitset: t.modifiers,
        });
        prev_line = t.line;
        prev_col = t.start_col;
    }
    out
}

/// Map our internal `SymbolKind` onto the LSP wire enum.
///
/// The LSP set is closed (numeric on the wire), so we map each variant
/// to the closest LSP equivalent. SystemVerilog-specific concepts
/// (`Property`, `Sequence`, `Covergroup`) don't have dedicated LSP
/// kinds; we fall back to `OBJECT` for those — VS Code renders them
/// with a neutral icon.
pub(crate) fn symbol_kind_to_lsp(kind: MSymbolKind) -> SymbolKind {
    match kind {
        MSymbolKind::Module => SymbolKind::MODULE,
        MSymbolKind::Interface => SymbolKind::INTERFACE,
        MSymbolKind::Program => SymbolKind::MODULE,
        MSymbolKind::Package => SymbolKind::PACKAGE,
        MSymbolKind::Class => SymbolKind::CLASS,
        MSymbolKind::Task => SymbolKind::FUNCTION,
        MSymbolKind::Function => SymbolKind::FUNCTION,
        MSymbolKind::Method => SymbolKind::METHOD,
        MSymbolKind::Typedef => SymbolKind::TYPE_PARAMETER,
        MSymbolKind::Parameter => SymbolKind::CONSTANT,
        MSymbolKind::Variable => SymbolKind::VARIABLE,
        MSymbolKind::Port => SymbolKind::FIELD,
        MSymbolKind::EnumMember => SymbolKind::ENUM_MEMBER,
        MSymbolKind::Macro => SymbolKind::CONSTANT,
        // SV `constraint` blocks have no direct LSP kind; `OBJECT` is
        // the same neutral fallback we use for SVA properties /
        // covergroups.
        MSymbolKind::Constraint
        | MSymbolKind::Property
        | MSymbolKind::Sequence
        | MSymbolKind::Covergroup => SymbolKind::OBJECT,
    }
}

/// Map a mimir-syntax [`MSymbolKind`] to a completion item kind.
///
/// Uses the nearest LSP [`CompletionItemKind`] for each SV construct.
/// Exists in `mimir-server` (not `mimir-syntax`) to keep LSP types out of
/// the lower crates, per the dependency rule in `CLAUDE.md`.
pub(crate) fn symbol_kind_to_completion_kind(kind: MSymbolKind) -> CompletionItemKind {
    match kind {
        MSymbolKind::Module => CompletionItemKind::MODULE,
        MSymbolKind::Interface => CompletionItemKind::INTERFACE,
        MSymbolKind::Program => CompletionItemKind::MODULE,
        MSymbolKind::Package => CompletionItemKind::MODULE,
        MSymbolKind::Class => CompletionItemKind::CLASS,
        MSymbolKind::Task => CompletionItemKind::FUNCTION,
        MSymbolKind::Function => CompletionItemKind::FUNCTION,
        MSymbolKind::Method => CompletionItemKind::METHOD,
        MSymbolKind::Typedef => CompletionItemKind::CLASS,
        MSymbolKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        MSymbolKind::Constraint => CompletionItemKind::FIELD,
        MSymbolKind::Parameter => CompletionItemKind::CONSTANT,
        MSymbolKind::Variable => CompletionItemKind::VARIABLE,
        MSymbolKind::Port => CompletionItemKind::VARIABLE,
        MSymbolKind::Property => CompletionItemKind::PROPERTY,
        MSymbolKind::Sequence => CompletionItemKind::VALUE,
        MSymbolKind::Covergroup => CompletionItemKind::STRUCT,
        MSymbolKind::Macro => CompletionItemKind::CONSTANT,
    }
}

/// Payload stored in `CompletionItem.data` so a later
/// `completionItem/resolve` request can re-locate the symbol cheaply
/// (no re-parse, no workspace re-scan) and read its declaration line
/// out of the rope.
///
/// Only attached to syntax-side user-symbol items (same-file + cross-file
/// from the workspace index). Keywords and slang-sourced items leave
/// `data` empty — keywords have no declaration to read, and the slang
/// sidecar doesn't yet return ranges (a follow-up).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompletionResolveData {
    /// URL of the file the declaration lives in.
    pub(crate) url: Url,
    /// Zero-based line of the declaration (`Symbol::name_range.start.line`).
    pub(crate) line: u32,
}

/// Build the `data` JSON payload that pairs with a syntax-side
/// completion item. The result is `None` only on serde failure (which
/// shouldn't happen for our shape) — the route then ships an item with
/// no resolve data and the resolve handler treats it as a no-op.
pub(crate) fn make_resolve_data(url: &Url, name_line: u32) -> Option<serde_json::Value> {
    serde_json::to_value(CompletionResolveData {
        url: url.clone(),
        line: name_line,
    })
    .ok()
}

/// Build a `CompletionItem` for a SystemVerilog keyword.
///
/// When the keyword has a registered snippet body in
/// [`mimir_syntax::keywords::KEYWORD_SNIPPETS`] (e.g. `module`, `class`,
/// `always_ff`), the item carries `insert_text` + `Snippet` format and a
/// `"snippet"` detail so the popup distinguishes it. Otherwise it's a
/// bare keyword item — the editor inserts `label` verbatim.
pub(crate) fn keyword_completion_item(kw: &'static str) -> CompletionItem {
    match mimir_syntax::keywords::snippet_for(kw) {
        Some(body) => CompletionItem {
            label: kw.to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some("snippet".to_owned()),
            insert_text: Some(body.to_owned()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        },
        None => CompletionItem {
            label: kw.to_owned(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        },
    }
}

/// Build a `DocumentSymbol` (no children attached yet).
#[allow(deprecated)]
pub(crate) fn symbol_to_lsp_document_symbol(
    sym: &Symbol,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    DocumentSymbol {
        name: sym.name.clone(),
        detail: None,
        kind: symbol_kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        range: m_range_to_lsp(sym.full_range),
        selection_range: m_range_to_lsp(sym.name_range),
        children,
    }
}

/// Turn the DFS-ordered flat symbol index into the nested
/// `DocumentSymbol` tree the LSP wants. A class's methods become
/// children of the class; a package's classes become children of the
/// package; etc.
///
/// The `mimir-syntax::index` walker emits parents before their
/// descendants, so we can nest in a single linear pass: each symbol's
/// children are the contiguous run of subsequent symbols whose
/// `full_range` is contained in this symbol's `full_range`.
pub(crate) fn nest_symbols(symbols: &[Symbol]) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < symbols.len() {
        let (node, consumed) = nest_symbol_subtree(&symbols[i..]);
        out.push(node);
        i += consumed;
    }
    out
}

/// Build one subtree starting at `symbols[0]`; returns the node plus
/// the number of slice entries it (and its descendants) consumed.
pub(crate) fn nest_symbol_subtree(symbols: &[Symbol]) -> (DocumentSymbol, usize) {
    let head = &symbols[0];
    let mut children: Vec<DocumentSymbol> = Vec::new();
    let mut i = 1;
    while i < symbols.len() && head.full_range.contains_range(symbols[i].full_range) {
        let (child, consumed) = nest_symbol_subtree(&symbols[i..]);
        children.push(child);
        i += consumed;
    }
    let node = symbol_to_lsp_document_symbol(
        head,
        if children.is_empty() {
            None
        } else {
            Some(children)
        },
    );
    (node, i)
}

/// Convert one parameter-name [`mimir_syntax::inlay::InlayLabel`] into the
/// LSP `InlayHint` shape — the single emit path for every branch of the
/// `inlay_hint` handler (slang ref-map, tree-sitter method resolution, and
/// plain function/macro lookups all push identical hints).
pub(crate) fn param_inlay_hint(label: mimir_syntax::inlay::InlayLabel) -> InlayHint {
    InlayHint {
        position: Position::new(label.position.line, label.position.character),
        label: InlayHintLabel::String(label.text),
        kind: Some(InlayHintKind::PARAMETER),
        text_edits: None,
        tooltip: None,
        padding_left: None,
        padding_right: Some(true),
        data: None,
    }
}

/// Map a tree-sitter (`mimir-syntax`) diagnostic onto the LSP wire format.
///
/// Delegates to [`crate::diagnostics`] so the severity/range/code mapping
/// lives in exactly one place across both the tree-sitter and slang paths.
pub(crate) fn syntax_to_lsp_diagnostic(d: MDiagnostic) -> Diagnostic {
    crate::diagnostics::mimir_diag_to_lsp(&crate::diagnostics::syntax_diag_to_mimir(&d))
}

/// Normalize a completion response to `isIncomplete: true`.
///
/// With `isIncomplete: false` (the implicit meaning of
/// [`CompletionResponse::Array`]), VS Code treats the returned list as the
/// complete set for the current position: it caches the items and filters
/// them client-side as the user types, and crucially **stops re-querying the
/// server**. That breaks editing back into a member-access prefix — type
/// `obj.a_some`, delete a char to `obj.a_som`, and no request is sent, so the
/// suggestion list never re-pops. Marking every response incomplete makes the
/// client re-query on each edit (including deletion), keeping completion live
/// wherever the cursor lands. The server already recomputes candidates from
/// the buffer on each call, so the extra round-trips are cheap.
pub(crate) fn into_incomplete(resp: CompletionResponse) -> CompletionResponse {
    let items = match resp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    CompletionResponse::List(CompletionList { is_incomplete: true, items })
}

/// Merge tree-sitter and slang diagnostics for a single file into the LSP
/// shape we publish.
///
/// **Conflict policy.** When `slang_active` is `true` (slang elaborated
/// this file successfully), slang's diagnostics are authoritative —
/// tree-sitter syntax errors are dropped because they're often cascading
/// false positives from preprocessor-driven code tree-sitter can't see
/// through (the apb.sv `missing endpackage` is the canonical example).
/// When `slang_active` is `false` (no sidecar configured, sidecar crashed,
/// or this file wasn't in the elaboration set), tree-sitter is the only
/// source of truth.
///
/// `slang` is expected to already be filtered to diagnostics for **this
/// file**. The caller does the path → URI matching; this function just
/// merges per-file diagnostic sets.
///
/// Today this is always called with `slang_active = false` because the
/// sidecar binary doesn't exist yet (Stage 1). The function lives now so
/// Stage 3 only flips the flag.
pub(crate) fn merge_diagnostics(syntax: Vec<MDiagnostic>) -> Vec<Diagnostic> {
    syntax.into_iter().map(syntax_to_lsp_diagnostic).collect()
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::{Position as MPosition, Range as MRange};
    use mimir_syntax::Diagnostic as MDiag;
    use mimir_syntax::DiagnosticSeverity as MSeverity;
    use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

    /// Helper: a tree-sitter diagnostic at a given severity.
    fn syntax_diag(sev: MSeverity) -> MDiag {
        MDiag {
            range: MRange::new(MPosition::new(0, 0), MPosition::new(0, 1)),
            message: "syntax".to_string(),
            severity: sev,
            code: "syntax",
        }
    }


    // ------------------------------------------------------------------
    // Regression: range-formatting edits
    // ------------------------------------------------------------------

    /// Apply LSP text edits to `original` the way an editor would.
    fn apply_edits(original: &str, edits: &[TextEdit]) -> String {
        let mut doc = mimir_core::TextDocument::new(original, 1);
        // Editors apply edits back-to-front so earlier ranges stay valid.
        let mut sorted: Vec<&TextEdit> = edits.iter().collect();
        sorted.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
        for e in sorted {
            let range = MRange::new(
                MPosition::new(e.range.start.line, e.range.start.character),
                MPosition::new(e.range.end.line, e.range.end.character),
            );
            doc.apply_incremental_edit(range, &e.new_text, 2).unwrap();
        }
        doc.text()
    }

    /// The edit a range-format returns, applied to the original document,
    /// must reproduce the formatter's output exactly.
    fn assert_range_edit_reproduces(original: &str, formatted: &str, start: u32, end: u32) {
        let rope = ropey::Rope::from_str(original);
        let edits = range_lines_edit(&rope, formatted, start, end)
            .expect("texts differ, so an edit is expected");
        assert_eq!(
            apply_edits(original, &edits),
            formatted,
            "applying {edits:#?} must yield the formatter's output",
        );
    }

    /// Regression: the formatter *shortens* a line in the range. The edit's
    /// end column used to be taken from the formatted line, so the tail of
    /// the original line survived behind the replacement — and the
    /// replacement carried its own newline while the original's was kept,
    /// inserting a blank line on every format.
    #[test]
    fn regression_range_format_edit_when_line_gets_shorter() {
        assert_range_edit_reproduces(
            "module m;\n  assign   a  =  b;\n  assign c = d;\nendmodule\n",
            "module m;\n  assign a = b;\n  assign c = d;\nendmodule\n",
            1,
            1,
        );
    }

    /// Regression: the formatter *joins* lines. Pairing "lines s..=e of the
    /// original" with "lines s..=e of the output" then pulled lines from
    /// below the range into the replacement, duplicating them.
    #[test]
    fn regression_range_format_edit_when_lines_are_joined() {
        assert_range_edit_reproduces(
            "module m;\n  assign a =\n    b +\n    c;\n  wire w;\nendmodule\n",
            "module m;\n  assign a = b + c;\n  wire w;\nendmodule\n",
            1,
            3,
        );
    }

    /// Regression: the formatter *splits* a line. The replacement used to be
    /// cut off at the original line count, silently deleting code.
    #[test]
    fn regression_range_format_edit_when_a_line_is_split() {
        assert_range_edit_reproduces(
            "module m;\n  always_comb begin a = b; end\n  wire w;\nendmodule\n",
            "module m;\n  always_comb begin\n    a = b;\n  end\n  wire w;\nendmodule\n",
            1,
            1,
        );
    }

    /// Edge cases: change on the very last line, with and without a trailing
    /// newline, and non-ASCII text ahead of the change on the same line.
    #[test]
    fn regression_range_format_edit_at_end_of_document() {
        assert_range_edit_reproduces("module m;\nendmodule   \n", "module m;\nendmodule\n", 1, 1);
        assert_range_edit_reproduces("module m;\nendmodule   ", "module m;\nendmodule", 1, 1);
        assert_range_edit_reproduces(
            "// é\nassign  a=b; // ünïcödé\n",
            "// é\nassign a = b;  // ünïcödé\n",
            1,
            1,
        );
    }

    /// Identical output ⇒ no edit at all.
    #[test]
    fn range_format_edit_is_none_when_nothing_changed() {
        let text = "module m;\n  wire w;\nendmodule\n";
        assert!(range_lines_edit(&ropey::Rope::from_str(text), text, 0, 2).is_none());
    }

    /// The edit is minimal: untouched lines above and below are not part of
    /// the replaced range.
    #[test]
    fn range_format_edit_is_confined_to_changed_lines() {
        let original = "module m;\n  assign   a  =  b;\n  wire w;\nendmodule\n";
        let formatted = "module m;\n  assign a = b;\n  wire w;\nendmodule\n";
        let edits = range_lines_edit(&ropey::Rope::from_str(original), formatted, 1, 1).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range.start, Position::new(1, 0));
        assert_eq!(edits[0].range.end, Position::new(2, 0));
        assert_eq!(edits[0].new_text, "  assign a = b;\n");
    }

    /// Regression: `SelectionRange` is a linked list (`parent: Box<…>`), one
    /// link per enclosing syntax node. For a cursor inside a pathologically
    /// deep expression that was tens of thousands of links — and building,
    /// serialising and dropping such a list all recurse once per link, so
    /// the response overflowed the stack and aborted the server. The chain
    /// is capped; the innermost and outermost ranges (the useful ones) stay.
    #[test]
    fn regression_selection_range_chain_is_depth_capped() {
        let depth = 200_000u32;
        let chain: Vec<MRange> = (0..depth)
            .map(|i| MRange::new(MPosition::new(0, depth - i), MPosition::new(0, depth + 1 + i)))
            .collect();

        // Small stack on purpose: the default test stack hides the problem.
        let handle = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || {
                let sel = build_selection_range(&chain);
                let json = serde_json::to_string(&sel).expect("serialises");
                let mut links = 0usize;
                let mut cur = Some(&sel);
                let mut outermost = sel.range;
                while let Some(s) = cur {
                    links += 1;
                    outermost = s.range;
                    cur = s.parent.as_deref();
                }
                (links, sel.range, outermost, json.len())
            })
            .unwrap();
        let (links, innermost, outermost, json_len) =
            handle.join().expect("must not overflow the stack");

        assert!(links <= MAX_SELECTION_DEPTH, "{links} links");
        assert_eq!(innermost.start.character, depth, "innermost range is kept");
        assert_eq!(outermost.start.character, 1, "outermost range is kept");
        assert!(json_len > 0);
    }

    /// Ordinary chains are passed through untouched, innermost first.
    #[test]
    fn selection_range_chain_short_is_unchanged() {
        let chain: Vec<MRange> = (0..5u32)
            .map(|i| MRange::new(MPosition::new(0, 10 - i), MPosition::new(0, 11 + i)))
            .collect();
        let sel = build_selection_range(&chain);
        let mut got = Vec::new();
        let mut cur = Some(&sel);
        while let Some(s) = cur {
            got.push(s.range.start.character);
            cur = s.parent.as_deref();
        }
        assert_eq!(got, vec![10, 9, 8, 7, 6]);
    }

    /// tree-sitter → LSP conversion preserves all the fields the editor needs.
    #[test]
    fn syntax_diagnostic_conversion_preserves_fields() {
        let d = MDiag {
            range: MRange::new(MPosition::new(1, 2), MPosition::new(1, 5)),
            message: "boom".to_string(),
            severity: MSeverity::Error,
            code: "syntax",
        };
        let lsp = syntax_to_lsp_diagnostic(d);
        assert_eq!(lsp.range.start.line, 1);
        assert_eq!(lsp.range.start.character, 2);
        assert_eq!(lsp.range.end.line, 1);
        assert_eq!(lsp.range.end.character, 5);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("mimir"));
        assert_eq!(lsp.message, "boom");
    }


    /// All four tree-sitter severity variants map to the right LSP severity.
    #[test]
    fn syntax_severity_maps_completely() {
        let cases = [
            (MSeverity::Error, DiagnosticSeverity::ERROR),
            (MSeverity::Warning, DiagnosticSeverity::WARNING),
            (MSeverity::Information, DiagnosticSeverity::INFORMATION),
            (MSeverity::Hint, DiagnosticSeverity::HINT),
        ];
        for (ours, theirs) in cases {
            assert_eq!(
                syntax_to_lsp_diagnostic(syntax_diag(ours)).severity,
                Some(theirs)
            );
        }
    }


    /// `merge_diagnostics` maps tree-sitter diagnostics to LSP and
    /// returns them in order.
    #[test]
    fn merge_passes_through_syntax_diagnostics() {
        let merged = merge_diagnostics(vec![syntax_diag(MSeverity::Error)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].code,
            Some(NumberOrString::String("syntax".into()))
        );
    }


    /// Empty input returns empty — guards against accidental diagnostic invention.
    #[test]
    fn merge_empty_in_empty_out() {
        assert!(merge_diagnostics(vec![]).is_empty());
    }


    /// Every internal `SymbolKind` variant maps to *some* LSP kind.
    /// Guards against a future refactor that adds a variant but forgets
    /// the match arm — we'd rather see a missed-test failure than
    /// silently fall through.
    #[test]
    fn symbol_kind_to_lsp_covers_every_variant() {
        let cases = [
            (MSymbolKind::Module, SymbolKind::MODULE),
            (MSymbolKind::Interface, SymbolKind::INTERFACE),
            (MSymbolKind::Program, SymbolKind::MODULE),
            (MSymbolKind::Package, SymbolKind::PACKAGE),
            (MSymbolKind::Class, SymbolKind::CLASS),
            (MSymbolKind::Task, SymbolKind::FUNCTION),
            (MSymbolKind::Function, SymbolKind::FUNCTION),
            (MSymbolKind::Method, SymbolKind::METHOD),
            (MSymbolKind::Typedef, SymbolKind::TYPE_PARAMETER),
            (MSymbolKind::Parameter, SymbolKind::CONSTANT),
            (MSymbolKind::Variable, SymbolKind::VARIABLE),
            (MSymbolKind::Port, SymbolKind::FIELD),
            (MSymbolKind::EnumMember, SymbolKind::ENUM_MEMBER),
            (MSymbolKind::Macro, SymbolKind::CONSTANT),
            (MSymbolKind::Property, SymbolKind::OBJECT),
            (MSymbolKind::Sequence, SymbolKind::OBJECT),
            (MSymbolKind::Covergroup, SymbolKind::OBJECT),
            (MSymbolKind::Constraint, SymbolKind::OBJECT),
        ];
        for (mine, theirs) in cases {
            assert_eq!(symbol_kind_to_lsp(mine), theirs, "{mine:?}");
        }
    }


    /// `nest_symbols` over an empty index returns no roots.
    #[test]
    fn nest_symbols_empty() {
        assert!(nest_symbols(&[]).is_empty());
    }


    /// A single top-level symbol becomes a single root with no children.
    #[test]
    fn nest_symbols_single_top_level() {
        let s = Symbol {
            name: "my_mod".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(0, 7), MPosition::new(0, 13)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(2, 9)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let out = nest_symbols(&[s]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "my_mod");
        assert!(out[0].children.is_none());
    }


    /// A class with two methods produces one root with two children, in
    /// source order.
    #[test]
    fn nest_symbols_class_with_methods() {
        // class c spans lines 0..6; method f spans 1..2; method g spans 3..4.
        let class = Symbol {
            name: "c".into(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(0, 6), MPosition::new(0, 7)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(6, 8)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let f = Symbol {
            name: "f".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(1, 18), MPosition::new(1, 19)),
            full_range: MRange::new(MPosition::new(1, 4), MPosition::new(2, 12)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let g = Symbol {
            name: "g".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(3, 9), MPosition::new(3, 10)),
            full_range: MRange::new(MPosition::new(3, 4), MPosition::new(4, 8)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let out = nest_symbols(&[class, f, g]);
        assert_eq!(out.len(), 1);
        let class_node = &out[0];
        assert_eq!(class_node.name, "c");
        let kids = class_node
            .children
            .as_ref()
            .expect("class should have children");
        let kid_names: Vec<&str> = kids.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(kid_names, vec!["f", "g"]);
        assert!(kids[0].children.is_none());
        assert!(kids[1].children.is_none());
    }


    /// Two unrelated top-level symbols stay siblings.
    #[test]
    fn nest_symbols_two_siblings() {
        let a = Symbol {
            name: "a".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(0, 7), MPosition::new(0, 8)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(1, 9)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let b = Symbol {
            name: "b".into(),
            kind: MSymbolKind::Module,
            name_range: MRange::new(MPosition::new(2, 7), MPosition::new(2, 8)),
            full_range: MRange::new(MPosition::new(2, 0), MPosition::new(3, 9)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let out = nest_symbols(&[a, b]);
        let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }


    /// Three-deep nesting: package > class > method.
    #[test]
    fn nest_symbols_deeply_nested() {
        let pkg = Symbol {
            name: "p".into(),
            kind: MSymbolKind::Package,
            name_range: MRange::new(MPosition::new(0, 8), MPosition::new(0, 9)),
            full_range: MRange::new(MPosition::new(0, 0), MPosition::new(8, 10)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let cls = Symbol {
            name: "c".into(),
            kind: MSymbolKind::Class,
            name_range: MRange::new(MPosition::new(1, 6), MPosition::new(1, 7)),
            full_range: MRange::new(MPosition::new(1, 0), MPosition::new(6, 8)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let m = Symbol {
            name: "f".into(),
            kind: MSymbolKind::Method,
            name_range: MRange::new(MPosition::new(2, 18), MPosition::new(2, 19)),
            full_range: MRange::new(MPosition::new(2, 4), MPosition::new(3, 12)),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        let out = nest_symbols(&[pkg, cls, m]);
        assert_eq!(out.len(), 1);
        let pkg_node = &out[0];
        assert_eq!(pkg_node.name, "p");
        let pkg_kids = pkg_node.children.as_ref().unwrap();
        assert_eq!(pkg_kids.len(), 1);
        assert_eq!(pkg_kids[0].name, "c");
        let cls_kids = pkg_kids[0].children.as_ref().unwrap();
        assert_eq!(cls_kids.len(), 1);
        assert_eq!(cls_kids[0].name, "f");
    }


    // ------------------------------------------------------------------
    // symbol_kind_to_completion_kind
    // ------------------------------------------------------------------

    /// Every `MSymbolKind` variant must map to a `CompletionItemKind` — if
    /// a variant is added without updating the match, this test panics.
    #[test]
    fn completion_kind_maps_all_symbol_kinds() {
        let all = [
            MSymbolKind::Module,
            MSymbolKind::Interface,
            MSymbolKind::Program,
            MSymbolKind::Package,
            MSymbolKind::Class,
            MSymbolKind::Task,
            MSymbolKind::Function,
            MSymbolKind::Method,
            MSymbolKind::Typedef,
            MSymbolKind::EnumMember,
            MSymbolKind::Constraint,
            MSymbolKind::Parameter,
            MSymbolKind::Variable,
            MSymbolKind::Port,
            MSymbolKind::Property,
            MSymbolKind::Sequence,
            MSymbolKind::Covergroup,
            MSymbolKind::Macro,
        ];
        for kind in all {
            let _ = symbol_kind_to_completion_kind(kind);
        }
    }


    /// `Macro` → `CONSTANT` in both LSP-symbol and completion-item mappings.
    #[test]
    fn macro_symbol_kind_maps_to_constant() {
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Macro),
            CompletionItemKind::CONSTANT,
        );
        assert_eq!(symbol_kind_to_lsp(MSymbolKind::Macro), SymbolKind::CONSTANT);
    }


    /// `Class` maps to `CLASS`, `Method` to `METHOD` — spot-check the
    /// most important SV-specific entries.
    #[test]
    fn completion_kind_spot_checks() {
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Class),
            CompletionItemKind::CLASS,
        );
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Method),
            CompletionItemKind::METHOD,
        );
        assert_eq!(
            symbol_kind_to_completion_kind(MSymbolKind::Parameter),
            CompletionItemKind::CONSTANT,
        );
    }


    // ------------------------------------------------------------------
    // keyword_completion_item
    // ------------------------------------------------------------------

    /// `module` has a registered snippet → item carries `Snippet` format.
    #[test]
    fn keyword_with_snippet_emits_snippet_item() {
        let item = keyword_completion_item("module");
        assert_eq!(item.kind, Some(CompletionItemKind::KEYWORD));
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(item
            .insert_text
            .as_deref()
            .unwrap_or("")
            .contains("endmodule"));
        assert_eq!(item.detail.as_deref(), Some("snippet"));
    }


    /// A keyword without a snippet stays a bare keyword item.
    #[test]
    fn keyword_without_snippet_is_plain() {
        let item = keyword_completion_item("if");
        assert_eq!(item.kind, Some(CompletionItemKind::KEYWORD));
        assert!(item.insert_text.is_none());
        assert!(item.insert_text_format.is_none());
        assert!(item.detail.is_none());
    }


    // ------------------------------------------------------------------
    // CompletionResolveData / read_line_trimmed
    // ------------------------------------------------------------------

    /// `make_resolve_data` round-trips through serde back into a
    /// `CompletionResolveData` matching the inputs.
    #[test]
    fn resolve_data_round_trips() {
        let url = Url::parse("file:///tmp/a.sv").unwrap();
        let value = make_resolve_data(&url, 42).expect("serializes");
        let back: CompletionResolveData = serde_json::from_value(value).unwrap();
        assert_eq!(back.url, url);
        assert_eq!(back.line, 42);
    }


    // ----------------------------------------------------------------------
    // semantic tokens — encoder
    // ----------------------------------------------------------------------

    /// First token in the stream encodes as absolute coordinates.
    #[test]
    fn encode_semantic_tokens_first_token_is_absolute() {
        let raw = vec![mimir_syntax::semantic_tokens::RawToken {
            line: 3,
            start_col: 7,
            length: 5,
            token_type: 0,
            modifiers: 0,
        }];
        let enc = encode_semantic_tokens(&raw);
        assert_eq!(enc.len(), 1);
        assert_eq!(enc[0].delta_line, 3);
        assert_eq!(enc[0].delta_start, 7);
        assert_eq!(enc[0].length, 5);
    }


    /// Same-line follow-up tokens encode `delta_start` as the column
    /// delta from the previous token, not an absolute column.
    #[test]
    fn encode_semantic_tokens_same_line_uses_column_delta() {
        let raw = vec![
            mimir_syntax::semantic_tokens::RawToken {
                line: 0,
                start_col: 0,
                length: 6,
                token_type: 0,
                modifiers: 0,
            },
            mimir_syntax::semantic_tokens::RawToken {
                line: 0,
                start_col: 7,
                length: 3,
                token_type: 1,
                modifiers: 0,
            },
        ];
        let enc = encode_semantic_tokens(&raw);
        assert_eq!(enc[1].delta_line, 0);
        assert_eq!(enc[1].delta_start, 7); // 7 - 0 = 7
    }


    /// When `delta_line > 0` the encoder must reset `delta_start` to
    /// the absolute column, not the column delta from the prior line.
    #[test]
    fn encode_semantic_tokens_new_line_resets_column() {
        let raw = vec![
            mimir_syntax::semantic_tokens::RawToken {
                line: 0,
                start_col: 10,
                length: 3,
                token_type: 0,
                modifiers: 0,
            },
            mimir_syntax::semantic_tokens::RawToken {
                line: 2,
                start_col: 4,
                length: 5,
                token_type: 0,
                modifiers: 0,
            },
        ];
        let enc = encode_semantic_tokens(&raw);
        assert_eq!(enc[1].delta_line, 2);
        assert_eq!(enc[1].delta_start, 4); // absolute, not 4 - 10
    }


    /// The legend the server advertises must have exactly as many
    /// entries as the classifier produces ordinals for. Mismatched
    /// counts would silently misrender colours in every client.
    #[test]
    fn semantic_tokens_legend_matches_syntax_crate() {
        use mimir_syntax::semantic_tokens::{TokenModifier, TokenType};
        let legend = semantic_tokens_legend();
        assert_eq!(legend.token_types.len(), TokenType::legend().len());
        assert_eq!(legend.token_modifiers.len(), TokenModifier::legend_names().len());
    }
}
