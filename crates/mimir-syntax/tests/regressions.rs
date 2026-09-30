//! Regression suite for `mimir-syntax`.
//!
//! One test per bug that was found in the field or in review. Each test
//! reproduces the original failure through the crate's public API, so it
//! keeps guarding the behaviour no matter how the internals are refactored.
//! Add new entries at the bottom; never delete one without deleting the
//! feature it protects.

use mimir_core::{Position, Range};
use mimir_syntax::calls::{active_arg_index, call_site_at};
use mimir_syntax::semantic_tokens::{semantic_tokens, TokenType};
use mimir_syntax::symbols;
use mimir_syntax::{SyntaxParser, SyntaxTree};
use ropey::Rope;

/// Parse `src` and hand back the tree plus a rope over the same text.
fn parse(src: &str) -> (SyntaxTree, Rope) {
    let mut parser = SyntaxParser::new().expect("grammar loads");
    let tree = parser.parse(src, None).expect("parse");
    (tree, Rope::from_str(src))
}

/// LSP position of the `nth` (0-based) occurrence of `needle` in `src`.
fn pos_of(src: &str, needle: &str, nth: usize) -> Position {
    let byte = src
        .match_indices(needle)
        .nth(nth)
        .unwrap_or_else(|| panic!("occurrence {nth} of {needle:?} not found"))
        .0;
    Position::from_byte_offset(&Rope::from_str(src), byte)
}

// --------------------------------------------------------------------------
// Crash: parse-error snippet truncated in the middle of a UTF-8 sequence
// --------------------------------------------------------------------------

/// The "syntax error near `…`" snippet is capped at 40 bytes. Slicing at a
/// fixed *byte* offset panicked whenever byte 40 fell inside a multi-byte
/// character — i.e. typing a syntax error on a line with non-ASCII text
/// (a comment, a string, a stray paste) took the whole server down.
#[test]
fn regression_error_snippet_with_multibyte_text_does_not_panic() {
    // Two adjacent string literals (what a half-typed concatenation looks
    // like) put an ERROR node around the first one. Every shift (0..4) moves
    // a different byte of a multi-byte char onto the 40-byte cut, so at
    // least one of them is guaranteed to hit a non-boundary.
    for shift in 0..4 {
        let src = format!(
            "module m;\n  int x = \"{}{}\" \"ééé\" ééé;\nendmodule\n",
            "x".repeat(shift),
            "é".repeat(47),
        );
        let (tree, rope) = parse(&src);
        let diags = mimir_syntax::diagnostics::collect(&tree, &rope);
        assert!(!diags.is_empty(), "broken line must produce a diagnostic");
        for d in &diags {
            assert!(
                d.message.chars().count() < 80,
                "snippet must stay short: {}",
                d.message
            );
        }
    }
}

// --------------------------------------------------------------------------
// Crash: recursive tree walkers overflow the stack on deep trees
// --------------------------------------------------------------------------

/// A long left-associative expression (`a + a + a + …`, routine in generated
/// RTL) produces a parse tree as deep as it is long. Every recursive walker
/// used to burn one native stack frame per level and overflowed a 2 MiB
/// tokio worker stack — an abort no panic hook can catch. The walkers must
/// survive such a tree on a small stack.
#[test]
fn regression_deep_expression_does_not_overflow_the_stack() {
    let terms = 20_000;
    let mut src = String::from("module m;\n  assign x = a");
    for _ in 0..terms {
        src.push_str(" + a");
    }
    src.push_str(";\nendmodule\n");

    // 2 MiB = tokio's default worker-thread stack size.
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let (tree, rope) = parse(&src);
            let syms = symbols::index(&tree, &rope);
            let _ = mimir_syntax::diagnostics::collect(&tree, &rope);
            let toks = semantic_tokens(&tree, &rope, true);
            let _ = mimir_syntax::folding::folding_ranges(&tree);
            let occ = symbols::occurrences_of(&tree, &rope, "a");
            let _ = symbols::occurrences_of_scoped(&tree, &rope, "a");
            let _ = symbols::occurrences_of_at(&tree, &rope, Position::new(1, 13));
            let whole = Range::new(Position::new(0, 0), Position::new(2, 0));
            let _ = mimir_syntax::calls::call_sites_in(&tree, &rope, whole);
            let _ = mimir_syntax::config_db::db_calls(&tree, &rope);
            let _ = mimir_syntax::uvm::phase_super_call_diagnostics(
                &tree,
                &rope,
                &["build_phase".to_string()],
                mimir_syntax::DiagnosticSeverity::Warning,
            );
            (syms.len(), toks.len(), occ.len())
        })
        .expect("spawn");
    let (syms, toks, occ) = handle.join().expect("walkers must not overflow the stack");
    assert!(syms >= 1, "module symbol indexed");
    assert!(toks > terms, "every term gets a token");
    assert_eq!(occ, terms + 1, "every `a` is found");
}

// --------------------------------------------------------------------------
// Wrong result: highlight/references/rename from a declaration's own name
// --------------------------------------------------------------------------

/// With the cursor on the *declaration* name of a class, the scope search
/// used to pick the class itself as "the scope that declares this name" and
/// returned only occurrences inside the class body — so renaming a class
/// from its declaration left every same-file use untouched.
#[test]
fn regression_occurrences_from_class_declaration_include_outside_uses() {
    let src = "\
class packet;
  int x;
endclass
module m;
  packet p;
  initial p = new();
endmodule
";
    let (tree, rope) = parse(src);
    let from_decl = symbols::occurrences_of_at(&tree, &rope, pos_of(src, "packet", 0));
    let from_use = symbols::occurrences_of_at(&tree, &rope, pos_of(src, "packet", 1));
    assert_eq!(from_decl.len(), 2, "declaration + the `packet p;` use: {from_decl:?}");
    assert_eq!(from_decl, from_use, "same symbol ⇒ same occurrence set from either site");
}

/// Same defect for a function: cursor on the declaration name must also
/// find the call sites elsewhere in the file.
#[test]
fn regression_occurrences_from_function_declaration_include_call_sites() {
    let src = "\
module m;
  function int add1(int v);
    return v + 1;
  endfunction
  initial $display(add1(3));
  int y = add1(4);
endmodule
";
    let (tree, rope) = parse(src);
    let from_decl = symbols::occurrences_of_at(&tree, &rope, pos_of(src, "add1", 0));
    assert_eq!(from_decl.len(), 3, "declaration + two calls: {from_decl:?}");
}

/// Shadowing must keep working after the fix above: a local that re-declares
/// a name hides the outer one inside its own scope only.
#[test]
fn regression_shadowing_still_prunes_inner_scope() {
    let src = "\
module m;
  int cnt;
  function void f();
    int cnt;
    cnt = 1;
  endfunction
  initial cnt = 2;
endmodule
";
    let (tree, rope) = parse(src);
    // Outer `cnt` (module level): declaration + the `initial` use.
    let outer = symbols::occurrences_of_at(&tree, &rope, pos_of(src, "cnt", 0));
    assert_eq!(outer.len(), 2, "{outer:?}");
    // Inner `cnt`: declaration + the assignment inside `f`.
    let inner = symbols::occurrences_of_at(&tree, &rope, pos_of(src, "cnt", 1));
    assert_eq!(inner.len(), 2, "{inner:?}");
    assert!(outer.iter().all(|r| !inner.contains(r)), "scopes must not overlap");
}

// --------------------------------------------------------------------------
// Wrong result: a function-local symbol treated as workspace-visible
// --------------------------------------------------------------------------

/// `references` / `rename` fan out across the whole workspace. For a name
/// bound inside a function, task or `begin…end` block that is wrong — no
/// other file can refer to it — and rename would rewrite unrelated
/// declarations that merely share the name. The scanner has to be able to
/// tell the two cases apart.
#[test]
fn regression_local_binding_is_reported_as_file_local() {
    let src = "\
class c;
  int field;
  function void f(int arg);
    int tmp;
    tmp = arg + field;
  endfunction
endclass
";
    let (tree, rope) = parse(src);
    assert!(
        symbols::is_local_binding_at(&tree, &rope, pos_of(src, "tmp", 1)),
        "`tmp` is a function local",
    );
    assert!(
        symbols::is_local_binding_at(&tree, &rope, pos_of(src, "arg", 1)),
        "`arg` is a formal argument",
    );
    assert!(
        !symbols::is_local_binding_at(&tree, &rope, pos_of(src, "field", 1)),
        "`field` is a class member — reachable from other files",
    );
    assert!(
        !symbols::is_local_binding_at(&tree, &rope, pos_of(src, "c", 0)),
        "the class itself is workspace-visible",
    );
    assert!(
        !symbols::is_local_binding_at(&tree, &rope, pos_of(src, "f", 3)),
        "a method is reachable from other files",
    );
}

// --------------------------------------------------------------------------
// Crash + wrong result: signature-help active argument
// --------------------------------------------------------------------------

/// A call written without parentheses (`my_task;`) has no argument list, so
/// its "open paren" position was a `(0, 0)` placeholder. The active-argument
/// scan then sliced the file from byte 1 — a panic when the file starts with
/// a multi-byte character, and a garbage comma count otherwise.
#[test]
fn regression_active_arg_for_parenless_call_is_zero_and_does_not_panic() {
    let src = "\
// é, a, b, c
module m;
  task my_task; endtask
  initial begin
    my_task;
  end
endmodule
";
    // Make the very first byte multi-byte so `1..` is not a char boundary.
    let src = format!("é{src}");
    let (tree, rope) = parse(&src);
    let pos = pos_of(&src, "my_task;\n  end", 0);
    let inside = Position::new(pos.line, pos.character + 3);
    if let Some(call) = call_site_at(&tree, &rope, inside) {
        assert_eq!(active_arg_index(&call, &rope, inside), 0);
    }
}

/// Commas inside a string literal are text, not argument separators.
/// `` `uvm_info("TAG", "a, b, c", UVM_LOW) `` used to report the cursor in
/// the message string as argument 3.
#[test]
fn regression_active_arg_ignores_commas_inside_strings() {
    let src = "\
module m;
  initial begin
    foo(\"a, b, c\", 1, 2);
  end
endmodule
";
    let (tree, rope) = parse(src);
    let call_pos = pos_of(src, "foo", 0);
    // Cursor on the `1` — the second argument.
    let one = pos_of(src, ", 1,", 0);
    let cursor = Position::new(one.line, one.character + 2);
    let call = call_site_at(&tree, &rope, cursor).expect("call site");
    assert_eq!(call.name, "foo");
    assert_eq!(call.name_range.start, call_pos);
    assert_eq!(active_arg_index(&call, &rope, cursor), 1);
    // Cursor inside the string, after its commas — still the first argument.
    let in_str = pos_of(src, "b, c", 0);
    assert_eq!(active_arg_index(&call, &rope, in_str), 0);
}

// --------------------------------------------------------------------------
// Wrong result: hover signature with non-ASCII text
// --------------------------------------------------------------------------

/// The signature formatter copied non-identifier *bytes* one at a time as
/// `char`s, turning every multi-byte character into Latin-1 mojibake.
#[test]
fn regression_signature_formatter_preserves_non_ascii() {
    let out = mimir_syntax::hover_format::format_sv_signature("function int f(int größe → x)");
    for ch in ['ö', 'ß', '→'] {
        assert!(out.contains(ch), "{ch:?} lost or mangled: {out}");
    }
    assert!(
        !out.contains('Ã') && !out.contains('â'),
        "UTF-8 bytes were re-encoded as Latin-1: {out}",
    );
    // The ASCII parts are still classified.
    assert!(out.contains("**function**") && out.contains("*int*") && out.contains("`x`"));
}

// --------------------------------------------------------------------------
// Wrong result: semantic tokens that span several lines
// --------------------------------------------------------------------------

/// LSP semantic tokens may not span lines unless the client opts in (VS Code
/// does not). A `/* … */` comment covering three lines was emitted as one
/// token whose length ran past the end of its first line, so only that line
/// was coloured and the length was nonsense. Each line gets its own token.
#[test]
fn regression_multiline_comment_is_split_into_per_line_tokens() {
    let src = "\
module m;
  /* first
     second line
     third */
  int x;
endmodule
";
    let (tree, rope) = parse(src);
    let toks = semantic_tokens(&tree, &rope, false);
    let comments: Vec<_> = toks
        .iter()
        .filter(|t| t.token_type == TokenType::Comment as u32)
        .collect();
    assert_eq!(comments.len(), 3, "one token per comment line: {comments:#?}");
    for t in &toks {
        let line = rope.line(t.line as usize).to_string();
        let line_len = line.trim_end_matches(['\n', '\r']).encode_utf16().count() as u32;
        assert!(
            t.start_col + t.length <= line_len,
            "token {t:?} runs past the end of its line ({line_len} cols)",
        );
    }
    // Source order (line, then column) is what the delta encoder relies on.
    for w in toks.windows(2) {
        assert!((w[0].line, w[0].start_col) <= (w[1].line, w[1].start_col));
    }
}

/// A ranged request over the middle of a multi-line comment returns only the
/// lines inside the window — and every ranged token also appears, unchanged,
/// in the full result.
#[test]
fn regression_ranged_tokens_clip_multiline_tokens_to_the_window() {
    use mimir_syntax::semantic_tokens::semantic_tokens_in_range;
    let src = "\
module m;
  /* first
     second line
     third */
  int x;
endmodule
";
    let (tree, rope) = parse(src);
    let full = semantic_tokens(&tree, &rope, false);
    // Window = lines 2..4 (byte range of "     second line\n     third */\n").
    let start = rope.line_to_byte(2);
    let end = rope.line_to_byte(4);
    let ranged = semantic_tokens_in_range(&tree, &rope, start..end, false);
    assert!(!ranged.is_empty());
    for t in &ranged {
        assert!((2..4).contains(&t.line), "token outside the window: {t:?}");
        assert!(full.contains(t), "ranged token missing from the full set: {t:?}");
    }
    assert_eq!(ranged.len(), 2, "the comment's two in-window lines: {ranged:#?}");
}

// --------------------------------------------------------------------------
// Wrong result: fixed-size unpacked arrays offered associative-array methods
// --------------------------------------------------------------------------

/// `int a[10]` is a fixed-size array, not an associative one. Completion and
/// hover used to offer `exists` / `first` / `next` on it.
#[test]
fn regression_fixed_size_array_is_not_associative() {
    use mimir_syntax::builtin_methods::methods_for_suffix;
    assert!(methods_for_suffix("[10]").is_empty());
    assert!(!methods_for_suffix("[string]").is_empty(), "real assoc arrays still work");
    assert!(!methods_for_suffix("[$]").is_empty(), "queues still work");
}
