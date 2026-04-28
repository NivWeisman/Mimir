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
//! actually emits and convert at the [`mimir-server`] boundary, the same
//! pattern `mimir-syntax` follows for its own [`Diagnostic`].
//!
//! ## Why our own enum and not slang's diagnostic codes?
//!
//! slang's diagnostic codes are stable strings (e.g. `"UnknownModule"`).
//! We forward them through the [`Diagnostic::code`] field unchanged so the
//! editor can group/filter by them, but we don't enumerate them on the Rust
//! side — keeping the two ends loosely coupled means a slang upgrade that
//! adds a new diagnostic code doesn't require a coordinated Rust release.

use mimir_core::{Position, Range};
use serde::{Deserialize, Serialize};

// --------------------------------------------------------------------------
// Method names
// --------------------------------------------------------------------------

/// Method name strings recognized by the sidecar. Kept as `pub const`s so
/// callers can refer to them symbolically and the compiler catches typos
/// the wire format wouldn't.
pub mod methods {
    /// Elaborate a set of source files and return any diagnostics.
    /// Params: [`super::ElaborateParams`]; result: [`super::ElaborateResult`].
    pub const ELABORATE: &str = "elaborate";

    /// Resolve the declaration site of an identifier reference. Used to
    /// power LSP `textDocument/definition` with slang's semantic
    /// resolver (scope-aware, hierarchical-name-aware).
    /// Params: [`super::DefinitionParams`]; result: [`super::DefinitionResult`].
    pub const DEFINITION: &str = "definition";

    /// Politely ask the sidecar to exit. No params, no result.
    /// The client should still wait on the child after sending this.
    pub const SHUTDOWN: &str = "shutdown";
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
    /// e.g. [`ElaborateResult`].
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

/// Params for [`methods::ELABORATE`].
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

/// Result for [`methods::ELABORATE`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElaborateResult {
    /// All diagnostics produced during preprocessing, parsing, and
    /// elaboration, flattened across every file in the request. Empty
    /// vector means "elaboration succeeded with no warnings or errors."
    pub diagnostics: Vec<Diagnostic>,
}

// --------------------------------------------------------------------------
// `definition` method
// --------------------------------------------------------------------------

/// Params for [`methods::DEFINITION`].
///
/// The first four fields mirror [`ElaborateParams`] so the sidecar can
/// reuse its compilation cache when the inputs match — answering
/// definition queries without re-elaborating. `target_path` and
/// `target_position` pin the cursor in that compilation unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionParams {
    /// Same shape as [`ElaborateParams::files`]: every source file the
    /// sidecar should see, with in-memory text overriding disk.
    pub files: Vec<SourceFile>,

    /// `+incdir+` paths, in slang's search order.
    #[serde(default)]
    pub include_dirs: Vec<String>,

    /// `+define+` macros to seed the preprocessor with.
    #[serde(default)]
    pub defines: Vec<MacroDefine>,

    /// Optional top module/program. When `None`, slang elaborates every
    /// top-level — same convention as `elaborate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<String>,

    /// Filesystem path of the file the cursor is in. Must match the
    /// `path` of one of the [`SourceFile`]s in `files` — the sidecar
    /// resolves the cursor to a `SourceLocation` by exact path match.
    pub target_path: String,

    /// LSP-coordinate position of the reference under the cursor
    /// (zero-based line, UTF-16 character).
    pub target_position: Position,
}

/// Result for [`methods::DEFINITION`].
///
/// An empty `locations` vector is the sidecar's authoritative "no
/// declaration found" — the server does not fall back to syntax in that
/// case, because slang's empty answer is more accurate than syntactic
/// fuzzy-match. Transport errors *do* fall back to syntax; that's
/// handled at the server boundary, not on the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefinitionResult {
    /// Zero, one, or many declaration sites. Multiple sites are valid
    /// (e.g. `extern` declarations + their definition); the editor
    /// shows them as a peek list.
    #[serde(default)]
    pub locations: Vec<DefinitionLocation>,
}

/// One declaration site returned by [`DefinitionResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionLocation {
    /// Filesystem path of the file the declaration lives in. Format
    /// matches [`Diagnostic::path`] / [`SourceFile::path`] — the same
    /// string the sidecar received in the request.
    pub path: String,
    /// LSP-coordinate range of the declaration's identifier token. The
    /// server hands this to the editor verbatim as `Location.range`.
    pub range: Range,
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
            method: methods::ELABORATE.to_string(),
            params: serde_json::json!({"files": []}),
        };
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.id, 7);
        assert_eq!(decoded.method, "elaborate");
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

    /// `DefinitionParams` round-trips, including the target fields.
    #[test]
    fn definition_params_roundtrip() {
        let p = DefinitionParams {
            files: vec![SourceFile {
                path: "/proj/a.sv".into(),
                text: "module a; endmodule".into(),
                is_compilation_unit: true,
            }],
            include_dirs: vec!["/proj/inc".into()],
            defines: vec![MacroDefine {
                name: "BUS_W".into(),
                value: Some("32".into()),
            }],
            top: Some("a".into()),
            target_path: "/proj/a.sv".into(),
            target_position: Position::new(0, 7),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: DefinitionParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.target_path, "/proj/a.sv");
        assert_eq!(back.target_position.line, 0);
        assert_eq!(back.target_position.character, 7);
        assert_eq!(back.top.as_deref(), Some("a"));
    }

    /// `DefinitionParams` defaults: omitting `include_dirs`, `defines`,
    /// `top` decodes successfully — the sidecar can rely on the
    /// defaults exactly as it does for `elaborate`.
    #[test]
    fn definition_params_defaults_on_missing_fields() {
        let json = r#"{
            "files": [{"path": "a.sv", "text": "module m; endmodule"}],
            "target_path": "a.sv",
            "target_position": {"line": 0, "character": 7}
        }"#;
        let p: DefinitionParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.files.len(), 1);
        assert!(p.include_dirs.is_empty());
        assert!(p.defines.is_empty());
        assert!(p.top.is_none());
        assert_eq!(p.target_path, "a.sv");
    }

    /// `DefinitionResult` with `locations: []` — the "no declaration
    /// found" wire shape — round-trips and decodes cleanly.
    #[test]
    fn definition_result_empty_locations_roundtrip() {
        let r = DefinitionResult { locations: vec![] };
        let s = serde_json::to_string(&r).unwrap();
        let back: DefinitionResult = serde_json::from_str(&s).unwrap();
        assert!(back.locations.is_empty());
        // Also: an entirely missing field decodes to empty by `#[serde(default)]`.
        let from_minimal: DefinitionResult = serde_json::from_str("{}").unwrap();
        assert!(from_minimal.locations.is_empty());
    }

    /// `DefinitionLocation` round-trips — same shape as a diagnostic
    /// minus severity/code/message.
    #[test]
    fn definition_location_roundtrip() {
        let loc = DefinitionLocation {
            path: "/proj/b.sv".into(),
            range: Range::new(Position::new(2, 6), Position::new(2, 12)),
        };
        let s = serde_json::to_string(&loc).unwrap();
        let back: DefinitionLocation = serde_json::from_str(&s).unwrap();
        assert_eq!(back, loc);
    }

    /// A `Request` envelope carrying `DefinitionParams` round-trips.
    /// Smoke-tests that the per-method param shape composes with the
    /// generic envelope, the way the client actually sends it.
    #[test]
    fn definition_request_envelope() {
        let req = Request {
            id: 42,
            method: methods::DEFINITION.to_string(),
            params: serde_json::json!({
                "files": [],
                "target_path": "x.sv",
                "target_position": {"line": 1, "character": 4},
            }),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, 42);
        assert_eq!(back.method, "definition");
        // Decoding the generic params back into the typed shape works.
        let typed: DefinitionParams = serde_json::from_value(back.params).unwrap();
        assert_eq!(typed.target_path, "x.sv");
        assert_eq!(typed.target_position.character, 4);
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
