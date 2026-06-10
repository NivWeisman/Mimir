//! Filesystem-path → `file://` URL conversion shared by feature modules.
//!
//! The sidecar and the AST reference map identify files by plain path
//! strings; LSP responses need [`Url`]s. The conversion must go through
//! [`Url::from_file_path`] so paths containing spaces or non-ASCII
//! characters are percent-encoded — `Url::parse` on a hand-assembled
//! `format!("file://{path}")` string rejects them outright, which used to
//! silently break go-to-definition into such files.

use tower_lsp::lsp_types::Url;
use tracing::debug;

/// Convert an absolute filesystem path into a `file://` [`Url`].
///
/// Returns `None` (with a debug log) when the path is not absolute — the
/// only way [`Url::from_file_path`] can fail on Unix. Callers treat that
/// as "target not resolvable" and fall through to their next strategy.
pub(crate) fn file_uri(path: &str) -> Option<Url> {
    match Url::from_file_path(path) {
        Ok(url) => Some(url),
        Err(()) => {
            debug!(path, "cannot convert non-absolute path to a file:// URL");
            None
        }
    }
}

/// Convert a `file://` [`Url`] into the plain path string used to key
/// sidecar requests and `MimirAst` file entries. Returns `None` for
/// non-file URLs and for paths that aren't valid UTF-8.
pub(crate) fn uri_to_path_string(uri: &Url) -> Option<String> {
    uri.to_file_path()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A plain absolute path converts and round-trips.
    #[test]
    fn absolute_path_converts() {
        let url = file_uri("/proj/src/top.sv").expect("absolute path converts");
        assert_eq!(url.as_str(), "file:///proj/src/top.sv");
        assert_eq!(url.to_file_path().unwrap().to_str().unwrap(), "/proj/src/top.sv");
    }

    /// Paths with spaces are percent-encoded instead of failing — the
    /// regression this helper exists to prevent (`Url::parse` on a
    /// hand-built string returns `Err` for these).
    #[test]
    fn path_with_spaces_is_percent_encoded() {
        let url = file_uri("/my proj/agent file.sv").expect("space path converts");
        assert_eq!(url.as_str(), "file:///my%20proj/agent%20file.sv");
        // Round-trip restores the original path.
        assert_eq!(
            url.to_file_path().unwrap().to_str().unwrap(),
            "/my proj/agent file.sv"
        );
    }

    /// Non-ASCII path components convert (and the old `Url::parse`
    /// formulation would have produced a host-mangled or invalid URL).
    #[test]
    fn non_ascii_path_converts() {
        let url = file_uri("/projekt/übung/tb.sv").expect("non-ascii path converts");
        assert_eq!(
            url.to_file_path().unwrap().to_str().unwrap(),
            "/projekt/übung/tb.sv"
        );
    }

    /// Relative paths cannot become file:// URLs; we return None rather
    /// than fabricating a wrong URL.
    #[test]
    fn relative_path_returns_none() {
        assert!(file_uri("src/top.sv").is_none());
        assert!(file_uri("").is_none());
    }
}
