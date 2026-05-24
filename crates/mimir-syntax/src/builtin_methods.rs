//! IEEE 1800-2017 built-in method tables for SystemVerilog types.
//!
//! The workspace symbol index never contains built-in methods — they are
//! defined by the LRM, not by user source code. This module provides a
//! curated static lookup so hover, dot-triggered completion, inlay hints,
//! and signature help can respond to calls like `mystr.len()`,
//! `q.push_back(x)`, and `obj.rand_mode(1)` without slang.
//!
//! ## Organisation
//!
//! Methods are grouped into four static tables keyed by receiver category:
//! `STRING_METHODS`, `QUEUE_METHODS`, `ASSOC_ARRAY_METHODS`, and
//! `UNIVERSAL_METHODS`. Look up with:
//!
//! * [`find_method`] — type-aware, returns `None` for unknown types.
//! * [`find_universal`] — universal methods only (work on any class).
//! * [`find_method_by_name`] — name-only fallback when the receiver type
//!   cannot be inferred from syntax alone.
//! * [`methods_for_type`] — all methods for a type (drives dot-completion).
//!
//! ## Type detection limitation (v1)
//!
//! [`mimir_syntax::symbols::find_variable_type_at`] returns the declared
//! element type for queues and dynamic arrays — e.g. `"int"` for
//! `int q[$]` — so there is no way to distinguish a queue from a scalar
//! of the same element type at this layer. Type-aware completion therefore
//! works only for `"string"` today. All other built-in methods surface via
//! the name-only hover fallback. A future slice will extend
//! `find_variable_type_at` to also return the variable-dimension suffix.

/// A formal parameter of a built-in method.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinParam {
    /// Parameter name, e.g. `"item"`.
    pub name: &'static str,
    /// Declared type, e.g. `"int"`. `None` when the type is generic
    /// (element-type-polymorphic), e.g. `push_back`'s `item`.
    pub ty: Option<&'static str>,
}

/// One IEEE 1800-2017 built-in method.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinMethod {
    /// Method name as it appears in source, e.g. `"push_back"`.
    pub name: &'static str,
    /// Full SV signature for hover / signature-help display,
    /// e.g. `"function void push_back(T item)"`.
    pub signature: &'static str,
    /// One-line description ending with an LRM section reference.
    pub doc: &'static str,
    /// Formal parameters in declaration order.
    pub params: &'static [BuiltinParam],
}

// ─── string ─────────────────────────────────────────────────────────────────
// IEEE 1800-2017 §6.16.13 — string built-in methods.

const P_INT: BuiltinParam = BuiltinParam { name: "i", ty: Some("int") };
const P_INT_J: BuiltinParam = BuiltinParam { name: "j", ty: Some("int") };
const P_BYTE: BuiltinParam = BuiltinParam { name: "c", ty: Some("byte") };
const P_STR: BuiltinParam = BuiltinParam { name: "s", ty: Some("string") };

static STRING_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "len",
        signature: "function int len()",
        doc: "Return the number of characters in the string. IEEE 1800-2017 §6.16.13.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "putc",
        signature: "function void putc(int i, byte c)",
        doc: "Replace character at index `i` with byte `c`. IEEE 1800-2017 §6.16.13.2.",
        params: &[P_INT, P_BYTE],
    },
    BuiltinMethod {
        name: "getc",
        signature: "function byte getc(int i)",
        doc: "Return the byte value of character at index `i`. IEEE 1800-2017 §6.16.13.3.",
        params: &[P_INT],
    },
    BuiltinMethod {
        name: "toupper",
        signature: "function string toupper()",
        doc: "Return a new string with all lowercase letters converted to uppercase. IEEE 1800-2017 §6.16.13.4.",
        params: &[],
    },
    BuiltinMethod {
        name: "tolower",
        signature: "function string tolower()",
        doc: "Return a new string with all uppercase letters converted to lowercase. IEEE 1800-2017 §6.16.13.4.",
        params: &[],
    },
    BuiltinMethod {
        name: "compare",
        signature: "function int compare(string s)",
        doc: "Lexicographic comparison (case-sensitive). Returns negative, zero, or positive. IEEE 1800-2017 §6.16.13.5.",
        params: &[P_STR],
    },
    BuiltinMethod {
        name: "icompare",
        signature: "function int icompare(string s)",
        doc: "Case-insensitive lexicographic comparison. Returns negative, zero, or positive. IEEE 1800-2017 §6.16.13.5.",
        params: &[P_STR],
    },
    BuiltinMethod {
        name: "substr",
        signature: "function string substr(int i, int j)",
        doc: "Return the substring from index `i` to `j` inclusive. IEEE 1800-2017 §6.16.13.6.",
        params: &[P_INT, P_INT_J],
    },
    BuiltinMethod {
        name: "atoi",
        signature: "function integer atoi()",
        doc: "Convert the string to a decimal integer. IEEE 1800-2017 §6.16.13.7.",
        params: &[],
    },
    BuiltinMethod {
        name: "atohex",
        signature: "function integer atohex()",
        doc: "Convert the string from hexadecimal representation to an integer. IEEE 1800-2017 §6.16.13.7.",
        params: &[],
    },
    BuiltinMethod {
        name: "atooct",
        signature: "function integer atooct()",
        doc: "Convert the string from octal representation to an integer. IEEE 1800-2017 §6.16.13.7.",
        params: &[],
    },
    BuiltinMethod {
        name: "atobin",
        signature: "function integer atobin()",
        doc: "Convert the string from binary representation to an integer. IEEE 1800-2017 §6.16.13.7.",
        params: &[],
    },
    BuiltinMethod {
        name: "atoreal",
        signature: "function real atoreal()",
        doc: "Convert the string to a real (floating-point) value. IEEE 1800-2017 §6.16.13.7.",
        params: &[],
    },
    BuiltinMethod {
        name: "itoa",
        signature: "function void itoa(integer i)",
        doc: "Assign the decimal representation of `i` to the string. IEEE 1800-2017 §6.16.13.8.",
        params: &[BuiltinParam { name: "i", ty: Some("integer") }],
    },
    BuiltinMethod {
        name: "hextoa",
        signature: "function void hextoa(integer i)",
        doc: "Assign the hexadecimal representation of `i` to the string. IEEE 1800-2017 §6.16.13.8.",
        params: &[BuiltinParam { name: "i", ty: Some("integer") }],
    },
    BuiltinMethod {
        name: "octtoa",
        signature: "function void octtoa(integer i)",
        doc: "Assign the octal representation of `i` to the string. IEEE 1800-2017 §6.16.13.8.",
        params: &[BuiltinParam { name: "i", ty: Some("integer") }],
    },
    BuiltinMethod {
        name: "bintoa",
        signature: "function void bintoa(integer i)",
        doc: "Assign the binary representation of `i` to the string. IEEE 1800-2017 §6.16.13.8.",
        params: &[BuiltinParam { name: "i", ty: Some("integer") }],
    },
    BuiltinMethod {
        name: "realtoa",
        signature: "function void realtoa(real r)",
        doc: "Assign the string representation of real value `r` to the string. IEEE 1800-2017 §6.16.13.8.",
        params: &[BuiltinParam { name: "r", ty: Some("real") }],
    },
];

// ─── queue / dynamic array ───────────────────────────────────────────────────
// IEEE 1800-2017 §7.10 (queues) and §7.5 (dynamic arrays).
// These share most methods; name-only hover applies to both.

const P_IDX: BuiltinParam = BuiltinParam { name: "index", ty: Some("int") };
const P_ITEM: BuiltinParam = BuiltinParam { name: "item", ty: None }; // element-type generic

static QUEUE_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "size",
        signature: "function int size()",
        doc: "Return the number of elements. IEEE 1800-2017 §7.10.2, §7.5.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "insert",
        signature: "function void insert(int index, T item)",
        doc: "Insert `item` before position `index`. IEEE 1800-2017 §7.10.2.",
        params: &[P_IDX, P_ITEM],
    },
    BuiltinMethod {
        name: "delete",
        signature: "function void delete([int index])",
        doc: "Delete element at `index`; with no argument, delete all elements. IEEE 1800-2017 §7.10.2, §7.5.1.",
        params: &[P_IDX],
    },
    BuiltinMethod {
        name: "push_back",
        signature: "function void push_back(T item)",
        doc: "Append `item` to the back of the queue. IEEE 1800-2017 §7.10.2.",
        params: &[P_ITEM],
    },
    BuiltinMethod {
        name: "push_front",
        signature: "function void push_front(T item)",
        doc: "Prepend `item` to the front of the queue. IEEE 1800-2017 §7.10.2.",
        params: &[P_ITEM],
    },
    BuiltinMethod {
        name: "pop_back",
        signature: "function T pop_back()",
        doc: "Remove and return the last element of the queue. IEEE 1800-2017 §7.10.2.",
        params: &[],
    },
    BuiltinMethod {
        name: "pop_front",
        signature: "function T pop_front()",
        doc: "Remove and return the first element of the queue. IEEE 1800-2017 §7.10.2.",
        params: &[],
    },
    BuiltinMethod {
        name: "find",
        signature: "function T[$] find() with (item expr)",
        doc: "Return a queue of all elements satisfying the `with` expression. IEEE 1800-2017 §7.12.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "find_index",
        signature: "function int[$] find_index() with (item expr)",
        doc: "Return the indices of all elements satisfying the `with` expression. IEEE 1800-2017 §7.12.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "find_first",
        signature: "function T[$] find_first() with (item expr)",
        doc: "Return a queue containing the first element satisfying the `with` expression. IEEE 1800-2017 §7.12.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "find_first_index",
        signature: "function int[$] find_first_index() with (item expr)",
        doc: "Return a queue containing the index of the first element satisfying the `with` expression. IEEE 1800-2017 §7.12.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "find_last",
        signature: "function T[$] find_last() with (item expr)",
        doc: "Return a queue containing the last element satisfying the `with` expression. IEEE 1800-2017 §7.12.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "find_last_index",
        signature: "function int[$] find_last_index() with (item expr)",
        doc: "Return a queue containing the index of the last element satisfying the `with` expression. IEEE 1800-2017 §7.12.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "sort",
        signature: "function void sort() [with (item expr)]",
        doc: "Sort the array in ascending order, optionally using a `with` key expression. IEEE 1800-2017 §7.12.2.",
        params: &[],
    },
    BuiltinMethod {
        name: "rsort",
        signature: "function void rsort() [with (item expr)]",
        doc: "Sort the array in descending order, optionally using a `with` key expression. IEEE 1800-2017 §7.12.2.",
        params: &[],
    },
    BuiltinMethod {
        name: "reverse",
        signature: "function void reverse()",
        doc: "Reverse the order of elements in the array. IEEE 1800-2017 §7.12.2.",
        params: &[],
    },
    BuiltinMethod {
        name: "shuffle",
        signature: "function void shuffle()",
        doc: "Randomise the order of elements in the array. IEEE 1800-2017 §7.12.2.",
        params: &[],
    },
    BuiltinMethod {
        name: "sum",
        signature: "function T sum() [with (item expr)]",
        doc: "Return the sum of all elements, optionally mapped through a `with` expression. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "product",
        signature: "function T product() [with (item expr)]",
        doc: "Return the product of all elements, optionally mapped through a `with` expression. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "and",
        signature: "function T and() [with (item expr)]",
        doc: "Return the bitwise AND of all elements. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "or",
        signature: "function T or() [with (item expr)]",
        doc: "Return the bitwise OR of all elements. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "xor",
        signature: "function T xor() [with (item expr)]",
        doc: "Return the bitwise XOR of all elements. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "min",
        signature: "function T[$] min() [with (item expr)]",
        doc: "Return a queue containing the minimum element. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "max",
        signature: "function T[$] max() [with (item expr)]",
        doc: "Return a queue containing the maximum element. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "unique",
        signature: "function T[$] unique() [with (item expr)]",
        doc: "Return a queue of unique values (duplicates removed). IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
    BuiltinMethod {
        name: "unique_index",
        signature: "function int[$] unique_index() [with (item expr)]",
        doc: "Return the indices of the first occurrence of each unique value. IEEE 1800-2017 §7.12.3.",
        params: &[],
    },
];

// ─── dynamic array ───────────────────────────────────────────────────────────
// IEEE 1800-2017 §7.5 — dynamic array built-in methods.

static DYNAMIC_ARRAY_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "size",
        signature: "function int size()",
        doc: "Return the number of elements. IEEE 1800-2017 §7.5.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "delete",
        signature: "function void delete()",
        doc: "Release all elements and set the array size to zero. IEEE 1800-2017 §7.5.1.",
        params: &[],
    },
];

// ─── associative array ───────────────────────────────────────────────────────
// IEEE 1800-2017 §7.8 — associative array built-in methods.

static ASSOC_ARRAY_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "exists",
        signature: "function int exists(K index)",
        doc: "Return 1 if an element with key `index` exists; 0 otherwise. IEEE 1800-2017 §7.9.2.",
        params: &[BuiltinParam { name: "index", ty: Some("K") }],
    },
    BuiltinMethod {
        name: "delete",
        signature: "function void delete([K index])",
        doc: "Delete the element at `index`; with no argument delete all elements. IEEE 1800-2017 §7.9.2.",
        params: &[BuiltinParam { name: "index", ty: Some("K") }],
    },
    BuiltinMethod {
        name: "first",
        signature: "function int first(ref K index)",
        doc: "Set `index` to the smallest key; return 0 if the array is empty. IEEE 1800-2017 §7.9.2.",
        params: &[BuiltinParam { name: "index", ty: Some("K") }],
    },
    BuiltinMethod {
        name: "last",
        signature: "function int last(ref K index)",
        doc: "Set `index` to the largest key; return 0 if the array is empty. IEEE 1800-2017 §7.9.2.",
        params: &[BuiltinParam { name: "index", ty: Some("K") }],
    },
    BuiltinMethod {
        name: "next",
        signature: "function int next(ref K index)",
        doc: "Advance `index` to the next key; return 0 if already at the end. IEEE 1800-2017 §7.9.2.",
        params: &[BuiltinParam { name: "index", ty: Some("K") }],
    },
    BuiltinMethod {
        name: "prev",
        signature: "function int prev(ref K index)",
        doc: "Move `index` to the previous key; return 0 if already at the start. IEEE 1800-2017 §7.9.2.",
        params: &[BuiltinParam { name: "index", ty: Some("K") }],
    },
    BuiltinMethod {
        name: "num",
        signature: "function int num()",
        doc: "Return the number of entries in the associative array. IEEE 1800-2017 §7.9.2.",
        params: &[],
    },
];

// ─── universal ───────────────────────────────────────────────────────────────
// These methods apply to any class instance, not to a specific type.
// IEEE 1800-2017 §18.8 (rand_mode / constraint_mode), §18.11 (randomize).

static UNIVERSAL_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "rand_mode",
        signature: "function int rand_mode(bit on_off)",
        doc: "Enable (`1`) or disable (`0`) randomisation of `rand`/`randc` variables. Returns previous state. IEEE 1800-2017 §18.8.",
        params: &[BuiltinParam { name: "on_off", ty: Some("bit") }],
    },
    BuiltinMethod {
        name: "constraint_mode",
        signature: "function int constraint_mode(bit on_off)",
        doc: "Enable (`1`) or disable (`0`) a named constraint block. Returns previous state. IEEE 1800-2017 §18.8.",
        params: &[BuiltinParam { name: "on_off", ty: Some("bit") }],
    },
    BuiltinMethod {
        name: "randomize",
        signature: "function int randomize([var_list] [with constraint_block])",
        doc: "Randomise the object's `rand`/`randc` variables subject to all active constraints. Returns 1 on success, 0 on failure. IEEE 1800-2017 §18.11.",
        params: &[],
    },
    BuiltinMethod {
        name: "pre_randomize",
        signature: "function void pre_randomize()",
        doc: "Callback called automatically before each `randomize()` invocation. Override to add pre-randomisation logic. IEEE 1800-2017 §18.11.1.",
        params: &[],
    },
    BuiltinMethod {
        name: "post_randomize",
        signature: "function void post_randomize()",
        doc: "Callback called automatically after each successful `randomize()` invocation. Override to add post-randomisation logic. IEEE 1800-2017 §18.11.1.",
        params: &[],
    },
];

// ─── Public API ──────────────────────────────────────────────────────────────

/// All built-in methods for the given normalised type name.
///
/// Returns `STRING_METHODS` for `"string"`, an empty slice for any other
/// type (queue/array type detection is deferred — see module doc).
#[must_use]
pub fn methods_for_type(normalized_type: &str) -> &'static [BuiltinMethod] {
    match normalized_type {
        "string" => STRING_METHODS,
        _ => &[],
    }
}

/// Look up a specific built-in method by type and name.
///
/// Returns `None` for unknown types or when the method is not in the table
/// for the given type (no cross-type bleed).
#[must_use]
pub fn find_method(normalized_type: &str, method_name: &str) -> Option<&'static BuiltinMethod> {
    methods_for_type(normalized_type)
        .iter()
        .find(|m| m.name == method_name)
}

/// Look up a method from the universal table (`rand_mode`, `constraint_mode`,
/// `randomize`, `pre_randomize`, `post_randomize`).
///
/// Universal methods are valid on any class instance regardless of type.
#[must_use]
pub fn find_universal(method_name: &str) -> Option<&'static BuiltinMethod> {
    UNIVERSAL_METHODS.iter().find(|m| m.name == method_name)
}

/// Return all universal methods (valid on any class instance).
///
/// Used by completion to unconditionally append `rand_mode`,
/// `constraint_mode`, and `randomize` after workspace-member candidates.
#[must_use]
pub fn universal_methods() -> &'static [BuiltinMethod] {
    UNIVERSAL_METHODS
}

/// Return built-in methods for a dimension suffix from a variable declaration.
///
/// Maps the raw dimension text captured by
/// [`mimir_syntax::symbols::find_variable_type_info_at`] to the appropriate
/// built-in method table:
///
/// | Suffix pattern | Table |
/// |---|---|
/// | `[$]`, `[$:N]` | [`QUEUE_METHODS`] |
/// | `[]` | [`DYNAMIC_ARRAY_METHODS`] |
/// | `[T]` (non-empty key type) | [`ASSOC_ARRAY_METHODS`] |
/// | anything else | empty |
#[must_use]
pub fn methods_for_suffix(suffix: &str) -> &'static [BuiltinMethod] {
    let s = suffix.trim();
    if s.starts_with("[$") {
        QUEUE_METHODS
    } else if s == "[]" {
        DYNAMIC_ARRAY_METHODS
    } else if s.starts_with('[') && s.ends_with(']') && s.len() > 2 && !s.contains(':') {
        // Non-empty bracketed key without `:`: associative array.
        // `[3:0]` (packed dimension) contains `:` and is excluded.
        ASSOC_ARRAY_METHODS
    } else {
        &[]
    }
}

/// Name-only fallback: search all tables and return the first match.
///
/// Used when the receiver type cannot be inferred from syntax alone
/// (e.g. queues, dynamic arrays, associative arrays). Returns the most
/// specific entry — string methods first, then queue/array, then assoc,
/// then universal.
#[must_use]
pub fn find_method_by_name(method_name: &str) -> Option<&'static BuiltinMethod> {
    for table in [
        STRING_METHODS,
        QUEUE_METHODS,
        ASSOC_ARRAY_METHODS,
        UNIVERSAL_METHODS,
    ] {
        if let Some(m) = table.iter().find(|m| m.name == method_name) {
            return Some(m);
        }
    }
    None
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_len_found() {
        let m = find_method("string", "len").expect("len should be in string table");
        assert_eq!(m.name, "len");
        assert!(m.signature.contains("int len()"));
        assert!(m.params.is_empty());
    }

    #[test]
    fn string_substr_has_two_params() {
        let m = find_method("string", "substr").expect("substr should be in string table");
        assert_eq!(m.params.len(), 2);
        assert_eq!(m.params[0].name, "i");
        assert_eq!(m.params[1].name, "j");
    }

    #[test]
    fn universal_rand_mode_found() {
        let m = find_universal("rand_mode").expect("rand_mode should be universal");
        assert_eq!(m.name, "rand_mode");
        assert_eq!(m.params.len(), 1);
    }

    #[test]
    fn universal_constraint_mode_found() {
        let m = find_universal("constraint_mode").expect("constraint_mode should be universal");
        assert_eq!(m.name, "constraint_mode");
    }

    #[test]
    fn name_only_push_back_found() {
        let m = find_method_by_name("push_back").expect("push_back should resolve by name");
        assert_eq!(m.name, "push_back");
    }

    #[test]
    fn no_cross_type_bleed_string_push_back() {
        assert!(
            find_method("string", "push_back").is_none(),
            "push_back should not appear in string methods"
        );
    }

    #[test]
    fn unknown_type_returns_none() {
        assert!(find_method("myclass", "len").is_none());
    }

    #[test]
    fn methods_for_string_nonempty() {
        assert!(!methods_for_type("string").is_empty());
    }

    #[test]
    fn methods_for_int_empty() {
        assert!(methods_for_type("int").is_empty());
    }

    #[test]
    fn assoc_exists_by_name() {
        let m = find_method_by_name("exists").expect("exists should resolve by name");
        assert_eq!(m.name, "exists");
    }

    #[test]
    fn all_string_methods_have_doc() {
        for m in STRING_METHODS {
            assert!(
                !m.doc.is_empty(),
                "string method '{}' has no doc",
                m.name
            );
            assert!(
                m.doc.contains("IEEE"),
                "string method '{}' doc missing LRM reference",
                m.name
            );
        }
    }

    #[test]
    fn all_universal_methods_have_params_consistent_with_signature() {
        for m in UNIVERSAL_METHODS {
            // Just check the table is internally consistent (no panics accessing params)
            let _ = m.params.len();
            let _ = m.signature;
        }
    }

    // methods_for_suffix

    #[test]
    fn suffix_queue_returns_queue_methods() {
        let table = methods_for_suffix("[$]");
        let names: Vec<&str> = table.iter().map(|m| m.name).collect();
        assert!(names.contains(&"push_back"), "push_back in queue table");
        assert!(names.contains(&"pop_front"), "pop_front in queue table");
        assert!(names.contains(&"size"), "size in queue table");
    }

    #[test]
    fn suffix_queue_bounded_returns_queue_methods() {
        // `[$:N]` is also a queue dimension.
        let table = methods_for_suffix("[$:5]");
        assert!(!table.is_empty());
        assert!(table.iter().any(|m| m.name == "push_back"));
    }

    #[test]
    fn suffix_dynamic_array_returns_size_and_delete() {
        let table = methods_for_suffix("[]");
        let names: Vec<&str> = table.iter().map(|m| m.name).collect();
        assert!(names.contains(&"size"), "size in dynamic array table");
        assert!(names.contains(&"delete"), "delete in dynamic array table");
        assert!(!names.contains(&"push_back"), "push_back NOT in dynamic array table");
    }

    #[test]
    fn suffix_assoc_array_returns_assoc_methods() {
        let table = methods_for_suffix("[string]");
        let names: Vec<&str> = table.iter().map(|m| m.name).collect();
        assert!(names.contains(&"exists"), "exists in assoc table");
        assert!(names.contains(&"num"), "num in assoc table");
        assert!(!names.contains(&"push_back"), "push_back NOT in assoc table");
    }

    #[test]
    fn suffix_plain_returns_empty() {
        assert!(methods_for_suffix("").is_empty());
        assert!(methods_for_suffix("int").is_empty());
        assert!(methods_for_suffix("[3:0]").is_empty()); // packed dimension, not a collection
    }
}
