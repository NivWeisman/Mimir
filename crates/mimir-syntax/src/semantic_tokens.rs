//! Semantic-token classifier for SystemVerilog.
//!
//! `textDocument/semanticTokens` is the LSP's "editor-side syntax
//! highlighting" feature: the server hands the editor a list of
//! (line, column, length, kind, modifiers) records and the editor
//! re-colors the visible source against the user's theme. We compute
//! these by walking the tree-sitter parse tree mimir already builds for
//! every open document.
//!
//! ## What this module emits
//!
//! Pure tree-sitter classifier — every decision is local to the node
//! kind and its parent's kind. No workspace symbol index, no slang.
//! That means:
//!
//! * Declaration-site identifiers (class name on `class my_class;`,
//!   function name on `function foo();`) are classified by walking up
//!   to the enclosing `*_declaration` node, so they get the right
//!   `class` / `function` / `parameter` colour and a `declaration`
//!   modifier.
//! * Type tokens (`int`, `logic`, `string`, `void`) are detected via
//!   their enclosing `data_type` / `integer_atom_type` /
//!   `integer_vector_type` and emitted as `type`.
//! * Keyword leaves (`always_ff`, `posedge`, `extends`, `module`, …)
//!   match the existing `crate::keywords::KEYWORDS` table and are
//!   emitted as `keyword`.
//! * Macro names (`text_macro_usage`/`text_macro_definition`) and
//!   system tasks (`$display`) get their own dedicated buckets.
//! * Identifier *references* (a name on the RHS of an expression, an
//!   element of a hierarchical path) carry no semantic info in the
//!   tree alone, so they're emitted as `variable` — that means a
//!   class reference and a real variable reference both colour as
//!   `variable` here. Fixing that requires the workspace symbol index
//!   and is a future slice.
//!
//! ## Output shape
//!
//! [`semantic_tokens`] returns a `Vec<RawToken>` in source order
//! (line-then-column). The server crate wraps these into LSP's
//! delta-encoded `SemanticToken` records — that encoding step is
//! protocol-specific and stays at the boundary, per the workspace's
//! "no `tower-lsp` in lower crates" rule.
//!
//! [`semantic_tokens_in_range`] is the same walker constrained to a
//! byte range; the server calls this for `semanticTokens/range`
//! requests when the editor only needs the visible viewport.

use ropey::Rope;
use tracing::trace;
use tree_sitter::Node;

use crate::keywords::KEYWORDS;
use crate::SyntaxTree;

/// Ordinal-stable list of semantic-token types we emit. The LSP wire
/// format encodes each token's type as an index into the legend the
/// server advertises in `initialize`, so this order *must not change*
/// once the server has booted — re-ordering would silently mis-colour
/// every open document.
///
/// New entries must go at the end. Removing an entry is a breaking
/// change to the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TokenType {
    /// Reserved SystemVerilog keyword (`module`, `always_ff`, `extends`).
    Keyword = 0,
    /// Built-in type (`int`, `logic`, `string`, `void`).
    Type = 1,
    /// Class name (declaration or reference site).
    Class = 2,
    /// Interface name.
    Interface = 3,
    /// Package / namespace name.
    Namespace = 4,
    /// Function / task / system-task name.
    Function = 5,
    /// Macro name (`` `define `` or `` `MACRO `` invocation).
    Macro = 6,
    /// Parameter / `localparam` name (declaration site).
    Parameter = 7,
    /// Default identifier classification when no more-specific rule
    /// matches. Coarse — see module docs.
    Variable = 8,
    /// Comment text — `// …` or `/* … */`.
    Comment = 9,
    /// String literal (including the surrounding quotes).
    String = 10,
    /// Numeric literal (integer, real, time).
    Number = 11,
    /// `%`-format specifier inside a string literal (`%0d`, `%h`, `%s`, …).
    /// Uses the LSP standard `regexp` token type so themes can colour it
    /// distinctly from the surrounding string body.
    Regexp = 12,
}

impl TokenType {
    /// Canonical names matching the LSP standard token-type list
    /// (LSP 3.16+). These strings become the `tokenTypes` legend the
    /// server advertises to clients.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Type => "type",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Namespace => "namespace",
            Self::Function => "function",
            Self::Macro => "macro",
            Self::Parameter => "parameter",
            Self::Variable => "variable",
            Self::Comment => "comment",
            Self::String => "string",
            Self::Number => "number",
            Self::Regexp => "regexp",
        }
    }

    /// Every token-type variant, in legend (ordinal) order.
    #[must_use]
    pub const fn legend() -> &'static [Self] {
        &[
            Self::Keyword,
            Self::Type,
            Self::Class,
            Self::Interface,
            Self::Namespace,
            Self::Function,
            Self::Macro,
            Self::Parameter,
            Self::Variable,
            Self::Comment,
            Self::String,
            Self::Number,
            Self::Regexp,
        ]
    }
}

/// Ordinal-stable list of semantic-token modifiers. Each modifier is
/// a bit in the wire-format `tokenModifiers` field — never reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenModifier(pub u32);

impl TokenModifier {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// Identifier introduced at this location (`class my_class;` — the
    /// `my_class` token).
    pub const DECLARATION: Self = Self(1 << 0);
    /// Read-only after initialization (`parameter`/`localparam`/`const`).
    pub const READONLY: Self = Self(1 << 1);

    /// Combine two modifier sets.
    #[must_use]
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Ordered list of modifier names, matching the bit positions of
    /// `DECLARATION` (0) and `READONLY` (1). Becomes the
    /// `tokenModifiers` legend.
    #[must_use]
    pub const fn legend_names() -> &'static [&'static str] {
        &["declaration", "readonly"]
    }
}

/// One semantic token in source order. Coordinates use LSP semantics:
/// `line` is a zero-based newline-separated line number; `start_col`
/// and `length` are in UTF-16 code units (LSP's required encoding).
///
/// The server converts a stream of these into LSP's delta-encoded
/// `SemanticToken` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawToken {
    /// Zero-based line index.
    pub line: u32,
    /// Zero-based UTF-16 column at the token's start.
    pub start_col: u32,
    /// Token length in UTF-16 code units.
    pub length: u32,
    /// Token type ordinal — index into [`TokenType::legend`].
    pub token_type: u32,
    /// Bitmask of [`TokenModifier`] bits.
    pub modifiers: u32,
}

/// Walk `tree` and emit every classifiable token in source order.
///
/// Returns tokens sorted by `(line, start_col)` — DFS pre-order over
/// the parse tree produces source order naturally; the contract is
/// asserted in unit tests.
///
/// When `format_specs` is `true` each `string_literal` is split into
/// alternating [`TokenType::String`] / [`TokenType::Regexp`] sub-tokens
/// so editors can colour `%`-format specifiers differently from the
/// surrounding text. Pass `false` to get the pre-feature behaviour (one
/// whole-string token per literal).
#[must_use]
pub fn semantic_tokens(tree: &SyntaxTree, rope: &Rope, format_specs: bool) -> Vec<RawToken> {
    let mut out = Vec::new();
    walk(tree.tree.root_node(), rope, None, format_specs, &mut out);
    trace!(count = out.len(), "collected semantic tokens");
    out
}

/// Same as [`semantic_tokens`], but skip nodes whose byte span lies
/// entirely outside `byte_range`. Used by the server for
/// `semanticTokens/range` requests so a huge file's coloring cost
/// scales with the visible viewport rather than the whole document.
///
/// `byte_range` is `[start, end)` in source bytes. `format_specs` has
/// the same meaning as in [`semantic_tokens`].
#[must_use]
pub fn semantic_tokens_in_range(
    tree: &SyntaxTree,
    rope: &Rope,
    byte_range: std::ops::Range<usize>,
    format_specs: bool,
) -> Vec<RawToken> {
    let mut out = Vec::new();
    walk(tree.tree.root_node(), rope, Some(byte_range), format_specs, &mut out);
    trace!(count = out.len(), "collected semantic tokens (ranged)");
    out
}

/// Tree walker. Recursive DFS. For each node:
///
/// * If it's a "stop" kind (string literal, comment, number,
///   `system_tf_identifier`) — emit one token and don't descend
///   (descending would produce overlapping inner tokens, which the
///   LSP protocol forbids).
/// * If it's a `simple_identifier` — classify by parent kind and emit
///   one token.
/// * If it's an anonymous keyword leaf — classify keyword-vs-type by
///   parent and emit.
/// * Otherwise, descend.
fn walk(
    node: Node<'_>,
    rope: &Rope,
    range: Option<std::ops::Range<usize>>,
    format_specs: bool,
    out: &mut Vec<RawToken>,
) {
    // Range filter: skip whole subtrees that don't overlap.
    if let Some(r) = &range {
        if node.end_byte() <= r.start || node.start_byte() >= r.end {
            return;
        }
    }

    let kind = node.kind();

    // ── stop kinds: emit a single token covering the whole node and
    //    don't descend (children would create overlaps).
    match kind {
        "one_line_comment" | "block_comment" => {
            push(node, rope, TokenType::Comment, TokenModifier::NONE, out);
            return;
        }
        "string_literal" => {
            if format_specs {
                emit_string_with_format_specs(node, rope, out);
            } else {
                push(node, rope, TokenType::String, TokenModifier::NONE, out);
            }
            return;
        }
        "integral_number" | "real_number" | "time_literal" => {
            push(node, rope, TokenType::Number, TokenModifier::NONE, out);
            return;
        }
        "system_tf_identifier" => {
            push(node, rope, TokenType::Function, TokenModifier::NONE, out);
            return;
        }
        _ => {}
    }

    // ── identifier classification by parent context.
    if kind == "simple_identifier" {
        let (ty, mods) = classify_identifier(node);
        push(node, rope, ty, mods, out);
        return;
    }

    // ── anonymous leaves whose `kind()` matches a reserved word are
    //    either `keyword` or `type`, depending on parent context.
    if !node.is_named() && node.child_count() == 0 && is_keyword_kind(kind) {
        let ty = if parent_is_type_container(node) {
            TokenType::Type
        } else {
            TokenType::Keyword
        };
        push(node, rope, ty, TokenModifier::NONE, out);
        return;
    }

    // ── otherwise descend.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, rope, range.clone(), format_specs, out);
    }
}

/// Push one [`RawToken`] for `node`. Skips tokens whose byte span is
/// empty (defensive — tree-sitter occasionally surfaces zero-width
/// nodes around ERROR recovery).
fn push(
    node: Node<'_>,
    rope: &Rope,
    token_type: TokenType,
    modifiers: TokenModifier,
    out: &mut Vec<RawToken>,
) {
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    if end_byte <= start_byte {
        return;
    }
    // tree-sitter `Point::row` is a `\n`-separated line index, which
    // matches LSP's `line` directly. For columns we go through the
    // rope so we get UTF-16 code units, not bytes.
    let start_point = node.start_position();
    let line = start_point.row as u32;

    let line_start_byte = rope.line_to_byte(line as usize);
    let start_col = utf16_len_between(rope, line_start_byte, start_byte);
    let length = utf16_len_between(rope, start_byte, end_byte);

    if length == 0 {
        return;
    }

    out.push(RawToken {
        line,
        start_col,
        length,
        token_type: token_type as u32,
        modifiers: modifiers.0,
    });
}

/// Push one [`RawToken`] directly from absolute byte offsets rather than a
/// tree-sitter `Node`. Used for sub-tokens within a `string_literal` where
/// we have no per-fragment node to hand to [`push`].
fn push_bytes(
    start_byte: usize,
    end_byte: usize,
    line: u32,
    line_start_byte: usize,
    rope: &Rope,
    token_type: TokenType,
    out: &mut Vec<RawToken>,
) {
    if end_byte <= start_byte {
        return;
    }
    let start_col = utf16_len_between(rope, line_start_byte, start_byte);
    let length = utf16_len_between(rope, start_byte, end_byte);
    if length == 0 {
        return;
    }
    out.push(RawToken {
        line,
        start_col,
        length,
        token_type: token_type as u32,
        modifiers: TokenModifier::NONE.0,
    });
}

/// Returns `true` for the ASCII bytes that legally terminate a SystemVerilog
/// `%`-format specifier (`%d`, `%0h`, `%8.0f`, …). The set is from IEEE
/// 1800-2017 §21.2.
fn is_sv_fmt_type(b: u8) -> bool {
    matches!(
        b,
        b'b' | b'B'
            | b'o' | b'O'
            | b'd' | b'D'
            | b'h' | b'H'
            | b'x' | b'X'
            | b's' | b'S'
            | b'e' | b'E'
            | b'f' | b'F'
            | b'g' | b'G'
            | b't'
            | b'm' | b'M'
            | b'p'
            | b'u'
            | b'v'
            | b'z'
            | b'c'
            | b'0'
    )
}

/// Split `string_literal` node into alternating [`TokenType::String`] and
/// [`TokenType::Regexp`] tokens, where `Regexp` covers each `%…` format
/// specifier and `String` covers everything else (including the surrounding
/// quote characters).
///
/// Falls through to a single whole-string token when no recognised specifiers
/// are present, matching the pre-feature behaviour exactly.
///
/// SV string literals are always single-line — `\n` inside a literal is the
/// two-character escape sequence, not a real newline — so all sub-tokens share
/// the node's line number.
fn emit_string_with_format_specs(node: Node<'_>, rope: &Rope, out: &mut Vec<RawToken>) {
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    let line = node.start_position().row as u32;
    let line_start = rope.line_to_byte(line as usize);

    // Collect bytes for ASCII scanning. Format specs are always ASCII so
    // multi-byte UTF-8 sequences in the surrounding text can't false-match.
    let text: Vec<u8> = rope.byte_slice(start_byte..end_byte).bytes().collect();

    let mut seg_start = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        if text[i] != b'%' {
            i += 1;
            continue;
        }
        let spec_start = i;
        i += 1;
        // "%%" is an escaped percent — not a format spec.
        if i < text.len() && text[i] == b'%' {
            i += 1;
            continue;
        }
        // Consume optional width / precision: digits and '.'.
        while i < text.len() && (text[i].is_ascii_digit() || text[i] == b'.') {
            i += 1;
        }
        // Must end with a recognised type character to be a valid spec.
        if i < text.len() && is_sv_fmt_type(text[i]) {
            i += 1;
            push_bytes(
                start_byte + seg_start,
                start_byte + spec_start,
                line,
                line_start,
                rope,
                TokenType::String,
                out,
            );
            push_bytes(
                start_byte + spec_start,
                start_byte + i,
                line,
                line_start,
                rope,
                TokenType::Regexp,
                out,
            );
            seg_start = i;
        }
        // Otherwise not a recognised spec — continue scanning.
    }
    // Remaining text after the last spec (or the whole literal if none found).
    push_bytes(
        start_byte + seg_start,
        end_byte,
        line,
        line_start,
        rope,
        TokenType::String,
        out,
    );
}

/// UTF-16 code-unit count between two byte offsets in `rope`.
/// Tokens that span newlines (multi-line strings, block comments)
/// would underflow this — they're rare in SV and the editor handles
/// them correctly so long as we report the lit length up to the next
/// newline; in practice the affected token kinds (`string_literal`,
/// `block_comment`) get one whole-node token here and the editor
/// applies it. If the token spans multiple lines the editor's
/// renderer trims to its own line-boundary semantics.
fn utf16_len_between(rope: &Rope, start_byte: usize, end_byte: usize) -> u32 {
    let start_char = rope.byte_to_char(start_byte);
    let end_char = rope.byte_to_char(end_byte);
    let start_u16 = rope.char_to_utf16_cu(start_char);
    let end_u16 = rope.char_to_utf16_cu(end_char);
    end_u16.saturating_sub(start_u16) as u32
}

/// Classify a `simple_identifier` by its parent's node kind. The
/// rules are intentionally narrow: we only emit a *specific* type
/// when the parent context makes the role unambiguous (declaration
/// sites, type references, macro names). Anything else falls back to
/// [`TokenType::Variable`].
fn classify_identifier(node: Node<'_>) -> (TokenType, TokenModifier) {
    let Some(parent) = node.parent() else {
        return (TokenType::Variable, TokenModifier::NONE);
    };
    match parent.kind() {
        "class_declaration" => {
            if is_first_named_kind(parent, "simple_identifier", node) {
                (TokenType::Class, TokenModifier::DECLARATION)
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "class_type" => {
            // In `pkg::ClassName`, both identifiers are direct children of
            // `class_type`. Distinguish the package qualifier (its next
            // sibling is `::`) from the actual class name (after `::` or
            // the only identifier).
            if node.next_sibling().is_some_and(|s| s.kind() == "::") {
                (TokenType::Namespace, TokenModifier::NONE)
            } else {
                (TokenType::Class, TokenModifier::NONE)
            }
        }
        "function_body_declaration" | "task_body_declaration" => {
            if is_first_named_kind(parent, "simple_identifier", node) {
                (TokenType::Function, TokenModifier::DECLARATION)
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "module_ansi_header" | "module_nonansi_header" => {
            if is_first_named_kind(parent, "simple_identifier", node) {
                (TokenType::Class, TokenModifier::DECLARATION)
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "interface_declaration" | "interface_ansi_header" | "interface_nonansi_header" => {
            if is_first_named_kind(parent, "simple_identifier", node) {
                (TokenType::Interface, TokenModifier::DECLARATION)
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "package_declaration" | "program_declaration" => {
            if is_first_named_kind(parent, "simple_identifier", node) {
                (TokenType::Namespace, TokenModifier::DECLARATION)
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "param_assignment" => {
            if is_first_named_kind(parent, "simple_identifier", node) {
                (
                    TokenType::Parameter,
                    TokenModifier::DECLARATION.or(TokenModifier::READONLY),
                )
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "text_macro_usage" | "text_macro_definition" | "text_macro_name" => {
            (TokenType::Macro, TokenModifier::NONE)
        }
        "type_declaration" => {
            // `typedef … alias_name;` — the alias is the *last*
            // simple_identifier child. Approximate by tagging any
            // simple_identifier here as a type; in practice tree-sitter
            // doesn't nest other identifiers directly under
            // `type_declaration`.
            (TokenType::Type, TokenModifier::DECLARATION)
        }
        "data_type" => {
            // A `simple_identifier` directly under `data_type` is a
            // user-defined type reference (typedef alias, enum, struct, or
            // class name). Built-in types (`int`, `logic`, `bit`, …)
            // appear as anonymous keyword nodes — they never reach this
            // branch. For package-scoped types (`pkg::MyType`), the
            // package qualifier appears as a sibling with a following `::`
            // token; classify it as Namespace so it colours consistently.
            if node.next_sibling().is_some_and(|s| s.kind() == "::") {
                (TokenType::Namespace, TokenModifier::NONE)
            } else {
                (TokenType::Type, TokenModifier::NONE)
            }
        }
        "hierarchical_identifier" => {
            // Both `foo(args)` and `obj.method(args)` produce a `tf_call`
            // whose first named child is `hierarchical_identifier`.
            // The *last* `simple_identifier` in that node is the callee;
            // earlier ones form the receiver chain and stay Variable.
            // Non-call uses of `hierarchical_identifier` (e.g. signal
            // references in expressions) have a different grandparent and
            // also fall through to Variable.
            let is_call = parent.parent().is_some_and(|gp| gp.kind() == "tf_call");
            if is_call && is_last_named_kind(parent, "simple_identifier", node) {
                (TokenType::Function, TokenModifier::NONE)
            } else {
                (TokenType::Variable, TokenModifier::NONE)
            }
        }
        "method_call_body" => {
            // `super.method(args)` / `this.method(args)` — the method
            // name `simple_identifier` is the `name:` field of
            // `method_call_body` (child of `method_call`).
            (TokenType::Function, TokenModifier::NONE)
        }
        _ => (TokenType::Variable, TokenModifier::NONE),
    }
}

/// True if `target` is the first child of `parent` whose `kind()`
/// equals `kind`. Used to disambiguate the declaration-name
/// `simple_identifier` from any later reference identifiers under the
/// same declaration node (e.g. `extends` argument under
/// `class_declaration`).
fn is_first_named_kind(parent: Node<'_>, kind: &str, target: Node<'_>) -> bool {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() == kind {
            return child.id() == target.id();
        }
    }
    false
}

/// True if `target` is the **last** child of `parent` whose `kind()`
/// equals `kind`. Used to identify the callee name in a
/// `hierarchical_identifier`: for `obj.method()`, `method` is the last
/// `simple_identifier`; earlier ones form the receiver chain.
fn is_last_named_kind(parent: Node<'_>, kind: &str, target: Node<'_>) -> bool {
    let mut last_id: Option<usize> = None;
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() == kind {
            last_id = Some(child.id());
        }
    }
    last_id == Some(target.id())
}

/// Parent kinds that turn an inner anonymous keyword token into a
/// type token rather than a generic keyword. E.g. `int` under
/// `integer_atom_type` is the type name `int`, not the keyword `int`.
fn parent_is_type_container(node: Node<'_>) -> bool {
    matches!(
        node.parent().map(|p| p.kind()),
        Some(
            "data_type"
                | "data_type_or_void"
                | "data_type_or_implicit"
                | "integer_atom_type"
                | "integer_vector_type"
                | "non_integer_type"
                | "net_type"
        )
    )
}

/// True when an anonymous leaf's `kind()` is a SV reserved keyword.
/// Tree-sitter sets `node.kind()` to the literal token text for
/// anonymous leaves; we look that up in our existing table.
fn is_keyword_kind(kind: &str) -> bool {
    KEYWORDS.contains(&kind)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxParser;
    use mimir_core::logging::init_for_tests;

    fn classify(src: &str) -> Vec<RawToken> {
        classify_with(src, false)
    }

    fn classify_with(src: &str, format_specs: bool) -> Vec<RawToken> {
        init_for_tests();
        let mut parser = SyntaxParser::new().expect("parser");
        let tree = parser.parse(src, None).expect("parse");
        semantic_tokens(&tree, &Rope::from_str(src), format_specs)
    }

    fn find_token<'a>(toks: &'a [RawToken], src: &str, needle: &str, rope: &Rope) -> &'a RawToken {
        toks.iter()
            .find(|t| {
                let line_start = rope.line_to_byte(t.line as usize);
                let utf16_to_byte = |col: u32| -> usize {
                    let line_start_char = rope.byte_to_char(line_start);
                    let target_char = rope.utf16_cu_to_char(
                        rope.char_to_utf16_cu(line_start_char) + col as usize,
                    );
                    rope.char_to_byte(target_char)
                };
                let start_byte = utf16_to_byte(t.start_col);
                let end_byte = utf16_to_byte(t.start_col + t.length);
                src.get(start_byte..end_byte) == Some(needle)
            })
            .unwrap_or_else(|| panic!("no token for {needle:?} in {toks:#?}"))
    }

    #[test]
    fn module_declaration_emits_keyword_and_class() {
        let src = "module top;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let kw = find_token(&toks, src, "module", &rope);
        assert_eq!(kw.token_type, TokenType::Keyword as u32);
        let name = find_token(&toks, src, "top", &rope);
        assert_eq!(name.token_type, TokenType::Class as u32);
        assert_eq!(name.modifiers, TokenModifier::DECLARATION.0);
    }

    #[test]
    fn class_declaration_marks_name_as_class_decl() {
        let src = "class my_class;\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let name = find_token(&toks, src, "my_class", &rope);
        assert_eq!(name.token_type, TokenType::Class as u32);
        assert_eq!(name.modifiers, TokenModifier::DECLARATION.0);
    }

    #[test]
    fn class_extends_reference_is_class_without_declaration() {
        let src = "class my_class extends base;\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let base = find_token(&toks, src, "base", &rope);
        assert_eq!(base.token_type, TokenType::Class as u32);
        assert_eq!(base.modifiers, TokenModifier::NONE.0);
    }

    #[test]
    fn package_scoped_class_qualifier_is_namespace_class_is_class() {
        // `pkg::my_class x;` — `pkg` is the package qualifier so it must
        // be Namespace; `my_class` is the class reference so it must be Class.
        let src = "module m;\n  pkg::my_class x;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let pkg = find_token(&toks, src, "pkg", &rope);
        assert_eq!(pkg.token_type, TokenType::Namespace as u32, "pkg should be Namespace");
        let cls = find_token(&toks, src, "my_class", &rope);
        assert_eq!(cls.token_type, TokenType::Class as u32, "my_class should be Class");
        assert_eq!(cls.modifiers, TokenModifier::NONE.0);
    }

    #[test]
    fn package_scoped_extends_qualifier_is_namespace() {
        // `class c extends pkg::base;` — same scoping rule in extends clause.
        let src = "class c extends pkg::base;\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let pkg = find_token(&toks, src, "pkg", &rope);
        assert_eq!(pkg.token_type, TokenType::Namespace as u32, "pkg should be Namespace in extends");
        let base = find_token(&toks, src, "base", &rope);
        assert_eq!(base.token_type, TokenType::Class as u32, "base should be Class in extends");
    }

    #[test]
    fn package_name_is_namespace() {
        let src = "package my_pkg;\nendpackage\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let name = find_token(&toks, src, "my_pkg", &rope);
        assert_eq!(name.token_type, TokenType::Namespace as u32);
    }

    #[test]
    fn interface_name_is_interface() {
        let src = "interface my_if;\nendinterface\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let name = find_token(&toks, src, "my_if", &rope);
        assert_eq!(name.token_type, TokenType::Interface as u32);
    }

    #[test]
    fn function_name_is_function() {
        let src = "class c;\n  function void foo(int a);\n  endfunction\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let name = find_token(&toks, src, "foo", &rope);
        assert_eq!(name.token_type, TokenType::Function as u32);
        assert_eq!(name.modifiers, TokenModifier::DECLARATION.0);
    }

    #[test]
    fn parameter_is_parameter_with_declaration_and_readonly() {
        let src = "module m;\n  parameter int W = 8;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let p = find_token(&toks, src, "W", &rope);
        assert_eq!(p.token_type, TokenType::Parameter as u32);
        let expected = TokenModifier::DECLARATION.or(TokenModifier::READONLY).0;
        assert_eq!(p.modifiers, expected);
    }

    #[test]
    fn int_is_type_logic_is_type() {
        let src = "module m;\n  int x;\n  logic y;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let int_tok = find_token(&toks, src, "int", &rope);
        assert_eq!(int_tok.token_type, TokenType::Type as u32);
        let logic_tok = find_token(&toks, src, "logic", &rope);
        assert_eq!(logic_tok.token_type, TokenType::Type as u32);
    }

    #[test]
    fn always_ff_is_keyword_posedge_is_keyword() {
        let src = "module m;\n  always_ff @(posedge clk) q <= d;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let always = find_token(&toks, src, "always_ff", &rope);
        assert_eq!(always.token_type, TokenType::Keyword as u32);
        let posedge = find_token(&toks, src, "posedge", &rope);
        assert_eq!(posedge.token_type, TokenType::Keyword as u32);
    }

    #[test]
    fn system_task_is_function() {
        let src = "module m;\n  initial $display(\"x\");\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let disp = find_token(&toks, src, "$display", &rope);
        assert_eq!(disp.token_type, TokenType::Function as u32);
    }

    #[test]
    fn string_literal_is_string_and_does_not_descend() {
        let src = "module m;\n  string s = \"hello\";\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let s = find_token(&toks, src, "\"hello\"", &rope);
        assert_eq!(s.token_type, TokenType::String as u32);
        // The inner `quoted_string_item 'hello'` must NOT have been
        // emitted as a separate token — that would overlap the string.
        assert!(
            toks.iter()
                .filter(|t| t.line == s.line)
                .filter(|t| {
                    // any other String/Variable token overlapping s
                    t.start_col >= s.start_col && t.start_col < s.start_col + s.length
                })
                .count()
                == 1,
            "expected exactly one token in the string-literal span, got {:#?}",
            toks
        );
    }

    #[test]
    fn comment_is_emitted_as_comment() {
        let src = "// hello world\nmodule m;\nendmodule\n";
        let toks = classify(src);
        let comment = toks
            .iter()
            .find(|t| t.token_type == TokenType::Comment as u32)
            .expect("comment token");
        assert_eq!(comment.line, 0);
        assert_eq!(comment.start_col, 0);
        // "// hello world" is 14 UTF-16 units.
        assert_eq!(comment.length, 14);
    }

    #[test]
    fn integer_literal_is_number() {
        let src = "module m;\n  int x = 42;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let n = find_token(&toks, src, "42", &rope);
        assert_eq!(n.token_type, TokenType::Number as u32);
    }

    #[test]
    fn macro_usage_name_is_macro() {
        let src = "module m;\n  initial `MY_MACRO(x);\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let mname = find_token(&toks, src, "MY_MACRO", &rope);
        assert_eq!(mname.token_type, TokenType::Macro as u32);
    }

    // ── typedef declaration & reference tests ────────────────────────────

    #[test]
    fn typedef_decl_alias_is_type_with_declaration() {
        // `typedef logic [7:0] byte_t;` — the alias name must be Type+DECLARATION.
        let src = "package p;\ntypedef logic [7:0] byte_t;\nendpackage\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let alias = find_token(&toks, src, "byte_t", &rope);
        assert_eq!(alias.token_type, TokenType::Type as u32, "typedef alias should be Type");
        assert_eq!(alias.modifiers, TokenModifier::DECLARATION.0, "typedef alias should have DECLARATION");
    }

    #[test]
    fn typedef_ref_in_variable_decl_is_type() {
        // `uvm_status_e status;` — typedef'd type in a variable declaration.
        let src = "module m;\n  uvm_status_e status;\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let ty = find_token(&toks, src, "uvm_status_e", &rope);
        assert_eq!(ty.token_type, TokenType::Type as u32, "uvm_status_e should be Type, got {:?}", ty);
    }

    #[test]
    fn typedef_ref_as_task_output_param_is_type() {
        // `task foo(output uvm_status_e s);` — typedef in parameter type position.
        let src = "class c;\n  task foo(output uvm_status_e s);\n  endtask\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let ty = find_token(&toks, src, "uvm_status_e", &rope);
        assert_eq!(ty.token_type, TokenType::Type as u32, "uvm_status_e param type should be Type");
    }

    #[test]
    fn typedef_ref_as_function_return_type_is_type() {
        // `function uvm_status_e get();` — typedef in function return type.
        let src = "class c;\n  function uvm_status_e get();\n  endfunction\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let ty = find_token(&toks, src, "uvm_status_e", &rope);
        assert_eq!(ty.token_type, TokenType::Type as u32, "uvm_status_e return type should be Type");
    }

    #[test]
    fn rand_field_typedef_is_type() {
        // `rand uvm_reg_data_t value;` — UVM rand field with typedef type.
        let src = "class c;\n  rand uvm_reg_data_t value;\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let ty = find_token(&toks, src, "uvm_reg_data_t", &rope);
        assert_eq!(ty.token_type, TokenType::Type as u32, "uvm_reg_data_t should be Type");
    }

    // ── function / method call-site tests ────────────────────────────────

    #[test]
    fn standalone_function_call_is_function() {
        // `foo()` — bare function call; the callee name must be Function.
        let src = "module m;\n  initial begin\n    foo();\n  end\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let callee = find_token(&toks, src, "foo", &rope);
        assert_eq!(callee.token_type, TokenType::Function as u32, "foo() callee should be Function");
        assert_eq!(callee.modifiers, TokenModifier::NONE.0);
    }

    #[test]
    fn method_call_name_is_function() {
        // `obj.method()` — the method name must be Function.
        let src = "module m;\n  initial begin\n    obj.method();\n  end\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let callee = find_token(&toks, src, "method", &rope);
        assert_eq!(callee.token_type, TokenType::Function as u32, "method should be Function");
        assert_eq!(callee.modifiers, TokenModifier::NONE.0);
    }

    #[test]
    fn method_call_receiver_is_variable() {
        // `obj.method()` — the receiver object must stay Variable.
        let src = "module m;\n  initial begin\n    obj.method();\n  end\nendmodule\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let receiver = find_token(&toks, src, "obj", &rope);
        assert_eq!(receiver.token_type, TokenType::Variable as u32, "obj receiver should be Variable");
    }

    #[test]
    fn super_dot_method_is_function() {
        // `super.run_phase(ph)` — method name via implicit_class_handle must be Function.
        // There are two occurrences of `run_phase`: declaration (Function+DECLARATION)
        // and the super call (Function, no DECLARATION).
        let src = "class c;\n  task run_phase(int ph);\n    super.run_phase(ph);\n  endtask\nendclass\n";
        let toks = classify(src);
        let rope = Rope::from_str(src);
        let matches: Vec<_> = toks
            .iter()
            .filter(|t| {
                let line_start = rope.line_to_byte(t.line as usize);
                let line_start_char = rope.byte_to_char(line_start);
                let start_char = rope.utf16_cu_to_char(
                    rope.char_to_utf16_cu(line_start_char) + t.start_col as usize,
                );
                let end_char = rope.utf16_cu_to_char(
                    rope.char_to_utf16_cu(line_start_char) + (t.start_col + t.length) as usize,
                );
                let start_byte = rope.char_to_byte(start_char);
                let end_byte = rope.char_to_byte(end_char);
                src.get(start_byte..end_byte) == Some("run_phase")
            })
            .collect();
        assert_eq!(matches.len(), 2, "expected two run_phase tokens, got {matches:#?}");
        // First: task declaration — Function + DECLARATION.
        assert_eq!(matches[0].token_type, TokenType::Function as u32);
        assert_eq!(matches[0].modifiers, TokenModifier::DECLARATION.0);
        // Second: super call — Function, no DECLARATION.
        assert_eq!(matches[1].token_type, TokenType::Function as u32, "super.run_phase should be Function");
        assert_eq!(matches[1].modifiers, TokenModifier::NONE.0, "super.run_phase should have no modifiers");
    }

    #[test]
    fn tokens_are_sorted_by_line_then_column() {
        let src = "module foo;\n  int x = 42;\n  always_comb y = x;\nendmodule\n";
        let toks = classify(src);
        for pair in toks.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                (a.line, a.start_col) <= (b.line, b.start_col),
                "tokens out of order: {a:?} then {b:?}",
            );
        }
    }

    #[test]
    fn ranged_classifier_skips_outside_byte_range() {
        let src = "module foo;\n  int x = 42;\n  string s = \"hi\";\nendmodule\n";
        let rope = Rope::from_str(src);
        // Range covering only line 1 ("  int x = 42;\n").
        let line1_start = rope.line_to_byte(1);
        let line2_start = rope.line_to_byte(2);
        init_for_tests();
        let mut parser = SyntaxParser::new().expect("parser");
        let tree = parser.parse(src, None).expect("parse");
        let toks = semantic_tokens_in_range(&tree, &rope, line1_start..line2_start, false);

        // Every emitted token must lie on line 1.
        for t in &toks {
            assert_eq!(t.line, 1, "ranged classifier leaked a token outside line 1: {t:?}");
        }
        // The string literal on line 2 must not appear.
        assert!(
            !toks.iter().any(|t| t.token_type == TokenType::String as u32),
            "string token from line 2 leaked into line 1 range: {toks:#?}",
        );
    }

    #[test]
    fn legend_length_matches_variant_count() {
        // Adding a TokenType variant without updating `legend()` would
        // make the wire format silently misaligned; this is the canary.
        assert_eq!(TokenType::legend().len(), 13);
        assert_eq!(TokenModifier::legend_names().len(), 2);
    }

    #[test]
    fn legend_ordinals_match_enum_values() {
        for (i, t) in TokenType::legend().iter().enumerate() {
            assert_eq!(
                *t as u32, i as u32,
                "legend[{i}] = {t:?} but variant ordinal is {}",
                *t as u32
            );
        }
    }

    // ── format-specifier sub-token tests ──────────────────────────────────

    #[test]
    fn format_spec_tokens_split() {
        // Two format specs in one string: %0d and %h.
        // Expected: "hi: " → String, "%0d" → Regexp, " and " → String,
        //           "%h" → Regexp, "!" → String.
        let src = "module m;\n  initial $display(\"hi: %0d and %h!\", x, y);\nendmodule\n";
        let toks = classify_with(src, true);
        let strings: Vec<_> = toks
            .iter()
            .filter(|t| t.token_type == TokenType::String as u32)
            .collect();
        let regexps: Vec<_> = toks
            .iter()
            .filter(|t| t.token_type == TokenType::Regexp as u32)
            .collect();
        assert_eq!(regexps.len(), 2, "expected 2 Regexp tokens, got {toks:#?}");
        // String tokens: text before first spec, between specs, after last spec.
        assert_eq!(strings.len(), 3, "expected 3 String tokens, got {toks:#?}");
        // Tokens must be in source order (left to right by start_col).
        let line_toks: Vec<_> = toks
            .iter()
            .filter(|t| {
                t.token_type == TokenType::String as u32
                    || t.token_type == TokenType::Regexp as u32
            })
            .filter(|t| t.line == strings[0].line)
            .collect();
        for w in line_toks.windows(2) {
            assert!(
                w[0].start_col < w[1].start_col,
                "tokens out of order: {:#?}",
                line_toks
            );
        }
    }

    #[test]
    fn string_no_specs_single_token() {
        // A plain string with no format specs: should produce exactly one
        // String token covering the whole literal, same as format_specs=false.
        let src = "module m;\n  string s = \"hello world\";\nendmodule\n";
        let toks = classify_with(src, true);
        let rope = Rope::from_str(src);
        let s = find_token(&toks, src, "\"hello world\"", &rope);
        assert_eq!(s.token_type, TokenType::String as u32);
        let on_line: Vec<_> = toks
            .iter()
            .filter(|t| t.line == s.line)
            .filter(|t| t.start_col >= s.start_col && t.start_col < s.start_col + s.length)
            .collect();
        assert_eq!(
            on_line.len(),
            1,
            "expected exactly one token in the plain string span, got {on_line:#?}"
        );
    }

    #[test]
    fn double_percent_not_spec() {
        // "%%" is an escaped percent and must NOT be treated as a format spec.
        let src = "module m;\n  initial $display(\"100%%\");\nendmodule\n";
        let toks = classify_with(src, true);
        assert!(
            toks.iter().all(|t| t.token_type != TokenType::Regexp as u32),
            "no Regexp tokens expected for '%%', got {toks:#?}"
        );
    }

    #[test]
    fn format_specs_off_single_token() {
        // With format_specs=false the whole string_literal is one String token.
        let src = "module m;\n  initial $display(\"hi: %0d\");\nendmodule\n";
        let toks = classify_with(src, false);
        let rope = Rope::from_str(src);
        let s = find_token(&toks, src, "\"hi: %0d\"", &rope);
        assert_eq!(s.token_type, TokenType::String as u32);
        assert!(
            toks.iter().all(|t| t.token_type != TokenType::Regexp as u32),
            "no Regexp tokens expected when format_specs=false, got {toks:#?}"
        );
    }
}
