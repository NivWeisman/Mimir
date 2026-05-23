//! Inlay-hint formatting for call sites.
//!
//! Given a matched `(CallSite, Symbol)` pair, [`hints_for`] emits one
//! [`InlayLabel`] per argument, anchored *before* the argument expression.
//!
//! ## Label style
//!
//! | Call kind                  | Label format     |
//! |---------------------------|------------------|
//! | Function / Task / Method   | `"<name>: <type>"` (or `"<name>"` when type is unknown) |
//! | Macro                     | `"<name>"`        |
//!
//! When the call has more arguments than the declared parameter list the extra
//! arguments are silently skipped (variadic SV functions are rare; emitting
//! wrong labels is worse than emitting nothing).

use tracing::debug;

use crate::calls::{CallKind, CallSite};
use crate::symbols::Symbol;
use mimir_core::Position;

/// Controls the label format for method / function / task call inlay hints.
///
/// Macro hints always use the parameter name only, regardless of mode.
/// The default (`Name`) matches macro behavior so projects that haven't
/// set `[inlay_hints] method_hint` in `.mimir.toml` get the least-noisy output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MethodHintMode {
    /// Show only the parameter name: `"a"`. Default.
    #[default]
    Name,
    /// Show only the parameter type: `"int"`. Falls back to the name when the
    /// type is not known.
    Type,
    /// Show name and type together: `"a: int"`. Falls back to name-only when
    /// the type is not known.
    NameAndType,
}

/// One inlay-hint label anchored before a call argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayLabel {
    /// The label text, e.g. `"a: int"` or `"x"`.
    pub text: String,
    /// LSP position just before the argument — the editor places the hint here.
    pub position: Position,
}

/// Pair each argument of `call` with the matching formal parameter of `sym`
/// and produce one [`InlayLabel`] per pair.
///
/// The caller is responsible for ensuring `sym` is the *right* symbol for
/// this call site. For non-method calls a name lookup is enough; for
/// `CallKind::Method` the caller must have resolved the receiver type
/// (e.g. via the AST for `this`/`super` keywords, or via slang). When the
/// caller passes a method's symbol we trust it and emit hints — there is
/// no defensive skip here.
///
/// `mode` controls the label text for non-macro calls (macros always use
/// name-only regardless of `mode`). See [`MethodHintMode`].
///
/// Returns an empty vector when:
/// * `sym.params` is `None` (the symbol is not callable).
/// * `call.args` is empty.
#[must_use]
pub fn hints_for(call: &CallSite, sym: &Symbol, mode: MethodHintMode) -> Vec<InlayLabel> {
    let Some(params) = &sym.params else {
        return Vec::new();
    };

    if call.args.is_empty() {
        return Vec::new();
    }

    if call.args.len() > params.len() {
        debug!(
            name = %call.name,
            args = call.args.len(),
            params = params.len(),
            "arg count exceeds param count; skipping inlay hints for this call",
        );
        return Vec::new();
    }

    call.args
        .iter()
        .zip(params.iter())
        .map(|(arg, param)| {
            let text = match &call.kind {
                CallKind::Macro => param.name.clone(),
                _ => match (mode, &param.ty) {
                    (MethodHintMode::Name, _) => param.name.clone(),
                    (MethodHintMode::Type, Some(ty)) => ty.clone(),
                    (MethodHintMode::Type, None) => param.name.clone(),
                    (MethodHintMode::NameAndType, Some(ty)) => {
                        format!("{}: {}", param.name, ty)
                    }
                    (MethodHintMode::NameAndType, None) => param.name.clone(),
                },
            };
            InlayLabel {
                text,
                position: arg.range.start,
            }
        })
        .collect()
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::{ArgSpan, CallKind};
    use crate::symbols::{Param, SymbolKind};
    use mimir_core::{Position, Range};

    fn make_range(line: u32, start: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(line, end))
    }

    fn make_call(name: &str, kind: CallKind, args: Vec<Range>) -> CallSite {
        CallSite {
            name: name.to_string(),
            name_range: make_range(0, 0, name.len() as u32),
            kind,
            args: args.into_iter().map(|r| ArgSpan { range: r }).collect(),
            paren_open: Position::new(0, name.len() as u32),
            paren_close: Position::new(0, name.len() as u32 + 10),
        }
    }

    fn make_sym(name: &str, kind: SymbolKind, params: Vec<Param>) -> Symbol {
        let r = make_range(0, 0, 1);
        Symbol {
            name: name.to_string(),
            kind,
            name_range: r,
            full_range: r,
            params: Some(params),
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        }
    }

    #[test]
    fn function_call_hints_name_and_type() {
        let call = make_call(
            "foo",
            CallKind::Function,
            vec![make_range(1, 4, 5), make_range(1, 7, 8)],
        );
        let sym = make_sym(
            "foo",
            SymbolKind::Function,
            vec![
                Param { name: "a".into(), ty: Some("int".into()) },
                Param { name: "b".into(), ty: Some("string".into()) },
            ],
        );
        let hints = hints_for(&call, &sym, MethodHintMode::NameAndType);
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].text, "a: int");
        assert_eq!(hints[0].position, Position::new(1, 4));
        assert_eq!(hints[1].text, "b: string");
    }

    #[test]
    fn function_call_hints_name_only() {
        let call = make_call(
            "foo",
            CallKind::Function,
            vec![make_range(1, 4, 5), make_range(1, 7, 8)],
        );
        let sym = make_sym(
            "foo",
            SymbolKind::Function,
            vec![
                Param { name: "a".into(), ty: Some("int".into()) },
                Param { name: "b".into(), ty: Some("string".into()) },
            ],
        );
        let hints = hints_for(&call, &sym, MethodHintMode::Name);
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].text, "a");
        assert_eq!(hints[1].text, "b");
    }

    #[test]
    fn function_call_hints_type_only() {
        let call = make_call(
            "foo",
            CallKind::Function,
            vec![make_range(1, 4, 5), make_range(1, 7, 8)],
        );
        let sym = make_sym(
            "foo",
            SymbolKind::Function,
            vec![
                Param { name: "a".into(), ty: Some("int".into()) },
                Param { name: "b".into(), ty: None }, // no type → falls back to name
            ],
        );
        let hints = hints_for(&call, &sym, MethodHintMode::Type);
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].text, "int");
        assert_eq!(hints[1].text, "b"); // fallback to name when type unknown
    }

    #[test]
    fn macro_call_name_only_regardless_of_mode() {
        let call = make_call(
            "MY_MACRO",
            CallKind::Macro,
            vec![make_range(1, 10, 11), make_range(1, 13, 14)],
        );
        let sym = make_sym(
            "MY_MACRO",
            SymbolKind::Macro,
            vec![
                Param { name: "x".into(), ty: None },
                Param { name: "y".into(), ty: None },
            ],
        );
        // Even with NameAndType mode, macros always use name-only.
        for mode in [MethodHintMode::Name, MethodHintMode::Type, MethodHintMode::NameAndType] {
            let hints = hints_for(&call, &sym, mode);
            assert_eq!(hints.len(), 2, "mode {mode:?}");
            assert_eq!(hints[0].text, "x", "mode {mode:?}");
            assert_eq!(hints[1].text, "y", "mode {mode:?}");
        }
    }

    #[test]
    fn method_call_with_resolved_symbol_emits_hints() {
        // The caller (e.g. inlay handler resolving `this.method` /
        // `super.method` via the AST) is responsible for passing the
        // right symbol. Once they do, `hints_for` trusts them — no
        // defensive method-skip.
        let call = make_call(
            "method",
            CallKind::Method {
                receiver_text: "this".into(),
                receiver_range: make_range(0, 0, 4),
            },
            vec![make_range(1, 4, 5)],
        );
        let sym = make_sym(
            "method",
            SymbolKind::Method,
            vec![Param { name: "x".into(), ty: Some("int".into()) }],
        );
        let hints = hints_for(&call, &sym, MethodHintMode::NameAndType);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].text, "x: int");
    }

    #[test]
    fn too_many_args_returns_empty() {
        let call = make_call(
            "foo",
            CallKind::Function,
            vec![
                make_range(1, 4, 5),
                make_range(1, 7, 8),
                make_range(1, 10, 11), // extra
            ],
        );
        let sym = make_sym(
            "foo",
            SymbolKind::Function,
            vec![Param { name: "a".into(), ty: Some("int".into()) }],
        );
        assert!(hints_for(&call, &sym, MethodHintMode::Name).is_empty());
    }

    #[test]
    fn no_params_returns_empty() {
        let call = make_call(
            "foo",
            CallKind::Function,
            vec![make_range(1, 4, 5)],
        );
        let sym = Symbol {
            name: "foo".into(),
            kind: SymbolKind::Function,
            name_range: make_range(0, 0, 1),
            full_range: make_range(0, 0, 1),
            params: None, // not callable / no params info
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        assert!(hints_for(&call, &sym, MethodHintMode::Name).is_empty());
    }
}
