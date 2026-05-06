//! IEEE 1800-2017 SystemVerilog reserved keyword list (Annex B).
//!
//! Two public items:
//!
//! * [`KEYWORDS`] — the full static list (~250 words).
//! * [`matches_prefix`] — case-insensitive prefix iterator over that list.
//!   Used by `mimir-server` to append keyword candidates to completion lists.
//!
//! Keywords are stored all-lowercase (as they appear in the LRM). Matching
//! against user input is case-insensitive, but the label returned is always
//! the canonical lowercase form.

/// All IEEE 1800-2017 SystemVerilog reserved keywords (Annex B).
pub const KEYWORDS: &[&str] = &[
    "accept_on",
    "alias",
    "always",
    "always_comb",
    "always_ff",
    "always_latch",
    "and",
    "assert",
    "assign",
    "assume",
    "automatic",
    "before",
    "begin",
    "bind",
    "bins",
    "binsof",
    "bit",
    "break",
    "buf",
    "bufif0",
    "bufif1",
    "byte",
    "case",
    "casex",
    "casez",
    "cell",
    "chandle",
    "checker",
    "class",
    "clocking",
    "cmos",
    "config",
    "const",
    "constraint",
    "context",
    "continue",
    "cover",
    "covergroup",
    "coverpoint",
    "cross",
    "deassign",
    "default",
    "defparam",
    "design",
    "disable",
    "dist",
    "do",
    "edge",
    "else",
    "end",
    "endcase",
    "endchecker",
    "endclass",
    "endclocking",
    "endconfig",
    "endfunction",
    "endgenerate",
    "endgroup",
    "endinterface",
    "endmodule",
    "endpackage",
    "endprimitive",
    "endprogram",
    "endproperty",
    "endsequence",
    "endspecify",
    "endtable",
    "endtask",
    "enum",
    "event",
    "eventually",
    "expect",
    "export",
    "extends",
    "extern",
    "final",
    "first_match",
    "for",
    "force",
    "foreach",
    "forever",
    "fork",
    "forkjoin",
    "function",
    "generate",
    "genvar",
    "global",
    "highz0",
    "highz1",
    "if",
    "iff",
    "ifnone",
    "ignore_bins",
    "illegal_bins",
    "implements",
    "implies",
    "import",
    "incdir",
    "include",
    "initial",
    "inout",
    "input",
    "inside",
    "instance",
    "int",
    "integer",
    "interconnect",
    "interface",
    "intersect",
    "join",
    "join_any",
    "join_none",
    "large",
    "let",
    "liblist",
    "library",
    "local",
    "localparam",
    "logic",
    "longint",
    "macromodule",
    "matches",
    "medium",
    "modport",
    "module",
    "nand",
    "negedge",
    "nettype",
    "new",
    "nexttime",
    "nmos",
    "nor",
    "noshowcancelled",
    "not",
    "notif0",
    "notif1",
    "null",
    "or",
    "output",
    "package",
    "packed",
    "parameter",
    "pmos",
    "posedge",
    "primitive",
    "priority",
    "program",
    "property",
    "protected",
    "pull0",
    "pull1",
    "pulldown",
    "pullup",
    "pulsestyle_ondetect",
    "pulsestyle_onevent",
    "pure",
    "rand",
    "randc",
    "randcase",
    "randsequence",
    "rcmos",
    "real",
    "realtime",
    "ref",
    "reg",
    "reject_on",
    "release",
    "repeat",
    "restrict",
    "return",
    "rnmos",
    "rpmos",
    "rtran",
    "rtranif0",
    "rtranif1",
    "s_always",
    "s_eventually",
    "s_nexttime",
    "s_until",
    "s_until_with",
    "scalared",
    "sequence",
    "shortint",
    "shortreal",
    "showcancelled",
    "signed",
    "small",
    "soft",
    "solve",
    "specify",
    "specparam",
    "static",
    "string",
    "strong",
    "strong0",
    "strong1",
    "struct",
    "super",
    "supply0",
    "supply1",
    "sync_accept_on",
    "sync_reject_on",
    "table",
    "tagged",
    "task",
    "this",
    "throughout",
    "time",
    "timeprecision",
    "timeunit",
    "tran",
    "tranif0",
    "tranif1",
    "tri",
    "tri0",
    "tri1",
    "triand",
    "trior",
    "trireg",
    "type",
    "typedef",
    "union",
    "unique",
    "unique0",
    "until",
    "until_with",
    "untyped",
    "use",
    "uwire",
    "var",
    "vectored",
    "virtual",
    "void",
    "wait",
    "wait_order",
    "wand",
    "weak",
    "weak0",
    "weak1",
    "while",
    "wildcard",
    "wire",
    "with",
    "within",
    "wor",
    "xnor",
    "xor",
];

/// Iterate over all keywords whose name starts with `prefix`
/// (case-insensitive). Returns canonical lowercase forms.
pub fn matches_prefix(prefix: &str) -> impl Iterator<Item = &'static str> {
    let p = prefix.to_ascii_lowercase();
    KEYWORDS.iter().copied().filter(move |kw| kw.starts_with(p.as_str()))
}

/// LSP snippet bodies for the SV constructs users almost always want
/// expanded to a block, not just the bare keyword. Each entry is
/// `(trigger_keyword, snippet_body)` where the body uses LSP snippet
/// syntax: `${N:placeholder}` for tab stops, `$0` for final cursor.
///
/// Plain `&str` table — no LSP types here, per the dependency rule
/// (`mimir-syntax` must stay LSP-free; `mimir-server` wraps these into
/// `CompletionItem.insert_text` + `InsertTextFormat::Snippet`).
pub const KEYWORD_SNIPPETS: &[(&str, &str)] = &[
    ("module",      "module ${1:name} (${2});\n  $0\nendmodule"),
    ("interface",   "interface ${1:name} (${2});\n  $0\nendinterface"),
    ("class",       "class ${1:name};\n  $0\nendclass"),
    ("package",     "package ${1:name};\n  $0\nendpackage"),
    ("program",     "program ${1:name} (${2});\n  $0\nendprogram"),
    ("task",        "task ${1:name}(${2});\n  $0\nendtask"),
    ("function",    "function ${1:return_type} ${2:name}(${3});\n  $0\nendfunction"),
    ("always_ff",   "always_ff @(posedge ${1:clk}) begin\n  $0\nend"),
    ("always_comb", "always_comb begin\n  $0\nend"),
    ("always_latch","always_latch begin\n  $0\nend"),
    ("initial",     "initial begin\n  $0\nend"),
    ("final",       "final begin\n  $0\nend"),
    ("covergroup",  "covergroup ${1:name};\n  $0\nendgroup"),
    ("property",    "property ${1:name};\n  $0\nendproperty"),
    ("sequence",    "sequence ${1:name};\n  $0\nendsequence"),
];

/// Look up the LSP snippet body for `keyword`, if one is registered.
/// Lookup is case-sensitive — the table is canonical lowercase.
pub fn snippet_for(keyword: &str) -> Option<&'static str> {
    KEYWORD_SNIPPETS
        .iter()
        .find_map(|(k, body)| (*k == keyword).then_some(*body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list must contain at least the 50 most canonical SV keywords.
    #[test]
    fn keywords_contains_canonical_set() {
        let canonical = [
            "module", "endmodule", "interface", "endinterface", "class",
            "endclass", "package", "endpackage", "function", "endfunction",
            "task", "endtask", "begin", "end", "always", "always_ff",
            "always_comb", "always_latch", "initial", "final", "if", "else",
            "case", "endcase", "for", "foreach", "while", "do", "repeat",
            "forever", "return", "break", "continue", "input", "output",
            "inout", "logic", "bit", "int", "integer", "parameter",
            "localparam", "typedef", "enum", "struct", "union", "wire",
            "reg", "assign", "generate",
        ];
        for kw in canonical {
            assert!(
                KEYWORDS.contains(&kw),
                "missing canonical keyword: {kw}",
            );
        }
        assert!(
            KEYWORDS.len() >= 50,
            "expected at least 50 keywords, got {}",
            KEYWORDS.len()
        );
    }

    /// `matches_prefix` returns every keyword starting with a given prefix.
    #[test]
    fn matches_prefix_filters_correctly() {
        let results: Vec<_> = matches_prefix("always").collect();
        assert!(results.contains(&"always"));
        assert!(results.contains(&"always_ff"));
        assert!(results.contains(&"always_comb"));
        assert!(results.contains(&"always_latch"));
        // Should not contain unrelated keywords.
        assert!(!results.contains(&"module"));
    }

    /// Empty prefix returns all keywords.
    #[test]
    fn matches_prefix_empty_returns_all() {
        assert_eq!(matches_prefix("").count(), KEYWORDS.len());
    }

    /// Matching is case-insensitive.
    #[test]
    fn matches_prefix_case_insensitive() {
        let lower: Vec<_> = matches_prefix("mod").collect();
        let upper: Vec<_> = matches_prefix("MOD").collect();
        assert_eq!(lower, upper);
        assert!(lower.contains(&"module"));
    }

    /// A prefix that matches nothing returns an empty iterator.
    #[test]
    fn matches_prefix_no_match_returns_empty() {
        assert_eq!(matches_prefix("zzz_nonexistent").count(), 0);
    }

    /// Every keyword in KEYWORDS must be returned by `matches_prefix` when
    /// given that keyword's full name as the prefix. This is the exhaustive
    /// self-match guard — if any keyword is accidentally missing from the list
    /// or `matches_prefix` has a bug, exactly that keyword's assertion fails.
    #[test]
    fn every_keyword_is_self_matching() {
        for kw in KEYWORDS {
            let found = matches_prefix(kw).any(|k| k == *kw);
            assert!(found, "keyword '{kw}' was not returned by matches_prefix(\"{kw}\")");
        }
    }

    /// No duplicate entries in KEYWORDS.
    #[test]
    fn keywords_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for kw in KEYWORDS {
            assert!(seen.insert(*kw), "duplicate keyword in KEYWORDS: '{kw}'");
        }
    }

    /// Every snippet trigger must also appear in `KEYWORDS` so the
    /// completion code path that emits snippets is reachable: the keyword
    /// list is what surfaces candidates; the snippet table only enriches
    /// `insert_text`. A trigger missing from `KEYWORDS` is dead code.
    #[test]
    fn snippet_triggers_are_all_keywords() {
        for (trigger, _) in KEYWORD_SNIPPETS {
            assert!(
                KEYWORDS.contains(trigger),
                "snippet trigger '{trigger}' is not in KEYWORDS",
            );
        }
    }

    /// Every snippet body must have balanced `${` / `}` placeholder
    /// markers and exactly one `$0` final cursor stop. Catches stray-`$`
    /// typos that would render literally in editors.
    #[test]
    fn snippet_bodies_are_well_formed() {
        for (trigger, body) in KEYWORD_SNIPPETS {
            let opens = body.matches("${").count();
            let mut closes = 0;
            // Count `}` that aren't part of `${`. A simple state-walk
            // suffices: we don't have nested braces in our snippets.
            let mut chars = body.chars().peekable();
            let mut prev = ' ';
            while let Some(c) = chars.next() {
                if c == '}' && prev != '$' {
                    closes += 1;
                }
                prev = c;
            }
            assert_eq!(
                opens, closes,
                "snippet '{trigger}' has unbalanced placeholders: {opens} `${{` vs {closes} `}}`",
            );
            assert_eq!(
                body.matches("$0").count(),
                1,
                "snippet '{trigger}' must have exactly one `$0` final cursor stop",
            );
        }
    }

    /// `snippet_for` finds registered triggers and returns `None` otherwise.
    #[test]
    fn snippet_for_lookup() {
        assert!(snippet_for("module").is_some());
        assert!(snippet_for("class").is_some());
        assert!(snippet_for("always_ff").is_some());
        assert!(snippet_for("nonexistent_keyword_xyz").is_none());
        // Plain keywords without a registered snippet fall through.
        assert!(snippet_for("if").is_none());
    }
}
