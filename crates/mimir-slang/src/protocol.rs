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

use mimir_core::Range;
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
