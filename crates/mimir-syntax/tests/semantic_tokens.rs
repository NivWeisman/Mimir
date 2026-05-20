//! Integration tests for [`mimir_syntax::semantic_tokens`].
//!
//! These tests exercise the public `semantic_tokens()` function with
//! larger, UVM-realistic SV snippets that combine multiple token categories
//! in a single parse.  The goal is to catch regressions where a change to
//! one classifier arm silently breaks another.

use mimir_core::logging::init_for_tests;
use mimir_syntax::semantic_tokens::{semantic_tokens, RawToken, TokenModifier, TokenType};
use mimir_syntax::SyntaxParser;
use ropey::Rope;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn classify(src: &str) -> Vec<RawToken> {
    init_for_tests();
    let mut parser = SyntaxParser::new().expect("parser");
    let tree = parser.parse(src, None).expect("parse");
    semantic_tokens(&tree, &Rope::from_str(src), false)
}

/// Return every token whose source text equals `needle`.
fn find_all<'a>(toks: &'a [RawToken], src: &str, needle: &str, rope: &Rope) -> Vec<&'a RawToken> {
    toks.iter()
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
            src.get(start_byte..end_byte) == Some(needle)
        })
        .collect()
}

fn find_one<'a>(toks: &'a [RawToken], src: &str, needle: &str, rope: &Rope) -> &'a RawToken {
    let hits = find_all(toks, src, needle, rope);
    assert!(!hits.is_empty(), "no token for {needle:?}");
    hits[0]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A condensed UVM register-field class exercises:
///  - `rand uvm_reg_data_t` field → typedef type reference
///  - `output uvm_status_e` task param → typedef type reference
///  - typedef declaration alias → Type + DECLARATION
///  - task declaration name → Function + DECLARATION
///  - method call `this.value.set(x)` → callee `set` is Function
#[test]
fn uvm_reg_field_snippet_types_and_calls() {
    let src = "\
package uvm_pkg;
  typedef logic [63:0] uvm_reg_data_t;
  typedef logic [1:0]  uvm_status_e;
endpackage

class uvm_reg_field;
  rand uvm_reg_data_t value;

  task write(output uvm_status_e status, input uvm_reg_data_t data);
    value.set(data);
  endtask
endclass
";
    let toks = classify(src);
    let rope = Rope::from_str(src);

    // Typedef declaration aliases are Type + DECLARATION.
    let dt = find_one(&toks, src, "uvm_reg_data_t", &rope);
    assert_eq!(dt.token_type, TokenType::Type as u32, "uvm_reg_data_t decl should be Type");
    assert_eq!(dt.modifiers, TokenModifier::DECLARATION.0);

    // The `rand` field type reference is Type (no DECLARATION).
    let field_types = find_all(&toks, src, "uvm_reg_data_t", &rope);
    assert!(
        field_types.len() >= 2,
        "expected at least 2 uvm_reg_data_t tokens (decl + field + param)"
    );
    // All occurrences must be Type.
    for t in &field_types {
        assert_eq!(t.token_type, TokenType::Type as u32, "all uvm_reg_data_t should be Type: {t:?}");
    }

    // `uvm_status_e` in output param is Type.
    let status_types = find_all(&toks, src, "uvm_status_e", &rope);
    for t in &status_types {
        assert_eq!(t.token_type, TokenType::Type as u32, "uvm_status_e should be Type: {t:?}");
    }

    // `write` task declaration is Function + DECLARATION.
    let write_tok = find_one(&toks, src, "write", &rope);
    assert_eq!(write_tok.token_type, TokenType::Function as u32);
    assert_eq!(write_tok.modifiers, TokenModifier::DECLARATION.0);

    // `set` in `value.set(data)` is the callee — Function, no DECLARATION.
    let set_tok = find_one(&toks, src, "set", &rope);
    assert_eq!(set_tok.token_type, TokenType::Function as u32, "set() callee should be Function");
    assert_eq!(set_tok.modifiers, TokenModifier::NONE.0);
}

/// Chained call `a.b.c()` — only the last identifier `c` is Function;
/// `a` and `b` form the receiver chain and stay Variable.
#[test]
fn chained_method_calls_are_functions() {
    let src = "module m;\n  initial begin\n    a.b.c();\n  end\nendmodule\n";
    let toks = classify(src);
    let rope = Rope::from_str(src);

    let a = find_one(&toks, src, "a", &rope);
    assert_eq!(a.token_type, TokenType::Variable as u32, "a should be Variable (receiver)");

    let b = find_one(&toks, src, "b", &rope);
    assert_eq!(b.token_type, TokenType::Variable as u32, "b should be Variable (receiver)");

    let c = find_one(&toks, src, "c", &rope);
    assert_eq!(c.token_type, TokenType::Function as u32, "c should be Function (callee)");
}

/// Typedef declaration followed immediately by a field using that type —
/// both the alias (declaration) and the reference site get correct types.
/// Uses a class body where `my_byte_t data;` is unambiguously a field
/// declaration (not a potential module instantiation as at module scope).
#[test]
fn typedef_then_usage_in_same_class() {
    let src = "\
class c;
  typedef logic [7:0] my_byte_t;
  my_byte_t data;
endclass
";
    let toks = classify(src);
    let rope = Rope::from_str(src);

    let hits = find_all(&toks, src, "my_byte_t", &rope);
    assert_eq!(hits.len(), 2, "expected declaration + usage, got {hits:#?}");

    // First occurrence: the typedef alias — Type + DECLARATION.
    assert_eq!(hits[0].token_type, TokenType::Type as u32, "typedef alias should be Type");
    assert_eq!(hits[0].modifiers, TokenModifier::DECLARATION.0, "typedef alias should have DECLARATION");

    // Second occurrence: the field type reference — Type, no modifiers.
    assert_eq!(hits[1].token_type, TokenType::Type as u32, "type reference should be Type");
    assert_eq!(hits[1].modifiers, TokenModifier::NONE.0, "type reference should have no modifiers");
}

/// `x = get_value() + 1;` inside an always block — `get_value` is a function
/// call and must be classified as Function.
#[test]
fn function_call_in_expression_is_function() {
    let src = "\
module m;
  int x;
  always_comb begin
    x = get_value() + 1;
  end
endmodule
";
    let toks = classify(src);
    let rope = Rope::from_str(src);

    let callee = find_one(&toks, src, "get_value", &rope);
    assert_eq!(callee.token_type, TokenType::Function as u32, "get_value() should be Function");
    assert_eq!(callee.modifiers, TokenModifier::NONE.0);
}

/// `super.run_phase(...)` — the method name in a `method_call` (implicit
/// class handle) must be Function (via `method_call_body` parent).
/// Note: `function new(...)` has `new` as an anonymous keyword node, so it
/// does NOT appear as a `simple_identifier` and is correctly Keyword.
#[test]
fn super_calls_are_functions() {
    let src = "\
class my_test extends base_test;
  task run_phase(uvm_phase phase);
    super.run_phase(phase);
  endtask

  task connect_phase(uvm_phase phase);
    super.connect_phase(phase);
  endtask
endclass
";
    let toks = classify(src);
    let rope = Rope::from_str(src);

    // `run_phase` appears twice: task decl (Function+DECLARATION) and super call (Function).
    let rp_toks = find_all(&toks, src, "run_phase", &rope);
    assert_eq!(rp_toks.len(), 2, "expected task decl + super call for run_phase");
    assert_eq!(rp_toks[0].token_type, TokenType::Function as u32, "run_phase decl should be Function");
    assert_eq!(rp_toks[0].modifiers, TokenModifier::DECLARATION.0);
    assert_eq!(rp_toks[1].token_type, TokenType::Function as u32, "super.run_phase should be Function");
    assert_eq!(rp_toks[1].modifiers, TokenModifier::NONE.0);

    // Same check for connect_phase.
    let cp_toks = find_all(&toks, src, "connect_phase", &rope);
    assert_eq!(cp_toks.len(), 2, "expected task decl + super call for connect_phase");
    assert_eq!(cp_toks[1].token_type, TokenType::Function as u32, "super.connect_phase should be Function");
}
