//! Backend-agnostic diagnostic conversion helpers.
//!
//! Every diagnostic source (tree-sitter, slang sidecar, future backends)
//! adapts its native representation to [`MimirDiag`] in its own adapter
//! module. This module provides the single [`MimirDiag`] → LSP conversion
//! so the mapping logic lives in exactly one place.
//!
//! # Conversion flow
//!
//! ```text
//! mimir_syntax::Diagnostic ──► syntax_diag_to_mimir() ──┐
//!                                                         ├──► mimir_diag_to_lsp() ──► publishDiagnostics
//! mimir_slang::Diagnostic  ──► slang_adapter (path)  ──┘
//! ```

use mimir_ast::{DiagSeverity, MimirDiag, MimirPos, MimirRange};
use mimir_syntax::Diagnostic as SyntaxDiag;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range,
};

// --------------------------------------------------------------------------
// MimirDiag → LSP
// --------------------------------------------------------------------------

/// Convert a [`MimirDiag`] to its LSP wire form.
///
/// `source` is always `"mimir"` — editors don't need two filter labels for
/// the tree-sitter and slang paths. `code` carries the stable diagnostic
/// code (e.g. `"UnknownModule"`, `"syntax"`) so editors can group or
/// suppress per code.
pub(crate) fn mimir_diag_to_lsp(d: &MimirDiag) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line:      d.range.start.line,
                character: d.range.start.character,
            },
            end: Position {
                line:      d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: Some(match d.severity {
            DiagSeverity::Error       => DiagnosticSeverity::ERROR,
            DiagSeverity::Warning     => DiagnosticSeverity::WARNING,
            DiagSeverity::Information => DiagnosticSeverity::INFORMATION,
            DiagSeverity::Hint        => DiagnosticSeverity::HINT,
        }),
        code:    Some(NumberOrString::String(d.code.clone())),
        source:  Some("mimir".to_string()),
        message: d.message.clone(),
        related_information: None,
        tags:             None,
        code_description: None,
        data:             None,
    }
}

// --------------------------------------------------------------------------
// tree-sitter → MimirDiag
// --------------------------------------------------------------------------

/// Convert a tree-sitter [`SyntaxDiag`] to [`MimirDiag`].
///
/// Both types use `(line, UTF-16 character)` coordinates, so the range
/// conversion is a field-by-field copy. `code` is widened from `&'static str`
/// to `String`; all other fields map directly.
pub(crate) fn syntax_diag_to_mimir(d: &SyntaxDiag) -> MimirDiag {
    use mimir_syntax::DiagnosticSeverity as SS;
    MimirDiag {
        range: MimirRange {
            start: MimirPos {
                line:      d.range.start.line,
                character: d.range.start.character,
            },
            end: MimirPos {
                line:      d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: match d.severity {
            SS::Error       => DiagSeverity::Error,
            SS::Warning     => DiagSeverity::Warning,
            SS::Information => DiagSeverity::Information,
            SS::Hint        => DiagSeverity::Hint,
        },
        code:    d.code.to_string(),
        message: d.message.clone(),
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::logging::init_for_tests;

    fn make_mimir_diag(sev: DiagSeverity, code: &str) -> MimirDiag {
        MimirDiag {
            range: MimirRange {
                start: MimirPos { line: 3, character: 5 },
                end:   MimirPos { line: 3, character: 9 },
            },
            severity: sev,
            code:    code.to_string(),
            message: "test message".to_string(),
        }
    }

    fn make_syntax_diag(sev: mimir_syntax::DiagnosticSeverity) -> SyntaxDiag {
        use mimir_core::{Position, Range};
        SyntaxDiag {
            range:    Range::new(Position::new(1, 0), Position::new(1, 4)),
            message:  "syntax error near `@@`".to_string(),
            severity: sev,
            code:     "syntax",
        }
    }

    // --- mimir_diag_to_lsp ---

    #[test]
    fn mimir_diag_to_lsp_preserves_fields() {
        init_for_tests();
        let d = make_mimir_diag(DiagSeverity::Error, "UnknownModule");
        let lsp = mimir_diag_to_lsp(&d);
        assert_eq!(lsp.range.start.line, 3);
        assert_eq!(lsp.range.start.character, 5);
        assert_eq!(lsp.range.end.character, 9);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("mimir"));
        assert_eq!(lsp.code, Some(NumberOrString::String("UnknownModule".into())));
        assert_eq!(lsp.message, "test message");
    }

    #[test]
    fn mimir_diag_all_severities_map() {
        init_for_tests();
        let cases = [
            (DiagSeverity::Error,       DiagnosticSeverity::ERROR),
            (DiagSeverity::Warning,     DiagnosticSeverity::WARNING),
            (DiagSeverity::Information, DiagnosticSeverity::INFORMATION),
            (DiagSeverity::Hint,        DiagnosticSeverity::HINT),
        ];
        for (mimir_sev, expected) in cases {
            let lsp = mimir_diag_to_lsp(&make_mimir_diag(mimir_sev, "X"));
            assert_eq!(lsp.severity, Some(expected));
        }
    }

    // --- syntax_diag_to_mimir ---

    #[test]
    fn syntax_diag_to_mimir_preserves_fields() {
        init_for_tests();
        use mimir_syntax::DiagnosticSeverity as SS;
        let s = make_syntax_diag(SS::Error);
        let m = syntax_diag_to_mimir(&s);
        assert_eq!(m.range.start.line, 1);
        assert_eq!(m.range.start.character, 0);
        assert_eq!(m.range.end.character, 4);
        assert_eq!(m.severity, DiagSeverity::Error);
        assert_eq!(m.code, "syntax");
        assert_eq!(m.message, s.message);
    }

    #[test]
    fn syntax_diag_all_severities_map() {
        init_for_tests();
        use mimir_syntax::DiagnosticSeverity as SS;
        let cases = [
            (SS::Error,       DiagSeverity::Error),
            (SS::Warning,     DiagSeverity::Warning),
            (SS::Information, DiagSeverity::Information),
            (SS::Hint,        DiagSeverity::Hint),
        ];
        for (ss, expected) in cases {
            let m = syntax_diag_to_mimir(&make_syntax_diag(ss));
            assert_eq!(m.severity, expected);
        }
    }

    /// Round-trip: syntax → MimirDiag → LSP should give the same result
    /// as the old direct syntax → LSP path.
    #[test]
    fn round_trip_syntax_to_lsp_via_mimir() {
        init_for_tests();
        use mimir_syntax::DiagnosticSeverity as SS;
        let s = make_syntax_diag(SS::Warning);
        let via_mimir = mimir_diag_to_lsp(&syntax_diag_to_mimir(&s));
        assert_eq!(via_mimir.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(via_mimir.source.as_deref(), Some("mimir"));
        assert_eq!(via_mimir.code, Some(NumberOrString::String("syntax".into())));
        assert_eq!(via_mimir.message, s.message);
    }
}
