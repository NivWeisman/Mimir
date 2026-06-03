//! UVM-specific lint checks over a tree-sitter parse tree.
//!
//! These are *verification-aware* diagnostics: they encode conventions of
//! the UVM methodology that a generic SystemVerilog parser knows nothing
//! about. Today there is one check — a phase-method override that forgets
//! to chain up to its `super` — but the module is the home for the rest of
//! the UVM lint family (factory registration, objection misuse, …).
//!
//! Everything here is **syntactic** (tree-sitter only, no slang): the
//! checks must run whether or not the sidecar is configured, and they
//! never need an elaborated symbol table.

use ropey::Rope;
use tree_sitter::Node;

use mimir_core::{Position, Range};

use crate::diagnostics::{Diagnostic, DiagnosticSeverity};
use crate::symbols::extract_callable_params;
use crate::SyntaxTree;

/// The standard UVM common phases. A method named like one of these, taking
/// a `uvm_phase` argument, is a phase override that should chain to `super`.
///
/// This is the default set the server falls back to when `.mimir.toml`
/// doesn't list its own `[diagnostics] uvm_phases`. Runtime phases
/// (`reset_phase`, `main_phase`, …) aren't in the default because their
/// `super` calls are objection-only and less universally expected; a
/// workspace that wants them can add them via config.
pub const DEFAULT_UVM_PHASES: &[&str] = &[
    "build_phase",
    "connect_phase",
    "end_of_elaboration_phase",
    "start_of_simulation_phase",
    "run_phase",
    "extract_phase",
    "check_phase",
    "report_phase",
    "final_phase",
];

/// Emit one diagnostic per UVM phase-method override whose body never calls
/// the matching `super.<phase>(...)`.
///
/// A node qualifies as a phase override when it is a `function_body_declaration`
/// or `task_body_declaration` (i.e. has a real body — `extern`/`pure`
/// prototypes are excluded) whose name is in `phases` *and* which declares a
/// parameter of type `uvm_phase`. The `uvm_phase` parameter is what
/// distinguishes a genuine phase override from an unrelated helper that
/// happens to share the name, so we don't need the class's `extends` chain.
///
/// Both in-class bodies and out-of-class definitions
/// (`function void cls::build_phase(...)`) are checked — both parse as
/// `*_body_declaration`.
pub fn phase_super_call_diagnostics(
    tree: &SyntaxTree,
    rope: &Rope,
    phases: &[String],
    severity: DiagnosticSeverity,
) -> Vec<Diagnostic> {
    let source = tree.source();
    let mut out = Vec::new();
    visit(tree.tree.root_node(), &mut |node| {
        check_phase_method(node, source, rope, phases, severity, &mut out);
    });
    out
}

/// If `node` is a phase-method body that doesn't chain to `super`, push a
/// diagnostic onto `out`.
fn check_phase_method(
    node: Node<'_>,
    source: &str,
    rope: &Rope,
    phases: &[String],
    severity: DiagnosticSeverity,
    out: &mut Vec<Diagnostic>,
) {
    if !matches!(
        node.kind(),
        "function_body_declaration" | "task_body_declaration"
    ) {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
        return;
    };
    if !phases.iter().any(|p| p == name) {
        return;
    }
    if !has_uvm_phase_param(node, source) {
        return;
    }
    if has_super_call(node, source, name) {
        return;
    }
    out.push(Diagnostic {
        range: node_range(name_node, rope),
        message: format!(
            "UVM phase `{name}` does not call `super.{name}(phase)` — \
             field automation and base-class setup won't run"
        ),
        severity,
        code: "uvm-phase-super",
    });
}

/// True when the callable `node` declares a parameter whose type mentions
/// `uvm_phase` (covers bare `uvm_phase`, `uvm_pkg::uvm_phase`, etc.).
fn has_uvm_phase_param(node: Node<'_>, source: &str) -> bool {
    extract_callable_params(node, source)
        .into_iter()
        .flatten()
        .any(|p| p.ty.as_deref().is_some_and(|t| t.contains("uvm_phase")))
}

/// True when the subtree rooted at `node` contains a `super.<name>(...)`
/// call — a `method_call` whose receiver is the `super` implicit class
/// handle and whose selector matches `name`.
fn has_super_call(node: Node<'_>, source: &str, name: &str) -> bool {
    let mut found = false;
    visit(node, &mut |n| {
        if found || n.kind() != "method_call" {
            return;
        }
        let mut c = n.walk();
        let kids: Vec<Node<'_>> = n.named_children(&mut c).collect();
        let is_super = kids.first().is_some_and(|f| {
            f.kind() == "implicit_class_handle"
                && f.utf8_text(source.as_bytes()).map(str::trim) == Ok("super")
        });
        if !is_super {
            return;
        }
        if let Some(mcb) = kids.iter().find(|k| k.kind() == "method_call_body") {
            if first_simple_identifier(*mcb, source).as_deref() == Some(name) {
                found = true;
            }
        }
    });
    found
}

/// Pre-order walk invoking `f` on every node in the subtree.
fn visit<'a>(node: Node<'a>, f: &mut impl FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, f);
    }
}

/// Text of the first `simple_identifier` descendant of `node`, if any.
fn first_simple_identifier(node: Node<'_>, source: &str) -> Option<String> {
    let mut result = None;
    visit(node, &mut |n| {
        if result.is_none() && n.kind() == "simple_identifier" {
            result = n.utf8_text(source.as_bytes()).ok().map(str::to_owned);
        }
    });
    result
}

/// Convert a tree-sitter node's byte span to an LSP range (UTF-16 columns).
fn node_range(node: Node<'_>, rope: &Rope) -> Range {
    let start = Position::from_byte_offset(rope, node.start_byte());
    let end = Position::from_byte_offset(rope, node.end_byte());
    Range::new(start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntaxParser;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let mut p = SyntaxParser::new().unwrap();
        let t = p.parse(src, None).unwrap();
        let rope = Rope::from_str(t.source());
        let phases: Vec<String> = DEFAULT_UVM_PHASES.iter().map(|s| s.to_string()).collect();
        phase_super_call_diagnostics(&t, &rope, &phases, DiagnosticSeverity::Warning)
    }

    #[test]
    fn flags_build_phase_without_super() {
        let src = "class my_comp extends uvm_component;\n\
                   function void build_phase(uvm_phase phase);\n\
                     int x = 1;\n\
                   endfunction\n\
                   endclass\n";
        let d = diags(src);
        assert_eq!(d.len(), 1, "expected one diagnostic, got {d:?}");
        assert!(d[0].message.contains("build_phase"));
        assert_eq!(d[0].code, "uvm-phase-super");
    }

    #[test]
    fn no_flag_when_super_called() {
        let src = "class my_comp extends uvm_component;\n\
                   function void build_phase(uvm_phase phase);\n\
                     super.build_phase(phase);\n\
                   endfunction\n\
                   endclass\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn flags_run_phase_task_without_super() {
        let src = "class my_comp extends uvm_component;\n\
                   task run_phase(uvm_phase phase);\n\
                     do_stuff();\n\
                   endtask\n\
                   endclass\n";
        let d = diags(src);
        assert_eq!(d.len(), 1, "expected one diagnostic, got {d:?}");
        assert!(d[0].message.contains("run_phase"));
    }

    #[test]
    fn no_flag_for_non_phase_method() {
        let src = "class my_comp extends uvm_component;\n\
                   function void helper(uvm_phase phase);\n\
                   endfunction\n\
                   endclass\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn no_flag_for_same_name_without_uvm_phase_param() {
        // A user function named build_phase that isn't a UVM phase override
        // (no uvm_phase argument) must not be flagged.
        let src = "class plain;\n\
                   function void build_phase(int x);\n\
                   endfunction\n\
                   endclass\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn respects_configured_phase_subset() {
        // With only connect_phase configured, a build_phase override without
        // super is not flagged.
        let mut p = SyntaxParser::new().unwrap();
        let src = "class c extends uvm_component;\n\
                   function void build_phase(uvm_phase phase);\n\
                   endfunction\n\
                   endclass\n";
        let t = p.parse(src, None).unwrap();
        let rope = Rope::from_str(t.source());
        let phases = vec!["connect_phase".to_string()];
        let d = phase_super_call_diagnostics(&t, &rope, &phases, DiagnosticSeverity::Warning);
        assert!(d.is_empty());
    }

    #[test]
    fn flags_out_of_class_body() {
        // Out-of-class definition `function void cls::build_phase(...)`.
        let src = "function void my_comp::build_phase(uvm_phase phase);\n\
                     int x = 1;\n\
                   endfunction\n";
        let d = diags(src);
        assert_eq!(d.len(), 1, "expected one diagnostic, got {d:?}");
    }
}
