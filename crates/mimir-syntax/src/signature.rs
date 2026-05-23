//! Signature formatting for callable symbols.
//!
//! Given a [`Symbol`] that has a `params` list, [`signature_for`] formats
//! the function/task/method/macro signature into a string label and records
//! the byte offsets of each parameter within that label. The server crate
//! converts these into [`lsp_types::SignatureInformation`] / [`lsp_types::ParameterInformation`]
//! at the boundary.
//!
//! ## Label format
//!
//! | Kind    | Format                                           |
//! |---------|--------------------------------------------------|
//! | Function/Task | `function <type> <name>(<ty> <param>, ...)` |
//! | Method  | `function/task <type> <name>(<ty> <param>, ...)` |
//! | Macro   | `` `define <name>(<param>, ...)``               |
//!
//! For macros, parameters have no type, so they appear as bare names.

use crate::symbols::{Param, Symbol, SymbolKind};

/// Formatted signature for a callable symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    /// The full label string shown in the popup, e.g.
    /// `"function bit my_func(int a, string b)"`.
    pub label: String,
    /// One entry per formal parameter, carrying the byte offsets of its
    /// text within [`label`](Self::label) for active-parameter highlighting.
    pub params: Vec<ParamInfo>,
}

/// One parameter's position within a [`SignatureInfo::label`] string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamInfo {
    /// Inclusive start and exclusive end byte offsets of the parameter
    /// text within the containing [`SignatureInfo::label`].
    ///
    /// The server converts this to `ParameterInformation { label: LabelOffsets([start, end]) }`.
    pub label_offset: (u32, u32),
}

/// Build a `SignatureInfo` from a callable symbol.
///
/// Returns `None` when `sym.params` is `None` (the symbol is not callable —
/// e.g. a module, variable, or typedef).
#[must_use]
pub fn signature_for(sym: &Symbol) -> Option<SignatureInfo> {
    let params = sym.params.as_ref()?;

    let mut label = String::new();
    let mut param_infos = Vec::new();

    match sym.kind {
        SymbolKind::Macro => {
            // `define MACRO_NAME(a, b)
            label.push_str("`define ");
            label.push_str(&sym.name);
            if !params.is_empty() {
                label.push('(');
                append_macro_params(&mut label, &mut param_infos, params);
                label.push(')');
            }
        }
        SymbolKind::Function | SymbolKind::Method => {
            label.push_str("function ");
            label.push_str(&sym.name);
            label.push('(');
            append_typed_params(&mut label, &mut param_infos, params);
            label.push(')');
        }
        SymbolKind::Task => {
            label.push_str("task ");
            label.push_str(&sym.name);
            label.push('(');
            append_typed_params(&mut label, &mut param_infos, params);
            label.push(')');
        }
        _ => return None,
    }

    Some(SignatureInfo {
        label,
        params: param_infos,
    })
}

/// Append `ty name` parameters (for functions/tasks/methods), recording offsets.
fn append_typed_params(label: &mut String, infos: &mut Vec<ParamInfo>, params: &[Param]) {
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        let start = label.len() as u32;
        if let Some(ty) = &p.ty {
            label.push_str(ty);
            label.push(' ');
        }
        label.push_str(&p.name);
        let end = label.len() as u32;
        infos.push(ParamInfo {
            label_offset: (start, end),
        });
    }
}

/// Append bare name parameters (for macros), recording offsets.
fn append_macro_params(label: &mut String, infos: &mut Vec<ParamInfo>, params: &[Param]) {
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        let start = label.len() as u32;
        label.push_str(&p.name);
        let end = label.len() as u32;
        infos.push(ParamInfo {
            label_offset: (start, end),
        });
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::SymbolKind;
    use mimir_core::{Position, Range};

    fn dummy_range() -> Range {
        Range::new(Position::new(0, 0), Position::new(0, 1))
    }

    fn make_sym(kind: SymbolKind, name: &str, params: Vec<Param>) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            name_range: dummy_range(),
            full_range: dummy_range(),
            params: Some(params),
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        }
    }

    #[test]
    fn function_with_typed_params() {
        let sym = make_sym(
            SymbolKind::Function,
            "my_func",
            vec![
                Param { name: "a".into(), ty: Some("int".into()) },
                Param { name: "b".into(), ty: Some("string".into()) },
            ],
        );
        let info = signature_for(&sym).expect("should produce SignatureInfo");
        assert_eq!(info.label, "function my_func(int a, string b)");
        assert_eq!(info.params.len(), 2);
        // First param "int a" starts at offset of 'i' in "int a"
        let (s0, e0) = info.params[0].label_offset;
        assert_eq!(&info.label[s0 as usize..e0 as usize], "int a");
        let (s1, e1) = info.params[1].label_offset;
        assert_eq!(&info.label[s1 as usize..e1 as usize], "string b");
    }

    #[test]
    fn task_no_params() {
        let sym = make_sym(SymbolKind::Task, "my_task", vec![]);
        let info = signature_for(&sym).expect("should produce SignatureInfo");
        assert_eq!(info.label, "task my_task()");
        assert!(info.params.is_empty());
    }

    #[test]
    fn macro_with_params() {
        let sym = make_sym(
            SymbolKind::Macro,
            "MY_MACRO",
            vec![
                Param { name: "x".into(), ty: None },
                Param { name: "y".into(), ty: None },
            ],
        );
        let info = signature_for(&sym).expect("should produce SignatureInfo");
        assert_eq!(info.label, "`define MY_MACRO(x, y)");
        assert_eq!(info.params.len(), 2);
        let (s0, e0) = info.params[0].label_offset;
        assert_eq!(&info.label[s0 as usize..e0 as usize], "x");
    }

    #[test]
    fn non_callable_returns_none() {
        let sym = Symbol {
            name: "my_mod".into(),
            kind: SymbolKind::Module,
            name_range: dummy_range(),
            full_range: dummy_range(),
            params: None,
            parent_class_name: None,
            return_type: None,
            decl_type: None,
        };
        assert!(signature_for(&sym).is_none());
    }

    #[test]
    fn function_with_implicit_type_param() {
        let sym = make_sym(
            SymbolKind::Function,
            "f",
            vec![Param { name: "x".into(), ty: None }],
        );
        let info = signature_for(&sym).expect("should produce SignatureInfo");
        assert_eq!(info.label, "function f(x)");
        let (s, e) = info.params[0].label_offset;
        assert_eq!(&info.label[s as usize..e as usize], "x");
    }
}
