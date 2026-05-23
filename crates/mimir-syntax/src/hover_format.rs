//! Markdown formatter for SystemVerilog declaration signatures.
//!
//! Converts a flat SV signature string (as produced by `signature_for` or
//! the built-in method tables) into rich markdown: declaration keywords are
//! **bold**, primitive types are *italic*, and identifiers / names are
//! rendered as inline `` `code` ``.  Non-word characters (punctuation,
//! parentheses, commas, `#`) pass through unchanged.
//!
//! ## Why not a code block?
//!
//! A `systemverilog` fenced code block is great when the editor has an SV
//! TextMate grammar that syntax-highlights the block.  Rich inline markdown
//! is better when the editor only renders markdown formatting (e.g. many
//! terminal-based editors or hover popups that don't load language grammars).
//! The formatted output is also readable as plain text if the client doesn't
//! render markdown at all.

/// Declaration-context keywords that typically appear in SV function / task /
/// method signatures.  Only a curated subset — not the full LRM keyword list —
/// so that `class`, `module`, etc. (which appear in hover for those construct
/// kinds) are also covered.
const SIGNATURE_KEYWORDS: &[&str] = &[
    "function",
    "task",
    "class",
    "module",
    "package",
    "interface",
    "program",
    "typedef",
    "enum",
    "struct",
    "union",
    "virtual",
    "automatic",
    "static",
    "local",
    "protected",
    "pure",
    "extern",
    "rand",
    "randc",
    "input",
    "output",
    "inout",
    "ref",
    "const",
    "endfunction",
    "endtask",
];

/// SV primitive / built-in scalar and aggregate types.
const PRIMITIVE_TYPES: &[&str] = &[
    "void",
    "int",
    "integer",
    "bit",
    "logic",
    "reg",
    "wire",
    "byte",
    "shortint",
    "longint",
    "real",
    "realtime",
    "time",
    "string",
    "chandle",
    "event",
];

/// Format a SystemVerilog declaration signature string as rich markdown.
///
/// Each ASCII identifier word in `sig` is classified and wrapped:
/// - Declaration keywords (`function`, `task`, `input`, …) → `**word**`
/// - Primitive types (`int`, `logic`, `string`, …) → `*word*`
/// - All other identifiers and names → `` `word` ``
/// - Non-word characters pass through literally.
///
/// # Example
///
/// ```
/// use mimir_syntax::hover_format::format_sv_signature;
/// let out = format_sv_signature("function int len()");
/// assert!(out.contains("**function**"));
/// assert!(out.contains("*int*"));
/// assert!(out.contains("`len`"));
/// assert!(out.ends_with("()"));
/// ```
#[must_use]
pub fn format_sv_signature(sig: &str) -> String {
    let mut result = String::with_capacity(sig.len() * 2);
    let bytes = sig.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            // Collect one full identifier (letters, digits, underscores).
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let word = &sig[start..i];
            if SIGNATURE_KEYWORDS.contains(&word) {
                result.push_str("**");
                result.push_str(word);
                result.push_str("**");
            } else if PRIMITIVE_TYPES.contains(&word) {
                result.push('*');
                result.push_str(word);
                result.push('*');
            } else {
                result.push('`');
                result.push_str(word);
                result.push('`');
            }
        } else {
            // Non-identifier byte: spaces, punctuation, digits that don't
            // start a word, etc.  Pass through as-is.
            // SAFETY: `sig` is valid UTF-8; we push one byte at a time only
            // when it is ASCII (< 0x80), so the invariant holds.
            result.push(b as char);
            i += 1;
        }
    }

    result
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_function_signature() {
        let out = format_sv_signature("function int len()");
        assert!(out.contains("**function**"), "keyword not bolded: {out}");
        assert!(out.contains("*int*"), "type not italicized: {out}");
        assert!(out.contains("`len`"), "name not inline-coded: {out}");
        assert!(out.ends_with("()"), "trailing punctuation lost: {out}");
    }

    #[test]
    fn formats_task_with_port_list() {
        let out = format_sv_signature("task automatic run(input int a)");
        assert!(out.contains("**task**"));
        assert!(out.contains("**automatic**"));
        assert!(out.contains("**input**"));
        assert!(out.contains("*int*"));
        assert!(out.contains("`run`"));
        assert!(out.contains("`a`"));
    }

    #[test]
    fn bare_identifier_with_underscore() {
        let out = format_sv_signature("rand_mode");
        assert_eq!(out, "`rand_mode`");
    }

    #[test]
    fn primitive_type_alone() {
        assert_eq!(format_sv_signature("void"), "*void*");
        assert_eq!(format_sv_signature("string"), "*string*");
    }

    #[test]
    fn punctuation_only_unchanged() {
        assert_eq!(format_sv_signature("()"), "()");
        assert_eq!(format_sv_signature(", ;"), ", ;");
    }

    #[test]
    fn empty_string() {
        assert_eq!(format_sv_signature(""), "");
    }

    #[test]
    fn virtual_method() {
        let out = format_sv_signature("virtual function void run_phase(input uvm_phase phase)");
        assert!(out.contains("**virtual**"));
        assert!(out.contains("**function**"));
        assert!(out.contains("*void*"));
        assert!(out.contains("`run_phase`"));
        assert!(out.contains("**input**"));
        assert!(out.contains("`uvm_phase`")); // user type → inline code
        assert!(out.contains("`phase`"));
    }

    #[test]
    fn numbers_in_signature_pass_through() {
        // e.g. "function bit [7:0] get8()" — digits and brackets literal
        let out = format_sv_signature("function bit get8()");
        assert!(out.contains("**function**"));
        assert!(out.contains("*bit*"));
        // `get8` starts with a letter so collected as one word → inline code
        assert!(out.contains("`get8`"));
    }
}
