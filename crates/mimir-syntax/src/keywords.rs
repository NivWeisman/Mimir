//! IEEE 1800-2017 SystemVerilog reserved keyword list (Annex B).
//!
//! Public items:
//!
//! * [`KEYWORDS`] — the full static list (~250 words).
//! * [`matches_prefix`] — case-insensitive prefix iterator over that list.
//!   Used by `mimir-server` to append keyword candidates to completion lists.
//! * [`KEYWORD_SNIPPETS`] / [`snippet_for`] — LSP snippet bodies for block
//!   constructs (`module`, `always_ff`, …).
//! * [`KEYWORD_DOCS`] / [`SYSTEM_TASK_DOCS`] / [`doc_for`] — one-line
//!   descriptions for hover help. Curated subset (not every reserved
//!   word) — entries are LRM-grounded with `§` section references.
//!
//! Keywords are stored all-lowercase (as they appear in the LRM). Matching
//! against user input is case-insensitive, but the label returned is always
//! the canonical lowercase form. System tasks (the `$…` table) match
//! case-sensitively — `$display` and `$DISPLAY` are distinct per the LRM.

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

/// One-line documentation strings for the SV reserved words that carry
/// non-obvious semantics — the ones a user is most likely to hover for
/// help on. Structural noise (`endmodule`, `endclass`, `endcase`, …) is
/// intentionally omitted: those terminate a block and have no behaviour
/// worth a popup.
///
/// Each entry is `(keyword, "one-line description. IEEE 1800-2017 §X.")`.
/// Descriptions are summaries, not LRM text — they exist to remind a
/// user what the keyword does, not to replace the spec.
pub const KEYWORD_DOCS: &[(&str, &str)] = &[
    // ── procedural blocks ────────────────────────────────────────────
    ("always",        "Procedural block; executes whenever its sensitivity list (or `@*`) triggers. IEEE 1800-2017 §9.2.2."),
    ("always_comb",   "Combinational always block with inferred sensitivity; tools flag latch inference. IEEE 1800-2017 §9.2.2.2."),
    ("always_ff",     "Edge-sensitive sequential always block; requires an explicit edge in the event control. IEEE 1800-2017 §9.2.2.4."),
    ("always_latch",  "Level-sensitive latch-inferring always block. IEEE 1800-2017 §9.2.2.3."),
    ("initial",       "Procedural block; runs once at simulation start (time 0). IEEE 1800-2017 §9.2.1."),
    ("final",         "Procedural block; runs once at the end of simulation (after `$finish`). IEEE 1800-2017 §9.2.3."),
    ("fork",          "Begin a fork-join block; statements inside run as parallel processes. IEEE 1800-2017 §9.3.2."),
    ("join",          "End a `fork` block; parent blocks until *all* spawned processes finish. IEEE 1800-2017 §9.3.2."),
    ("join_any",      "End a `fork` block; parent unblocks when *any* spawned process finishes (others keep running). IEEE 1800-2017 §9.3.2."),
    ("join_none",     "End a `fork` block; parent continues immediately, spawned processes run detached. IEEE 1800-2017 §9.3.2."),
    ("disable",       "Terminate a named block, task, or `fork` group. IEEE 1800-2017 §9.6.2."),
    ("wait",          "Block the current process until an expression evaluates true. IEEE 1800-2017 §9.4.1."),
    ("wait_order",    "Wait for events to occur in a specified order. IEEE 1800-2017 §9.4.4."),

    // ── data types ────────────────────────────────────────────────────
    ("logic",         "4-state single-driver scalar/vector type; the default for new SV code. IEEE 1800-2017 §6.3.1."),
    ("bit",           "2-state scalar/vector type (no `x` / `z`); faster simulation than `logic`. IEEE 1800-2017 §6.11.2."),
    ("reg",           "4-state procedurally-assigned net (legacy Verilog name for what `logic` replaces). IEEE 1800-2017 §6.3.1."),
    ("wire",          "4-state continuously-driven net; resolves multiple drivers per net type. IEEE 1800-2017 §6.6."),
    ("int",           "Signed 2-state 32-bit integer. IEEE 1800-2017 §6.11.1."),
    ("integer",       "Signed 4-state 32-bit integer (legacy Verilog). IEEE 1800-2017 §6.11.1."),
    ("longint",       "Signed 2-state 64-bit integer. IEEE 1800-2017 §6.11.1."),
    ("shortint",      "Signed 2-state 16-bit integer. IEEE 1800-2017 §6.11.1."),
    ("byte",          "Signed 2-state 8-bit integer. IEEE 1800-2017 §6.11.1."),
    ("real",          "Double-precision floating-point. IEEE 1800-2017 §6.12."),
    ("shortreal",     "Single-precision floating-point. IEEE 1800-2017 §6.12."),
    ("realtime",      "Floating-point time type (alias of `real` for clarity). IEEE 1800-2017 §6.12."),
    ("time",          "64-bit unsigned integer time type. IEEE 1800-2017 §6.11.1."),
    ("string",        "Dynamic-length character-string type with rich method API. IEEE 1800-2017 §6.16."),
    ("chandle",       "Opaque handle for DPI-imported foreign C objects. IEEE 1800-2017 §6.14."),
    ("event",         "Named synchronization event; triggered with `->`, awaited with `@`. IEEE 1800-2017 §6.17."),
    ("enum",          "Enumerated type; named integer values with optional explicit encodings. IEEE 1800-2017 §6.19."),
    ("struct",        "Aggregate type bundling named members; default unpacked. IEEE 1800-2017 §7.2."),
    ("union",         "Aggregate type whose members share storage; default unpacked. IEEE 1800-2017 §7.3."),
    ("packed",        "Apply to `struct`/`union`/array to lay members out as a contiguous bit vector. IEEE 1800-2017 §7.2.1."),
    ("typedef",       "Create a named alias for a type expression. IEEE 1800-2017 §6.18."),
    ("type",          "In `typedef`/parameter context: introduce a type parameter. IEEE 1800-2017 §6.20.3."),
    ("void",          "No-value return type for tasks/functions used as statements. IEEE 1800-2017 §6.13."),

    // ── modifiers / qualifiers ───────────────────────────────────────
    ("signed",        "Force a vector to be treated as two's-complement signed. IEEE 1800-2017 §6.20.2."),
    ("automatic",     "Allocate task/function locals per-call (re-entrant); required for recursion. IEEE 1800-2017 §13.4.2."),
    ("static",        "Allocate task/function locals once at elaboration; the legacy default for non-class scopes. IEEE 1800-2017 §13.4.2."),
    ("const",         "Read-only after initialization. IEEE 1800-2017 §6.24."),
    ("local",         "Class-member visibility: accessible only inside the declaring class. IEEE 1800-2017 §8.18."),
    ("protected",     "Class-member visibility: accessible inside the class and its subclasses. IEEE 1800-2017 §8.18."),
    ("virtual",       "On methods: enable dynamic dispatch / override. On classes: forbid direct instantiation. On interfaces: introduce a virtual-interface handle. IEEE 1800-2017 §8.20, §25.9."),
    ("pure",          "Combined with `virtual`: an abstract method with no implementation; subclasses must override. IEEE 1800-2017 §8.21."),
    ("rand",          "Class member is a solver-controlled random variable. IEEE 1800-2017 §18.4."),
    ("randc",         "Class member is a *cyclic* random variable (every value before any repeat). IEEE 1800-2017 §18.4."),
    ("ref",           "Pass a task/function argument by reference. IEEE 1800-2017 §13.5.2."),
    ("extern",        "Declare a class method's prototype here; define it out-of-class. IEEE 1800-2017 §8.24."),
    ("export",        "Make an imported package symbol re-exportable, or export a DPI function. IEEE 1800-2017 §26.3, §35.5."),
    ("import",        "Bring package symbols into the current scope. IEEE 1800-2017 §26.3."),

    // ── classes / OO ─────────────────────────────────────────────────
    ("class",         "Declare a class (parameterizable OOP type with methods, inheritance, polymorphism). IEEE 1800-2017 §8."),
    ("extends",       "Declare a class's base class. IEEE 1800-2017 §8.13."),
    ("implements",    "Declare interface-class conformance (since IEEE 1800-2012). IEEE 1800-2017 §8.26."),
    ("super",         "Reference the enclosing class's base-class scope. IEEE 1800-2017 §8.15."),
    ("this",          "Reference the current class instance. IEEE 1800-2017 §8.11."),
    ("new",           "Class constructor / dynamic-array allocator. IEEE 1800-2017 §8.7."),
    ("null",          "Class-handle equivalent of zero / no-instance. IEEE 1800-2017 §8.4."),

    // ── design units / instantiation ─────────────────────────────────
    ("module",        "Declare a hardware module (the primary design unit). IEEE 1800-2017 §23.2."),
    ("interface",     "Declare an interface (bundle of signals + modports + methods). IEEE 1800-2017 §25."),
    ("modport",       "Inside an `interface`: name a directional view of its signals for one port. IEEE 1800-2017 §25.5."),
    ("package",       "Declare a package (namespace for shared types, functions, parameters). IEEE 1800-2017 §26.2."),
    ("program",       "Declare a program block (testbench scheduling region, no re-entrant `always`). IEEE 1800-2017 §24."),
    ("generate",      "Begin a generate region: structural code conditionally elaborated via `if`/`case`/`for`. IEEE 1800-2017 §27."),
    ("genvar",        "Elaboration-time loop variable for `generate for`. IEEE 1800-2017 §27.4."),
    ("parameter",     "Compile-time constant; overridable at instantiation. IEEE 1800-2017 §6.20.1."),
    ("localparam",    "Compile-time constant that *cannot* be overridden from outside. IEEE 1800-2017 §6.20.4."),
    ("bind",          "Attach a module/program/interface instance into another scope without editing the target. IEEE 1800-2017 §23.11."),

    // ── control flow ─────────────────────────────────────────────────
    ("if",            "Conditional statement. IEEE 1800-2017 §12.4."),
    ("else",          "Alternative branch of an `if`. IEEE 1800-2017 §12.4."),
    ("case",          "Multi-way branch on an expression; `casex`/`casez` add wildcard semantics. IEEE 1800-2017 §12.5."),
    ("casex",         "Like `case`, but `x` and `z` in either side are don't-cares. IEEE 1800-2017 §12.5.1."),
    ("casez",         "Like `case`, but `z` (and `?`) in either side are don't-cares. IEEE 1800-2017 §12.5.1."),
    ("unique",        "On `if`/`case`: assert exactly one branch matches; warn otherwise. IEEE 1800-2017 §12.4.2, §12.5.3."),
    ("unique0",       "On `if`/`case`: assert at most one branch matches. IEEE 1800-2017 §12.4.2, §12.5.3."),
    ("priority",      "On `if`/`case`: assert at least one branch matches; ordered priority semantics. IEEE 1800-2017 §12.4.2, §12.5.3."),
    ("for",           "C-style counted loop. IEEE 1800-2017 §12.7.1."),
    ("foreach",       "Iterate over the indices of an array dimension. IEEE 1800-2017 §12.7.3."),
    ("while",         "Pre-test loop. IEEE 1800-2017 §12.7.2."),
    ("do",            "Begin a `do … while` post-test loop. IEEE 1800-2017 §12.7.2."),
    ("forever",       "Unbounded loop; equivalent to `while (1)`. IEEE 1800-2017 §12.7.4."),
    ("repeat",        "Loop a counted number of iterations. IEEE 1800-2017 §12.7.4."),
    ("break",         "Exit the innermost enclosing loop. IEEE 1800-2017 §12.8."),
    ("continue",      "Skip to the next iteration of the innermost loop. IEEE 1800-2017 §12.8."),
    ("return",        "Return from a task/function; in a function may carry a value. IEEE 1800-2017 §13.4.1."),

    // ── assertions / SVA ─────────────────────────────────────────────
    ("assert",        "Assertion statement; immediate or concurrent depending on context. IEEE 1800-2017 §16.3, §16.5."),
    ("assume",        "Assumption directive; formal tools treat as a constraint. IEEE 1800-2017 §16.5."),
    ("cover",         "Coverage directive; tools count when the property holds. IEEE 1800-2017 §16.5."),
    ("restrict",      "Formal-only constraint that need not be checked in simulation. IEEE 1800-2017 §16.7."),
    ("expect",        "Procedural blocking assertion; suspends the process until satisfied. IEEE 1800-2017 §16.17."),
    ("property",      "Declare a temporal property for use in concurrent assertions. IEEE 1800-2017 §16.12."),
    ("sequence",      "Declare a named sequence of boolean expressions over time. IEEE 1800-2017 §16.8."),
    ("throughout",    "SVA sequence operator: a condition must hold across every cycle of a sub-sequence. IEEE 1800-2017 §16.9.9."),
    ("within",        "SVA sequence operator: sub-sequence A must occur entirely within sub-sequence B. IEEE 1800-2017 §16.9.10."),
    ("first_match",   "SVA sequence operator: succeed on the earliest match, discarding later alternatives. IEEE 1800-2017 §16.9.5."),
    ("intersect",     "SVA sequence operator: both operands must match over the same time interval. IEEE 1800-2017 §16.9.7."),
    ("matches",       "Pattern-match operator in `case … matches`. IEEE 1800-2017 §12.6."),
    ("iff",           "Guard for SVA sequences and event controls; restrict when the construct is active. IEEE 1800-2017 §16.7."),

    // ── constraints ──────────────────────────────────────────────────
    ("constraint",    "Declare a class constraint block for the randomizer. IEEE 1800-2017 §18.5."),
    ("solve",         "Constraint solve-order directive (`solve A before B`). IEEE 1800-2017 §18.5.9."),
    ("with",          "Trailing constraint block on `randomize`/`new`, or array-method query. IEEE 1800-2017 §18.7, §7.12."),
    ("inside",        "Set-membership operator; common in constraints. IEEE 1800-2017 §11.4.13."),
    ("dist",          "Constraint distribution operator (`:=` / `:/` weights). IEEE 1800-2017 §18.5.4."),
    ("soft",          "Soft constraint; relaxed when the constraint solver finds it infeasible. IEEE 1800-2017 §18.5.13."),

    // ── coverage ─────────────────────────────────────────────────────
    ("covergroup",    "Functional-coverage container; samples one or more coverpoints. IEEE 1800-2017 §19.3."),
    ("coverpoint",    "Sampling target inside a covergroup; defines bins over an expression. IEEE 1800-2017 §19.5."),
    ("cross",         "Cross-coverage between two or more coverpoints. IEEE 1800-2017 §19.6."),
    ("bins",          "Named coverage bin(s) on a coverpoint or cross. IEEE 1800-2017 §19.5.1."),
    ("ignore_bins",   "Bins excluded from coverage. IEEE 1800-2017 §19.5.1."),
    ("illegal_bins",  "Bins whose sampling raises a runtime error. IEEE 1800-2017 §19.5.1."),
    ("binsof",        "Cross-bin selector: pick rows by the originating coverpoint. IEEE 1800-2017 §19.6.1."),

    // ── nets / drive strength (less common but visible) ──────────────
    ("assign",        "Continuous assignment to a net. IEEE 1800-2017 §10.3."),
    ("force",         "Procedurally override a net/variable value, ignoring other drivers. IEEE 1800-2017 §10.6.2."),
    ("release",       "Cancel a prior `force`. IEEE 1800-2017 §10.6.2."),
    ("deassign",      "Cancel a prior procedural-continuous assignment (legacy). IEEE 1800-2017 §10.6.1."),

    // ── event control / timing ───────────────────────────────────────
    ("posedge",       "Event control: trigger on a 0/x/z → 1 transition. IEEE 1800-2017 §9.4.2."),
    ("negedge",       "Event control: trigger on a 1/x/z → 0 transition. IEEE 1800-2017 §9.4.2."),
    ("edge",          "Event control: trigger on any signal change. IEEE 1800-2017 §9.4.2."),
    ("clocking",      "Declare a clocking block (synchronous testbench/DUT interface). IEEE 1800-2017 §14."),

    // ── DPI / misc that beginners hit ────────────────────────────────
    ("nettype",       "Declare a user-defined net type (SV 2012). IEEE 1800-2017 §6.6.7."),
    ("checker",       "Declare a checker (encapsulated assertion module). IEEE 1800-2017 §17."),
    ("randcase",      "Procedural random case: pick a branch by weight. IEEE 1800-2017 §18.16."),
    ("randsequence",  "Procedural random production-rule generator. IEEE 1800-2017 §18.17."),
    ("tagged",        "Tagged-union constructor / pattern in `case matches`. IEEE 1800-2017 §7.3.2."),
];

/// One-line documentation strings for the SystemVerilog system tasks and
/// functions a verification engineer hits most often. Curated subset —
/// the LRM defines well over a hundred; this table covers I/O, control,
/// randomization, queries, bit-vector utilities, and the common
/// simulation-control entry points.
///
/// Keys include the leading `$`. Lookup via [`doc_for`] is
/// case-sensitive — `$DISPLAY` (uppercase) is *not* the same as
/// `$display` per IEEE 1800-2017.
pub const SYSTEM_TASK_DOCS: &[(&str, &str)] = &[
    // ── display / write ──────────────────────────────────────────────
    ("$display",      "Print arguments followed by a newline to simulator stdout. IEEE 1800-2017 §21.2.1."),
    ("$displayb",     "Like `$display` with default-binary radix. IEEE 1800-2017 §21.2.1."),
    ("$displayh",     "Like `$display` with default-hex radix. IEEE 1800-2017 §21.2.1."),
    ("$displayo",     "Like `$display` with default-octal radix. IEEE 1800-2017 §21.2.1."),
    ("$write",        "Like `$display` but no trailing newline. IEEE 1800-2017 §21.2.1."),
    ("$writeb",       "`$write` with default-binary radix. IEEE 1800-2017 §21.2.1."),
    ("$writeh",       "`$write` with default-hex radix. IEEE 1800-2017 §21.2.1."),
    ("$writeo",       "`$write` with default-octal radix. IEEE 1800-2017 §21.2.1."),
    ("$strobe",       "Like `$display` but defers printing to the end of the current time step. IEEE 1800-2017 §21.2.2."),
    ("$monitor",      "Print whenever any argument changes (only one active at a time). IEEE 1800-2017 §21.2.3."),
    ("$monitoroff",   "Disable the currently-active `$monitor`. IEEE 1800-2017 §21.2.3."),
    ("$monitoron",    "Re-enable the most recent `$monitor`. IEEE 1800-2017 §21.2.3."),
    ("$sformat",      "Format args into a string (writes the first arg). IEEE 1800-2017 §21.3.3."),
    ("$sformatf",     "Format args and return a string. IEEE 1800-2017 §21.3.3."),
    ("$swrite",       "Format args into a string by concatenation. IEEE 1800-2017 §21.3.3."),
    ("$psprintf",     "Vendor extension: format args and return a string (use `$sformatf` in portable code)."),

    // ── severity / control ───────────────────────────────────────────
    ("$info",         "Emit an info-level message via the assertion-message API. IEEE 1800-2017 §20.10."),
    ("$warning",      "Emit a warning-level message via the assertion-message API. IEEE 1800-2017 §20.10."),
    ("$error",        "Emit an error-level message; simulation continues. IEEE 1800-2017 §20.10."),
    ("$fatal",        "Emit a fatal-level message and terminate simulation. IEEE 1800-2017 §20.10."),
    ("$finish",       "End simulation cleanly. Optional verbosity arg controls diagnostics printed. IEEE 1800-2017 §20.2."),
    ("$stop",         "Pause simulation (drop to interactive prompt). IEEE 1800-2017 §20.2."),
    ("$exit",         "Wait for all programs to finish and then exit simulation. IEEE 1800-2017 §20.5."),

    // ── time ─────────────────────────────────────────────────────────
    ("$time",         "Return current simulation time as a 64-bit integer in the current time unit. IEEE 1800-2017 §20.3."),
    ("$stime",        "Return current simulation time as a 32-bit integer. IEEE 1800-2017 §20.3."),
    ("$realtime",     "Return current simulation time as a `real`. IEEE 1800-2017 §20.3."),
    ("$timeformat",   "Set the format used by `%t` in `$display`-family calls. IEEE 1800-2017 §21.4.2."),
    ("$printtimescale","Print the timescale of the calling scope (or argument scope). IEEE 1800-2017 §21.4.1."),

    // ── type / object conversion ─────────────────────────────────────
    ("$cast",         "Dynamic-cast: assign one class handle to another with runtime type check. IEEE 1800-2017 §8.16."),
    ("$bits",         "Return the bit-width of a type or expression. IEEE 1800-2017 §20.6.2."),
    ("$typename",     "Return a string naming the (resolved) type of an expression. IEEE 1800-2017 §20.6.1."),
    ("$isunknown",    "Return 1 if any bit of the expression is `x` or `z`. IEEE 1800-2017 §20.9."),

    // ── randomization ────────────────────────────────────────────────
    ("$urandom",      "Return a 32-bit unsigned pseudo-random value (PRNG per process). IEEE 1800-2017 §18.13.1."),
    ("$urandom_range","Return a uniformly-distributed unsigned integer in `[min, max]`. IEEE 1800-2017 §18.13.2."),
    ("$random",       "Return a 32-bit signed pseudo-random value (legacy Verilog generator). IEEE 1800-2017 §20.15.1."),
    ("$dist_uniform", "Return a uniformly-distributed pseudo-random integer. IEEE 1800-2017 §20.15.2."),
    ("$dist_normal",  "Return a normally-distributed pseudo-random integer. IEEE 1800-2017 §20.15.2."),
    ("$dist_exponential","Return an exponentially-distributed pseudo-random integer. IEEE 1800-2017 §20.15.2."),
    ("$dist_poisson", "Return a Poisson-distributed pseudo-random integer. IEEE 1800-2017 §20.15.2."),
    ("$srandom",      "Seed the per-process PRNG. IEEE 1800-2017 §18.14.3."),

    // ── arithmetic / bit-vector utility ──────────────────────────────
    ("$countones",    "Count the number of 1 bits in a vector. IEEE 1800-2017 §20.9."),
    ("$onehot",       "Return 1 iff exactly one bit of the operand is 1. IEEE 1800-2017 §20.9."),
    ("$onehot0",      "Return 1 iff at most one bit of the operand is 1. IEEE 1800-2017 §20.9."),
    ("$clog2",        "Return ceil(log2(arg)) — useful for sizing address/index widths. IEEE 1800-2017 §20.8.1."),
    ("$ln",           "Natural logarithm. IEEE 1800-2017 §20.8.2."),
    ("$log10",        "Base-10 logarithm. IEEE 1800-2017 §20.8.2."),
    ("$pow",          "`$pow(x, y)` = x raised to y. IEEE 1800-2017 §20.8.2."),
    ("$sqrt",         "Square root. IEEE 1800-2017 §20.8.2."),
    ("$floor",        "Floor of a real argument. IEEE 1800-2017 §20.8.2."),
    ("$ceil",         "Ceiling of a real argument. IEEE 1800-2017 §20.8.2."),
    ("$signed",       "Reinterpret a vector as signed. IEEE 1800-2017 §20.6."),
    ("$unsigned",     "Reinterpret a vector as unsigned. IEEE 1800-2017 §20.6."),

    // ── array querying ───────────────────────────────────────────────
    ("$size",         "Return the number of elements along a (possibly nested) array dimension. IEEE 1800-2017 §20.7."),
    ("$dimensions",   "Return the number of array dimensions of an expression. IEEE 1800-2017 §20.7."),
    ("$left",         "Return the left bound of an array dimension. IEEE 1800-2017 §20.7."),
    ("$right",        "Return the right bound of an array dimension. IEEE 1800-2017 §20.7."),
    ("$low",          "Return the lower bound of an array dimension. IEEE 1800-2017 §20.7."),
    ("$high",         "Return the upper bound of an array dimension. IEEE 1800-2017 §20.7."),
    ("$increment",    "Return +1 if dimension is ascending, -1 if descending. IEEE 1800-2017 §20.7."),

    // ── assertion control ────────────────────────────────────────────
    ("$asserton",     "Enable assertions in the named scope. IEEE 1800-2017 §20.12."),
    ("$assertoff",    "Disable assertions in the named scope (still elaborated, results suppressed). IEEE 1800-2017 §20.12."),
    ("$assertkill",   "Disable and discard in-flight assertions in the named scope. IEEE 1800-2017 §20.12."),
    ("$rose",         "SVA sampled-value function: 1 if the operand rose this cycle. IEEE 1800-2017 §16.9.3."),
    ("$fell",         "SVA sampled-value function: 1 if the operand fell this cycle. IEEE 1800-2017 §16.9.3."),
    ("$stable",       "SVA sampled-value function: 1 if the operand is unchanged this cycle. IEEE 1800-2017 §16.9.3."),
    ("$past",         "SVA sampled-value function: value of the operand N cycles ago (default 1). IEEE 1800-2017 §16.9.3."),
    ("$changed",      "SVA sampled-value function: 1 if the operand changed this cycle. IEEE 1800-2017 §16.9.3."),
    ("$sampled",      "Return the sampled (pre-NBA) value of an expression. IEEE 1800-2017 §16.9.2."),

    // ── file I/O ─────────────────────────────────────────────────────
    ("$fopen",        "Open a file; return a file descriptor (or multichannel mask). IEEE 1800-2017 §21.3.1."),
    ("$fclose",       "Close a file descriptor. IEEE 1800-2017 §21.3.1."),
    ("$fdisplay",     "Like `$display`, but to a file descriptor. IEEE 1800-2017 §21.3.1."),
    ("$fwrite",       "Like `$write`, but to a file descriptor. IEEE 1800-2017 §21.3.1."),
    ("$fscanf",       "Read formatted input from a file. IEEE 1800-2017 §21.3.4."),
    ("$sscanf",       "Read formatted input from a string. IEEE 1800-2017 §21.3.4."),
    ("$fgets",        "Read a line from a file. IEEE 1800-2017 §21.3.4."),
    ("$readmemb",     "Initialize a memory from a binary-radix text file. IEEE 1800-2017 §21.4."),
    ("$readmemh",     "Initialize a memory from a hex-radix text file. IEEE 1800-2017 §21.4."),
    ("$writememb",    "Write a memory contents to a binary-radix text file. IEEE 1800-2017 §21.4."),
    ("$writememh",    "Write a memory contents to a hex-radix text file. IEEE 1800-2017 §21.4."),

    // ── command-line / plusargs ──────────────────────────────────────
    ("$test$plusargs","Return non-zero if the simulator was invoked with `+key…`. IEEE 1800-2017 §21.6."),
    ("$value$plusargs","Parse a `+key=value` plusarg into a variable. IEEE 1800-2017 §21.6."),
];

/// Look up the static documentation string for a SystemVerilog keyword
/// or system task. Returns `None` for names not in either table.
///
/// Routing rule: a leading `$` selects [`SYSTEM_TASK_DOCS`]
/// (case-sensitive lookup, per LRM); anything else is treated as a
/// reserved keyword and matched case-insensitively against
/// [`KEYWORD_DOCS`].
#[must_use]
pub fn doc_for(name: &str) -> Option<&'static str> {
    if name.starts_with('$') {
        SYSTEM_TASK_DOCS
            .iter()
            .find_map(|(k, v)| (*k == name).then_some(*v))
    } else {
        let lower = name.to_ascii_lowercase();
        KEYWORD_DOCS
            .iter()
            .find_map(|(k, v)| (*k == lower.as_str()).then_some(*v))
    }
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
            let mut prev = ' ';
            for c in body.chars() {
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

    /// Every documented keyword must also appear in `KEYWORDS` — otherwise
    /// either the docs table has a typo or `KEYWORDS` is missing a word.
    #[test]
    fn keyword_docs_keys_are_all_keywords() {
        for (kw, _) in KEYWORD_DOCS {
            assert!(
                KEYWORDS.contains(kw),
                "KEYWORD_DOCS entry '{kw}' is not in KEYWORDS",
            );
        }
    }

    /// `KEYWORD_DOCS` must not have duplicate keys: a second entry would
    /// be unreachable since `doc_for` returns the first match.
    #[test]
    fn keyword_docs_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (kw, _) in KEYWORD_DOCS {
            assert!(seen.insert(*kw), "duplicate KEYWORD_DOCS entry: '{kw}'");
        }
    }

    /// Every doc value should end with a period — keeps the hover popup
    /// looking like sentences, not fragments.
    #[test]
    fn keyword_docs_values_are_well_formed() {
        for (kw, doc) in KEYWORD_DOCS {
            assert!(
                doc.trim_end().ends_with('.'),
                "KEYWORD_DOCS['{kw}'] does not end with '.': '{doc}'",
            );
            assert!(!doc.is_empty(), "KEYWORD_DOCS['{kw}'] is empty");
        }
    }

    /// Every system-task key starts with `$` (LRM-defined system tasks
    /// have this prefix; without it `doc_for`'s router won't find them).
    #[test]
    fn system_task_keys_start_with_dollar() {
        for (name, _) in SYSTEM_TASK_DOCS {
            assert!(
                name.starts_with('$'),
                "SYSTEM_TASK_DOCS key '{name}' does not start with '$'",
            );
            // Reject embedded whitespace — defends against accidental
            // `"$display "` with trailing space, etc.
            assert!(
                !name.chars().any(char::is_whitespace),
                "SYSTEM_TASK_DOCS key '{name}' contains whitespace",
            );
        }
    }

    /// `SYSTEM_TASK_DOCS` must not have duplicate keys.
    #[test]
    fn system_task_docs_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in SYSTEM_TASK_DOCS {
            assert!(seen.insert(*name), "duplicate SYSTEM_TASK_DOCS entry: '{name}'");
        }
    }

    /// `doc_for` is case-insensitive for keywords (mirrors the rest of
    /// the keyword API in this module).
    #[test]
    fn doc_for_keyword_is_case_insensitive() {
        let lower = doc_for("always_ff").expect("always_ff has docs");
        let upper = doc_for("ALWAYS_FF").expect("ALWAYS_FF resolves case-insensitively");
        let mixed = doc_for("Always_Ff").expect("Always_Ff resolves case-insensitively");
        assert_eq!(lower, upper);
        assert_eq!(lower, mixed);
    }

    /// `doc_for` is case-sensitive for system tasks — the LRM treats
    /// `$display` and `$DISPLAY` as distinct, and most simulators only
    /// accept the lowercase form.
    #[test]
    fn doc_for_system_task_is_case_sensitive() {
        assert!(doc_for("$display").is_some());
        assert!(
            doc_for("$DISPLAY").is_none(),
            "system task lookup should be case-sensitive",
        );
    }

    /// Names not in either table return `None`.
    #[test]
    fn doc_for_unknown_returns_none() {
        assert!(doc_for("not_a_keyword_xyz").is_none());
        assert!(doc_for("$not_a_system_task_xyz").is_none());
        // Empty input and bare `$` are both safe to look up.
        assert!(doc_for("").is_none());
        assert!(doc_for("$").is_none());
    }

    /// Spot-check that the key entries the hover handler is expected to
    /// surface are present. If any of these are removed the README claim
    /// ("keyword/system-task hover help") becomes inaccurate.
    #[test]
    fn doc_for_covers_advertised_set() {
        for kw in ["always_ff", "always_comb", "assert", "covergroup", "constraint"] {
            assert!(doc_for(kw).is_some(), "missing keyword doc: {kw}");
        }
        for st in ["$display", "$fatal", "$cast", "$urandom", "$value$plusargs"] {
            assert!(doc_for(st).is_some(), "missing system-task doc: {st}");
        }
    }
}
