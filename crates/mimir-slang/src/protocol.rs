//! Wire protocol between [`crate::Client`] and the slang sidecar.
//!
//! NDJSON over stdio: each request and each response is one JSON object on
//! one line. The shape is JSON-RPC-flavoured (we use `id`, `method`,
//! `params`, `result`, `error`) but we drop the `jsonrpc` discriminator —
//! both ends are ours, so the version negotiation is moot.
//!
//! ## Why our own types and not `lsp_types`?
//!
//! `lsp_types` is the LSP wire format, which is closer than we'd like (LSP
//! also uses `(line, character)` ranges) but pulls in a lot of unrelated
//! request/response shapes and ties this crate to the LSP version
//! `tower-lsp` happens to ship. We mirror just the shapes the sidecar
//! actually emits and convert at the `mimir-server` boundary, the same
//! pattern `mimir-syntax` follows for its own [`Diagnostic`].
//!
//! ## Why our own enum and not slang's diagnostic codes?
//!
//! slang's diagnostic codes are stable strings (e.g. `"UnknownModule"`).
//! We forward them through the [`Diagnostic::code`] field unchanged so the
//! editor can group/filter by them, but we don't enumerate them on the Rust
//! side — keeping the two ends loosely coupled means a slang upgrade that
//! adds a new diagnostic code doesn't require a coordinated Rust release.

use mimir_ast::MimirAst;
use mimir_core::{Position, Range};
use serde::{Deserialize, Serialize};

// --------------------------------------------------------------------------
// Method names
// --------------------------------------------------------------------------

/// Method name strings recognized by the sidecar. Kept as `pub const`s so
/// callers can refer to them symbolically and the compiler catches typos
/// the wire format wouldn't.
pub mod methods {
    /// Elaborate and export the full symbol table as a [`super::CompileResult`].
    /// Params: [`super::ElaborateParams`]; result: [`super::CompileResult`].
    pub const COMPILE: &str = "compile";

    /// Politely ask the sidecar to exit. No params, no result.
    /// The client should still wait on the child after sending this.
    pub const SHUTDOWN: &str = "shutdown";

    /// Recursively expand the macro under a cursor position and return the
    /// expanded source text. Params: [`super::ExpandMacroParams`]; result:
    /// [`super::ExpandMacroResult`].
    pub const EXPAND_MACRO: &str = "expandMacro";
}

// --------------------------------------------------------------------------
// Request / Response envelopes
// --------------------------------------------------------------------------

/// One request sent client → sidecar.
///
/// `params` is left as a raw `serde_json::Value` so this envelope can carry
/// any method's payload without a sum type that has to be updated for every
/// new method. Method-specific param types ([`ElaborateParams`], …) live
/// alongside in this module and are encoded into `params` by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Monotonically increasing per-client. The sidecar must echo this back
    /// in its response so the client can correlate even if responses
    /// arrive out of order (today they don't, but we don't want to bake
    /// "single-flight" into the wire format).
    pub id: u64,
    /// One of the constants in [`methods`].
    pub method: String,
    /// Method-specific payload. May be `null` for parameter-less methods
    /// like `shutdown`.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// One response sent sidecar → client. Exactly one of `result` / `error` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Echo of the originating [`Request::id`].
    pub id: u64,
    /// Set on success. Method-specific shape; the client decodes it into
    /// e.g. [`CompileResult`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Set on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// Failure payload inside a [`Response`]. Mirrors JSON-RPC's error shape
/// minus the `data` field, which we don't currently carry anything in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    /// Numeric error class. We don't define a full enum yet — the sidecar
    /// just uses `-1` for "internal error" and `-32602` for invalid params,
    /// matching JSON-RPC's reserved range so a future migration is cheap.
    pub code: i32,
    /// Human-readable message. Not localised; intended for logs and the
    /// editor's "language server output" panel, not end-user UI.
    pub message: String,
}

// --------------------------------------------------------------------------
// `elaborate` method
// --------------------------------------------------------------------------

/// Params for [`methods::COMPILE`]. (The name predates the `elaborate` →
/// `compile` method rename; the wire shape is unchanged.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElaborateParams {
    /// In-memory snapshot of every source file slang should see. The
    /// sidecar does **not** read from disk for these; it uses `text`
    /// verbatim. This lets us elaborate a file the user is currently
    /// editing (with unsaved changes) without writing to disk first.
    pub files: Vec<SourceFile>,

    /// Directories searched for `` `include "..." ``. Order matters —
    /// slang tries them left-to-right.
    #[serde(default)]
    pub include_dirs: Vec<String>,

    /// `+define+NAME[=VALUE]` macros to seed the preprocessor with.
    #[serde(default)]
    pub defines: Vec<MacroDefine>,

    /// Optional top module/program name to elaborate from. When `None`, the
    /// sidecar elaborates every top-level it finds — useful for
    /// "diagnostics across the whole compilation unit" mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<String>,

    /// Long-tail libslang flags parsed through `slang::driver::Driver` in
    /// the sidecar. Use this for any option that doesn't have a dedicated
    /// typed field — e.g. `["--allow-use-before-declare"]`,
    /// `["--ignore-unknown-modules"]`. For `--single-unit` and
    /// `--timescale` use the typed fields below; on conflict the typed
    /// field wins.
    ///
    /// Omitted from the wire when empty (`skip_serializing_if`) so existing
    /// sidecar versions that do not recognise the field are unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,

    /// When `true`, every `is_compilation_unit: true` file is parsed into a
    /// single shared compilation unit so `` `define `` macros leak across
    /// files in the order they were sent. Mirrors slang's `--single-unit`
    /// CLI flag. When `false` (the default and the wire-omitted form) each
    /// file is its own CU — slang's default behaviour.
    ///
    /// This is the right knob for UVM-style projects where headers like
    /// `uvm_macros.svh` are included once and the macros are expected to
    /// be visible to every later file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_unit: bool,

    /// Default timescale applied to design elements that don't declare
    /// their own (e.g. `"1ns/1ps"`). Parsed in the sidecar via
    /// `slang::TimeScale::fromString`; invalid strings are logged and
    /// dropped — never an RPC error. Wire-omitted when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timescale: Option<String>,
}

/// One file in [`ElaborateParams::files`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    /// Filesystem path. Need not exist on disk — the sidecar uses this
    /// purely as an identity / for error messages. Diagnostics report the
    /// same string back in [`Diagnostic::path`] so the client can match.
    pub path: String,
    /// Source text. Must be valid UTF-8. Slang internally re-encodes for
    /// its own buffers; we don't pass any encoding hints.
    pub text: String,
    /// Whether the sidecar should treat this file as its own top-level
    /// compilation unit (i.e. wrap it in a `SyntaxTree` and add it to the
    /// `Compilation`).
    ///
    /// `true` for files listed in the project filelist — those are the
    /// roots slang elaborates from.
    ///
    /// `false` for files we send only so their unsaved buffer is visible
    /// to the preprocessor when some compilation unit `` `include ``s
    /// them. Parsing an includee standalone produces spurious errors
    /// (e.g. class definitions outside their `package` context) and, if
    /// the path collides with what the preprocessor already loaded via
    /// `` `include ``, slang's `SourceManager::assignText` rejects the
    /// duplicate buffer outright.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub is_compilation_unit: bool,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(v: &bool) -> bool {
    *v
}

/// One `+define+` macro.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroDefine {
    /// Macro identifier. No leading `+define+`, no whitespace.
    pub name: String,
    /// Optional replacement text. `None` is equivalent to `+define+NAME` —
    /// the macro is defined but expands to nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Result for [`methods::COMPILE`].
///
/// Returned by the sidecar's `compile` RPC: the elaborated symbol table in
/// Mimir's backend-agnostic format, plus the flat diagnostics list used to
/// drive LSP `publishDiagnostics` (same items as `ast.files[].diagnostics`
/// but flattened and keyed by file path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileResult {
    /// The elaborated symbol table for all compiled files.
    pub ast: MimirAst,
    /// All diagnostics produced during compilation, flattened by file path.
    pub diagnostics: Vec<Diagnostic>,
}

// --------------------------------------------------------------------------
// `expandMacro` method
// --------------------------------------------------------------------------

/// Params for [`methods::EXPAND_MACRO`].
///
/// Carries the same preprocessor context as [`ElaborateParams`] (files,
/// include dirs, defines, `single_unit`, `timescale`, `extra_args`) so the
/// expansion sees exactly the macro state a compile would — critical for UVM,
/// where the macros being expanded are defined in a header (`uvm_macros.svh`)
/// included once near the top of the unit. Adds the cursor's file + position
/// identifying which macro usage to expand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandMacroParams {
    /// In-memory snapshot of every source file slang should see — same shape
    /// and semantics as [`ElaborateParams::files`].
    pub files: Vec<SourceFile>,

    /// Directories searched for `` `include "..." ``.
    #[serde(default)]
    pub include_dirs: Vec<String>,

    /// `+define+NAME[=VALUE]` macros to seed the preprocessor with.
    #[serde(default)]
    pub defines: Vec<MacroDefine>,

    /// Long-tail libslang flags — see [`ElaborateParams::extra_args`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,

    /// Single shared compilation unit — see [`ElaborateParams::single_unit`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_unit: bool,

    /// Default timescale — see [`ElaborateParams::timescale`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timescale: Option<String>,

    /// Path of the file whose macro usage we want expanded. Must match one of
    /// the [`SourceFile::path`] strings in `files`.
    pub target_path: String,

    /// Cursor position (zero-based line, UTF-16 character) somewhere on the
    /// macro usage to expand.
    pub position: Position,
}

/// Result for [`methods::EXPAND_MACRO`].
///
/// `expanded_text` is the fully-recursive expansion of the macro usage. When
/// the cursor isn't on a macro usage the sidecar returns
/// [`ExpandMacroResult::found`] `= false` with the other fields at their
/// defaults — the server turns that into a null LSP response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExpandMacroResult {
    /// Whether a macro usage was found at the requested position.
    #[serde(default)]
    pub found: bool,

    /// The fully-expanded source text. Empty when `found` is `false`, or when
    /// the macro legitimately expands to nothing.
    #[serde(default)]
    pub expanded_text: String,

    /// Name of the macro that was expanded (without the leading backtick),
    /// e.g. `"uvm_component_utils_begin"`. Empty when `found` is `false`.
    #[serde(default)]
    pub macro_name: String,

    /// Range of the macro usage in `target_path` that was expanded — the
    /// editor can use this to anchor a hover or selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_range: Option<Range>,

    /// Number of lines in `expanded_text`. Precomputed so the hover footer
    /// can show "Expands to N lines" without the server re-counting.
    #[serde(default)]
    pub line_count: u32,

    /// Diagnostics produced while preprocessing for the expansion (e.g. an
    /// undefined nested macro). Usually empty.
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

// --------------------------------------------------------------------------
// Diagnostic
// --------------------------------------------------------------------------

/// One diagnostic emitted by slang. Mirrors LSP's shape closely enough that
/// `mimir-server` can convert in a few lines, but lives in this crate so
/// `mimir-slang` doesn't depend on `lsp_types` (same reasoning as
/// `mimir-syntax::Diagnostic`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Path of the file the diagnostic refers to. Matches a
    /// [`SourceFile::path`] from the originating request — the sidecar
    /// echoes the exact string we sent so the client doesn't have to do
    /// any path canonicalisation to correlate.
    pub path: String,
    /// LSP-coordinate range (zero-based line, UTF-16 character offset).
    /// The sidecar is responsible for converting from slang's internal
    /// byte offsets — doing it on the C++ side keeps this crate from
    /// having to re-tokenise to count UTF-16 units.
    pub range: Range,
    /// Severity bucket. See [`Severity`].
    pub severity: Severity,
    /// Slang's stable diagnostic code (e.g. `"UnknownModule"`,
    /// `"ExpectedExpression"`). Forwarded verbatim — see the module-level
    /// docs for why we don't enumerate them.
    pub code: String,
    /// Human-readable message. Includes any inline source quote slang
    /// generates; intended to be shown directly to the user.
    pub message: String,
}

/// Severity bucket. Mirrors LSP's four levels.
///
/// Serialised as a lowercase string (`"error"`, `"warning"`, …) rather
/// than LSP's numeric code because human-debuggable JSON is more useful
/// than a wire-byte saved per message at this volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Hard error — code won't compile / elaborate.
    Error,
    /// Soft warning — likely problem, won't block compile.
    Warning,
    /// Informational — style hint, etc.
    Information,
    /// Editor hint — usually rendered as faded text.
    Hint,
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::Position;
    use pretty_assertions::assert_eq;

    /// `Request` round-trips through JSON without losing any fields.
    #[test]
    fn request_roundtrip() {
        let original = Request {
            id: 7,
            method: methods::COMPILE.to_string(),
            params: serde_json::json!({"files": []}),
        };
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.id, 7);
        assert_eq!(decoded.method, "compile");
        assert_eq!(decoded.params, original.params);
    }

    /// A success `Response` carries `result` and no `error`, and serialises
    /// without an `error: null` field cluttering the wire.
    #[test]
    fn response_success_omits_error_field() {
        let r = Response {
            id: 1,
            result: Some(serde_json::json!({"diagnostics": []})),
            error: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("error"), "expected no `error` in: {s}");
        assert!(s.contains("result"));
    }

    /// A failure `Response` carries `error` and no `result`.
    #[test]
    fn response_error_omits_result_field() {
        let r = Response {
            id: 1,
            result: None,
            error: Some(ResponseError {
                code: -1,
                message: "boom".into(),
            }),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("result"), "expected no `result` in: {s}");
        assert!(s.contains("error"));
        assert!(s.contains("boom"));
    }

    /// Severity serialises lowercase so the JSON is human-readable.
    #[test]
    fn severity_serialises_lowercase() {
        assert_eq!(serde_json::to_string(&Severity::Error).unwrap(), "\"error\"");
        assert_eq!(serde_json::to_string(&Severity::Warning).unwrap(), "\"warning\"");
        assert_eq!(serde_json::to_string(&Severity::Information).unwrap(), "\"information\"");
        assert_eq!(serde_json::to_string(&Severity::Hint).unwrap(), "\"hint\"");
    }

    /// `ElaborateParams` defaults: omitting `include_dirs`, `defines`, and
    /// `top` decodes successfully — the sidecar can rely on the defaults.
    #[test]
    fn elaborate_params_defaults_on_missing_fields() {
        let json = r#"{"files": [{"path": "a.sv", "text": "module m; endmodule"}]}"#;
        let p: ElaborateParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.files.len(), 1);
        assert!(p.include_dirs.is_empty());
        assert!(p.defines.is_empty());
        assert!(p.top.is_none());
        // Older requests without the field decode as compilation units —
        // that's the sidecar's previous behavior, preserved.
        assert!(p.files[0].is_compilation_unit);
        // Newer typed knobs decode as their false / None defaults so older
        // clients (no `single_unit` / `timescale` on the wire) keep the
        // sidecar's pre-existing per-CU + no-default-timescale behaviour.
        assert!(!p.single_unit);
        assert!(p.timescale.is_none());
    }

    /// `single_unit` and `timescale` round-trip through JSON, and the
    /// wire form omits them when at their default values so older
    /// sidecars don't see unknown fields.
    #[test]
    fn elaborate_params_single_unit_and_timescale_roundtrip() {
        let p = ElaborateParams {
            files: vec![],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: true,
            timescale: Some("1ns/1ps".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("single_unit"), "expected `single_unit` on the wire: {s}");
        assert!(s.contains("1ns/1ps"), "expected timescale on the wire: {s}");
        let back: ElaborateParams = serde_json::from_str(&s).unwrap();
        assert!(back.single_unit);
        assert_eq!(back.timescale.as_deref(), Some("1ns/1ps"));
    }

    /// Default values for the new fields are skipped from the wire so a
    /// pre-0.7.11 sidecar with `deny_unknown_fields` (if any) keeps
    /// accepting the payload.
    #[test]
    fn elaborate_params_omits_default_typed_knobs_on_serialise() {
        let p = ElaborateParams {
            files: vec![SourceFile {
                path: "a.sv".into(),
                text: "".into(),
                is_compilation_unit: true,
            }],
            include_dirs: vec![],
            defines: vec![],
            top: None,
            extra_args: vec![],
            single_unit: false,
            timescale: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("single_unit"), "expected default `single_unit` to be skipped: {s}");
        assert!(!s.contains("timescale"), "expected default `timescale` to be skipped: {s}");
    }

    /// A `SourceFile` with the default flag round-trips and the encoded
    /// JSON omits the field, keeping the wire compact.
    #[test]
    fn source_file_compilation_unit_default_omitted_on_serialise() {
        let f = SourceFile {
            path: "a.sv".into(),
            text: "".into(),
            is_compilation_unit: true,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(
            !s.contains("is_compilation_unit"),
            "expected default-true field to be skipped: {s}",
        );
    }

    /// `is_compilation_unit: false` survives a round-trip — this is the
    /// signal the sidecar uses to seed the SourceManager without parsing
    /// the file as its own translation unit.
    #[test]
    fn source_file_includee_roundtrip() {
        let f = SourceFile {
            path: "agent.sv".into(),
            text: "class c; endclass".into(),
            is_compilation_unit: false,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("is_compilation_unit"));
        let back: SourceFile = serde_json::from_str(&s).unwrap();
        assert!(!back.is_compilation_unit);
    }

    /// A `CompileResult` wire payload that carries a `references` table
    /// on one of its files decodes intact, with the entries' fields
    /// preserved. This locks the wire shape of the new reference map.
    #[test]
    fn compile_result_with_references_roundtrip() {
        let payload = serde_json::json!({
            "ast": {
                "files": [{
                    "uri": "/proj/use.sv",
                    "diagnostics": [],
                    "top_scope": {
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 0}},
                        "declarations": [],
                        "children": [],
                        "imported_packages": []
                    },
                    "references": [{
                        "use_range": {"start": {"line": 5, "character": 8}, "end": {"line": 5, "character": 17}},
                        "target_path": "/proj/def.sv",
                        "target_range": {"start": {"line": 42, "character": 6}, "end": {"line": 42, "character": 15}},
                        "target_kind": "function"
                    }]
                }]
            },
            "diagnostics": []
        });
        let decoded: CompileResult =
            serde_json::from_value(payload).expect("decode references payload");
        let refs = &decoded.ast.files[0].references;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target_path, "/proj/def.sv");
        assert_eq!(refs[0].use_range.start.line, 5);
        assert_eq!(refs[0].target_range.start.line, 42);

        // Encode → decode again to confirm round-trip stability.
        let encoded = serde_json::to_string(&decoded).unwrap();
        let back: CompileResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back.ast.files[0].references[0].target_path, "/proj/def.sv");
    }

    /// A sidecar that pre-dates the reference map omits `references`.
    /// The decoder must accept the legacy payload and surface an empty
    /// `references` vec — this is the rollout-compatibility contract.
    #[test]
    fn compile_result_without_references_decodes_as_empty() {
        let legacy = serde_json::json!({
            "ast": {
                "files": [{
                    "uri": "/proj/legacy.sv",
                    "diagnostics": [],
                    "top_scope": {
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
                        "declarations": [],
                        "children": [],
                        "imported_packages": []
                    }
                }]
            },
            "diagnostics": []
        });
        let decoded: CompileResult =
            serde_json::from_value(legacy).expect("legacy decode");
        assert!(decoded.ast.files[0].references.is_empty());
    }

    /// A wire payload that includes a fully-populated ref — with
    /// target_type_str / target_params / target_parent_class — decodes
    /// into the new `MimirRef` fields. This locks the wire shape that
    /// the sidecar will emit for callable targets (functions, tasks)
    /// so hover/inlay-hint/signature-help can render entirely from the
    /// ref, without needing to find the target's declaration in the
    /// AST (which often lives in a UVM-or-vendor file the client
    /// didn't put in `params["files"]`).
    #[test]
    fn ref_with_target_metadata_decodes_on_the_wire() {
        let payload = serde_json::json!({
            "ast": {
                "files": [{
                    "uri": "/proj/use.sv",
                    "diagnostics": [],
                    "top_scope": {
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 0}},
                        "declarations": [],
                        "children": [],
                        "imported_packages": []
                    },
                    "references": [{
                        "use_range":    {"start": {"line": 4, "character": 10}, "end": {"line": 4, "character": 19}},
                        "target_path":  "/uvm-1.2/src/reg/uvm_mem.svh",
                        "target_range": {"start": {"line": 1234, "character": 17}, "end": {"line": 1234, "character": 26}},
                        "target_kind":  "function",
                        "target_type_str": "void",
                        "target_params": [
                            {"name": "name",  "type_str": "string"},
                            {"name": "size",  "type_str": "longint unsigned"},
                            {"name": "n_bits","type_str": "int unsigned"}
                        ],
                        "target_parent_class": "uvm_mem"
                    }]
                }]
            },
            "diagnostics": []
        });
        let decoded: CompileResult =
            serde_json::from_value(payload).expect("decode ref with target metadata");
        let r = &decoded.ast.files[0].references[0];
        assert_eq!(r.target_type_str.as_deref(), Some("void"));
        assert_eq!(r.target_params.len(), 3);
        assert_eq!(r.target_params[1].name, "size");
        assert_eq!(r.target_params[1].type_str.as_deref(), Some("longint unsigned"));
        assert_eq!(r.target_parent_class.as_deref(), Some("uvm_mem"));
    }

    /// Refs from older sidecars (without target_* metadata) must still
    /// decode — the new fields default to None/empty so the consumer's
    /// ref-first paths gracefully fall back to their name-based behaviour.
    #[test]
    fn ref_without_target_metadata_decodes_with_defaults() {
        let legacy = serde_json::json!({
            "ast": {
                "files": [{
                    "uri": "/proj/use.sv",
                    "diagnostics": [],
                    "top_scope": {
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
                        "declarations": [], "children": [], "imported_packages": []
                    },
                    "references": [{
                        "use_range":    {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                        "target_path":  "/x.sv",
                        "target_range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 3}},
                        "target_kind":  "variable"
                    }]
                }]
            },
            "diagnostics": []
        });
        let decoded: CompileResult =
            serde_json::from_value(legacy).expect("legacy ref decode");
        let r = &decoded.ast.files[0].references[0];
        assert!(r.target_type_str.is_none());
        assert!(r.target_params.is_empty());
        assert!(r.target_parent_class.is_none());
    }

    /// `ExpandMacroParams` round-trips, and the default context knobs
    /// (`extra_args` / `single_unit` / `timescale`) are omitted from the
    /// wire so an older sidecar never sees unknown fields.
    #[test]
    fn expand_macro_params_roundtrip_and_omits_defaults() {
        let p = ExpandMacroParams {
            files: vec![SourceFile {
                path: "/proj/agent.sv".into(),
                text: "`uvm_info(\"T\", \"hi\", UVM_LOW)".into(),
                is_compilation_unit: true,
            }],
            include_dirs: vec!["/uvm/src".into()],
            defines: vec![],
            extra_args: vec![],
            single_unit: false,
            timescale: None,
            target_path: "/proj/agent.sv".into(),
            position: Position::new(0, 4),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("extra_args"), "default extra_args should be skipped: {s}");
        assert!(!s.contains("single_unit"), "default single_unit should be skipped: {s}");
        assert!(!s.contains("timescale"), "default timescale should be skipped: {s}");
        assert!(s.contains("target_path"));
        assert!(s.contains("position"));

        let back: ExpandMacroParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.target_path, "/proj/agent.sv");
        assert_eq!(back.position, Position::new(0, 4));
        assert_eq!(back.include_dirs, vec!["/uvm/src".to_string()]);
    }

    /// `single_unit` and `timescale` survive the round-trip when set.
    #[test]
    fn expand_macro_params_carries_single_unit_and_timescale() {
        let p = ExpandMacroParams {
            files: vec![],
            include_dirs: vec![],
            defines: vec![],
            extra_args: vec!["--allow-use-before-declare".into()],
            single_unit: true,
            timescale: Some("1ns/1ps".into()),
            target_path: "/x.sv".into(),
            position: Position::new(2, 1),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: ExpandMacroParams = serde_json::from_str(&s).unwrap();
        assert!(back.single_unit);
        assert_eq!(back.timescale.as_deref(), Some("1ns/1ps"));
        assert_eq!(back.extra_args, vec!["--allow-use-before-declare".to_string()]);
    }

    /// A full `ExpandMacroResult` decodes intact from the wire shape the
    /// sidecar emits on a hit.
    #[test]
    fn expand_macro_result_found_roundtrip() {
        let payload = serde_json::json!({
            "found": true,
            "expanded_text": "function void build_phase();\n  super.build_phase();\nendfunction",
            "macro_name": "uvm_component_utils_begin",
            "usage_range": {"start": {"line": 10, "character": 2}, "end": {"line": 10, "character": 30}},
            "line_count": 3,
            "diagnostics": []
        });
        let r: ExpandMacroResult = serde_json::from_value(payload).expect("decode expand result");
        assert!(r.found);
        assert_eq!(r.macro_name, "uvm_component_utils_begin");
        assert_eq!(r.line_count, 3);
        assert!(r.expanded_text.contains("build_phase"));
        let usage = r.usage_range.expect("usage_range present");
        assert_eq!(usage.start.line, 10);

        // Re-encode → decode to confirm stability.
        let s = serde_json::to_string(&r).unwrap();
        let back: ExpandMacroResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.macro_name, "uvm_component_utils_begin");
    }

    /// A "not on a macro" result decodes with `found = false` and empty
    /// fields. The minimal wire form (just `{"found": false}`) must also
    /// decode, since the sidecar skips empty/None fields.
    #[test]
    fn expand_macro_result_not_found_decodes_minimal() {
        let r: ExpandMacroResult =
            serde_json::from_str(r#"{"found": false}"#).expect("decode minimal");
        assert!(!r.found);
        assert!(r.expanded_text.is_empty());
        assert!(r.macro_name.is_empty());
        assert!(r.usage_range.is_none());
        assert_eq!(r.line_count, 0);
        assert!(r.diagnostics.is_empty());

        // Default-constructed value is the same "not found" shape.
        let d = ExpandMacroResult::default();
        assert!(!d.found);
    }

    /// `Diagnostic` round-trips with a realistic-looking range.
    #[test]
    fn diagnostic_roundtrip() {
        let d = Diagnostic {
            path: "/proj/a.sv".into(),
            range: Range::new(Position::new(3, 12), Position::new(3, 18)),
            severity: Severity::Error,
            code: "UnknownModule".into(),
            message: "unknown module 'apb_master'".into(),
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: Diagnostic = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }
}
