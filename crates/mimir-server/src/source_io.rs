//! Reads source files from disk as text, tolerating invalid UTF-8.
//!
//! Every place the server loads a file that isn't open in the editor —
//! workspace-index hydration, the closed-file texts sent to the slang
//! sidecar, hover / completion-resolve declaration lookups — goes through
//! [`read_source_lossy`], so they all agree on what "the file's text" is.

use std::path::Path;

use tracing::debug;

/// Read a source file from disk as text, tolerating invalid UTF-8.
///
/// SystemVerilog sources are nominally ASCII, but legacy RTL and vendor IP
/// routinely carry Latin-1 bytes in comments (`©`, `°`, `µ`, umlauts in
/// author names). `std::fs::read_to_string` rejects such a file outright,
/// and every caller used to treat that as "file missing": it vanished from
/// the workspace index and was handed to slang as an *empty* compilation
/// unit, producing a cascade of bogus "unknown module" errors.
///
/// Invalid sequences are replaced with U+FFFD instead. Each offending byte
/// becomes exactly one character, so line numbers and UTF-16 columns — the
/// coordinates every LSP position uses — match what an editor shows for the
/// same file.
///
/// Returns `None` only when the file can't be read at all.
pub(crate) fn read_source_lossy(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            debug!(path = %path.display(), "source file is not valid UTF-8; decoding lossily");
            String::from_utf8_lossy(err.as_bytes()).into_owned()
        }
    })
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Latin-1 bytes decode to one replacement character each, keeping
    /// columns aligned; valid UTF-8 is returned untouched; a missing file
    /// is `None`.
    #[test]
    fn read_source_lossy_tolerates_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let latin1 = dir.path().join("latin1.sv");
        std::fs::write(&latin1, b"// \xA9 2019\nmodule m;\n").unwrap();
        let text = read_source_lossy(&latin1).expect("readable");
        assert_eq!(text, "// \u{FFFD} 2019\nmodule m;\n");

        let utf8 = dir.path().join("utf8.sv");
        std::fs::write(&utf8, "// © 2019\n").unwrap();
        assert_eq!(read_source_lossy(&utf8).as_deref(), Some("// © 2019\n"));

        assert!(read_source_lossy(&dir.path().join("missing.sv")).is_none());
    }
}
