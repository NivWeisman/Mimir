//! Resolve `` `include`` directives.
//!
//! Mirrors slang's preprocessor lookup so the server can expand a seed file
//! list into the full transitive set of headers a compilation unit needs.
//! Used by:
//!
//! 1. [`crate::workspace_index`] — so tree-sitter symbol indexing follows
//!    `` `include`` chains and not just the explicit filelist.
//! 2. [`crate::backend::assemble_elaborate_params`] — so unsaved edits in
//!    `` `include`` d files reach slang via the `files` array (instead of
//!    relying on slang's own on-disk lookup, which never sees in-memory
//!    edits).
//!
//! The scanner is deliberately small: it skips line and block comments and
//! basic string literals, then matches the literal `` `include`` followed by
//! either a `"…"` or `<…>` filename. It does **not** evaluate macros, so an
//! `` `include`` whose path comes from a macro expansion is silently
//! skipped — slang itself will still see it on the source side.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::{debug, trace, warn};

/// Scan `text` for `` `include`` directives and return the raw filename
/// strings (without the surrounding quotes / brackets), in source order.
///
/// Handles:
/// * `` `include "header.sv" ``
/// * `` `include <header.sv> ``
/// * comments (`//` to end-of-line, `/* … */` block) — directives inside are
///   ignored.
/// * basic double-quoted strings — `"abc \`include something"` will not be
///   treated as a directive.
///
/// Does not evaluate macros: `` `include `MY_HDR `` returns nothing.
///
/// Thin wrapper over [`scan_includes_with_spans`] — one scanner, two views.
#[must_use]
pub fn scan_includes(text: &str) -> Vec<String> {
    scan_includes_with_spans(text)
        .into_iter()
        .map(|span| span.name)
        .collect()
}

/// One `` `include `` directive located in source, with the byte span of its
/// filename (the text *inside* the quotes/brackets, excluding them).
///
/// Powers `textDocument/documentLink`: the server converts `start..end` to an
/// LSP range via the document rope and resolves `name` to a clickable target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeSpan {
    /// Filename as written, without the surrounding `"…"` / `<…>`.
    pub name: String,
    /// Byte offset of the first character of `name` in the source.
    pub start: usize,
    /// Byte offset one past the last character of `name`.
    pub end: usize,
}

/// Like [`scan_includes`] but also returns the byte span of each filename so
/// the caller can build clickable document links.
///
/// Same scanning rules as [`scan_includes`] (skips comments and strings,
/// matches `` `include `` followed by `"…"` or `<…>`, ignores macro-derived
/// paths). The returned span covers only the filename text, not the
/// delimiters — clicking anywhere on the path navigates to the file.
#[must_use]
pub fn scan_includes_with_spans(text: &str) -> Vec<IncludeSpan> {
    let bytes = text.as_bytes();
    let mut out: Vec<IncludeSpan> = Vec::new();
    let mut i = 0;
    let n = bytes.len();

    while i < n {
        let b = bytes[i];

        // Line comment.
        if b == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment.
        if b == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // Plain double-quoted string.
        if b == b'"' {
            i += 1;
            while i < n && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < n {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i = (i + 1).min(n);
            continue;
        }
        // `` `include "..." `` / `` `include <...> ``.
        if b == b'`' {
            let rest = &bytes[i + 1..];
            if rest.starts_with(b"include")
                && rest.get(7).map_or(true, |c| c.is_ascii_whitespace())
            {
                i += 1 + 7;
                while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
                    i += 1;
                }
                if i >= n {
                    break;
                }
                let close = match bytes[i] {
                    b'"' => b'"',
                    b'<' => b'>',
                    _ => continue,
                };
                i += 1;
                let start = i;
                while i < n && bytes[i] != close && bytes[i] != b'\n' {
                    i += 1;
                }
                if i < n && bytes[i] == close {
                    if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                        out.push(IncludeSpan {
                            name: s.to_owned(),
                            start,
                            end: i,
                        });
                    }
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Resolve a single relative include filename to an absolute path,
/// delegating the existence check to `exists`.
///
/// Search order (matches slang's preprocessor):
/// 1. `current_dir` — the directory of the file that contained the
///    `` `include `` directive.
/// 2. Each entry in `include_dirs`, in order.
///
/// Returns the first candidate `exists` accepts. Absolute `rel` paths are
/// checked directly. The closure-based design lets tests swap an in-memory
/// disk and lets [`expand_includes`] reuse its read seam as the existence
/// check.
#[must_use]
pub fn resolve_include_with<F>(
    rel: &str,
    current_dir: &Path,
    include_dirs: &[PathBuf],
    mut exists: F,
) -> Option<PathBuf>
where
    F: FnMut(&Path) -> bool,
{
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return exists(rel_path).then(|| rel_path.to_path_buf());
    }
    let from_current = current_dir.join(rel_path);
    if exists(&from_current) {
        return Some(from_current);
    }
    for dir in include_dirs {
        let candidate = dir.join(rel_path);
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Expand a seed list of paths to the full transitive `` `include`` set.
///
/// BFS from each seed: read its text via `read`, scan for directives,
/// resolve each one against `include_dirs` (with the current file's
/// directory tried first), and enqueue any newly discovered file. Cycles
/// are prevented by a visited set keyed on the resolved path; a `read`
/// returning `None` skips that file with a `warn!` log.
///
/// The output is the seed list followed by the newly discovered files,
/// in BFS order. Seeds are preserved verbatim (no canonicalisation) so
/// callers can match them back to the input list. Discovered files are
/// added at most once.
#[must_use]
pub fn expand_includes<F>(
    seeds: &[PathBuf],
    include_dirs: &[PathBuf],
    mut read: F,
) -> Vec<PathBuf>
where
    F: FnMut(&Path) -> Option<String>,
{
    let mut order: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
    // Texts already read by the candidate probe below, keyed by resolved
    // path. Consumed when that path is dequeued, so each discovered file
    // is read exactly once instead of once for the probe and again for
    // the scan.
    let mut prefetched: HashMap<PathBuf, String> = HashMap::new();

    for seed in seeds {
        if seen.insert(seed.clone()) {
            order.push(seed.clone());
            queue.push_back(seed.clone());
        }
    }

    while let Some(path) = queue.pop_front() {
        let Some(text) = prefetched.remove(&path).or_else(|| read(&path)) else {
            warn!(path = %path.display(), "include expand: file unreadable; skipping");
            continue;
        };
        let current_dir = path.parent().unwrap_or_else(|| Path::new(""));
        for rel in scan_includes(&text) {
            // The probe *reads* each candidate (the read closure is the
            // only disk seam, which keeps tests stubbable); a successful
            // read doubles as the existence check, and the text is kept
            // for the eventual scan of that file.
            let mut probe_hit: Option<(PathBuf, String)> = None;
            let Some(resolved) = resolve_include_with(&rel, current_dir, include_dirs, |p| {
                match read(p) {
                    Some(t) => {
                        probe_hit = Some((p.to_path_buf(), t));
                        true
                    }
                    None => false,
                }
            }) else {
                trace!(rel, "include not found in include_dirs; skipping");
                continue;
            };
            if let Some((hit_path, hit_text)) = probe_hit {
                if hit_path == resolved {
                    prefetched.insert(hit_path, hit_text);
                }
            }
            if seen.insert(resolved.clone()) {
                debug!(
                    from = %path.display(),
                    rel,
                    to = %resolved.display(),
                    "include resolved",
                );
                order.push(resolved.clone());
                queue.push_back(resolved);
            }
        }
    }
    order
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Quoted include returns the bare filename (no quotes).
    #[test]
    fn scan_includes_quoted() {
        let text = r#"`include "uvm_pkg.sv""#;
        assert_eq!(scan_includes(text), vec!["uvm_pkg.sv"]);
    }

    /// `scan_includes_with_spans` returns the filename plus the byte span
    /// that covers exactly the filename text (excluding the delimiters), so
    /// the document-link range underlines just the path.
    #[test]
    fn scan_includes_with_spans_locates_filename() {
        let text = "module m;\n`include \"hdr/uvm_pkg.svh\"\nendmodule\n";
        let spans = scan_includes_with_spans(text);
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!(s.name, "hdr/uvm_pkg.svh");
        // The span must slice back to the exact filename text.
        assert_eq!(&text[s.start..s.end], "hdr/uvm_pkg.svh");
    }

    /// Multiple includes (quoted + angle) get distinct, in-order spans, and
    /// directives inside comments are skipped.
    #[test]
    fn scan_includes_with_spans_skips_comments_and_keeps_order() {
        let text = "`include \"a.svh\"\n// `include \"ignored.svh\"\n`include <b.svh>\n";
        let spans = scan_includes_with_spans(text);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "a.svh");
        assert_eq!(spans[1].name, "b.svh");
        assert_eq!(&text[spans[0].start..spans[0].end], "a.svh");
        assert_eq!(&text[spans[1].start..spans[1].end], "b.svh");
    }

    /// Angle-bracket include returns the bare filename (no brackets).
    #[test]
    fn scan_includes_angle() {
        let text = "`include <uvm_pkg.sv>";
        assert_eq!(scan_includes(text), vec!["uvm_pkg.sv"]);
    }

    /// Whitespace between `` `include `` and the filename is tolerated.
    #[test]
    fn scan_includes_extra_whitespace() {
        let text = "`include   \t\"a.sv\"";
        assert_eq!(scan_includes(text), vec!["a.sv"]);
    }

    /// Multiple includes are returned in source order.
    #[test]
    fn scan_includes_multiple() {
        let text = "`include \"a.sv\"\n`include \"b.sv\"\n`include <c.sv>\n";
        assert_eq!(scan_includes(text), vec!["a.sv", "b.sv", "c.sv"]);
    }

    /// Includes inside line comments are ignored.
    #[test]
    fn scan_includes_skips_line_comments() {
        let text = "// `include \"x.sv\"\n`include \"y.sv\"\n";
        assert_eq!(scan_includes(text), vec!["y.sv"]);
    }

    /// Includes inside block comments are ignored.
    #[test]
    fn scan_includes_skips_block_comments() {
        let text = "/* `include \"x.sv\" */ `include \"y.sv\"";
        assert_eq!(scan_includes(text), vec!["y.sv"]);
    }

    /// Includes inside string literals are ignored.
    #[test]
    fn scan_includes_skips_strings() {
        let text = r#"const string s = "`include \"x.sv\"";
`include "y.sv""#;
        assert_eq!(scan_includes(text), vec!["y.sv"]);
    }

    /// `` `define `` (or other directives) are not matched.
    #[test]
    fn scan_includes_other_directives_ignored() {
        let text = "`define FOO 1\n`ifdef BAR\n`include \"a.sv\"\n`endif";
        assert_eq!(scan_includes(text), vec!["a.sv"]);
    }

    /// Macro-expanded includes are skipped (we don't run the preprocessor).
    #[test]
    fn scan_includes_macro_expanded_skipped() {
        let text = "`include `MY_HEADER\n`include \"real.sv\"";
        assert_eq!(scan_includes(text), vec!["real.sv"]);
    }

    /// Resolver tries the current dir first.
    #[test]
    fn resolve_include_prefers_current_dir() {
        let exists = |p: &Path| {
            matches!(
                p.to_str(),
                Some("/proj/sub/uvm_pkg.sv") | Some("/uvm/src/uvm_pkg.sv")
            )
        };
        let got = resolve_include_with(
            "uvm_pkg.sv",
            Path::new("/proj/sub"),
            &[p("/uvm/src")],
            exists,
        );
        assert_eq!(got, Some(p("/proj/sub/uvm_pkg.sv")));
    }

    /// Resolver falls back to include_dirs in order when current dir misses.
    #[test]
    fn resolve_include_falls_back_to_include_dirs() {
        let exists = |p: &Path| matches!(p.to_str(), Some("/uvm/src/uvm_pkg.sv"));
        let got = resolve_include_with(
            "uvm_pkg.sv",
            Path::new("/proj/sub"),
            &[p("/other"), p("/uvm/src")],
            exists,
        );
        assert_eq!(got, Some(p("/uvm/src/uvm_pkg.sv")));
    }

    /// Absolute paths resolve to themselves when they exist.
    #[test]
    fn resolve_include_absolute_path() {
        let exists = |p: &Path| p == Path::new("/abs/file.sv");
        assert_eq!(
            resolve_include_with(
                "/abs/file.sv",
                Path::new("/anything"),
                &[],
                exists,
            ),
            Some(p("/abs/file.sv"))
        );
    }

    /// Missing file → None.
    #[test]
    fn resolve_include_missing_returns_none() {
        let got = resolve_include_with(
            "nope.sv",
            Path::new("/x"),
            &[p("/y")],
            |_| false,
        );
        assert_eq!(got, None);
    }

    /// `expand_includes` follows a single transitive chain.
    #[test]
    fn expand_includes_follows_transitive_chain() {
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (p("/proj/uvm.sv"), "`include \"uvm_pkg.sv\"".into()),
            (p("/proj/uvm_pkg.sv"), "`include \"uvm_object.sv\"".into()),
            (p("/proj/uvm_object.sv"), "// leaf".into()),
        ]);
        let got = expand_includes(&[p("/proj/uvm.sv")], &[], |p| {
            texts.get(p).cloned()
        });
        assert_eq!(
            got,
            vec![
                p("/proj/uvm.sv"),
                p("/proj/uvm_pkg.sv"),
                p("/proj/uvm_object.sv"),
            ]
        );
    }

    /// `expand_includes` doesn't loop on cyclic includes.
    #[test]
    fn expand_includes_breaks_cycles() {
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (p("/proj/a.sv"), "`include \"b.sv\"".into()),
            (p("/proj/b.sv"), "`include \"a.sv\"".into()),
        ]);
        let got = expand_includes(&[p("/proj/a.sv")], &[], |p| {
            texts.get(p).cloned()
        });
        assert_eq!(got, vec![p("/proj/a.sv"), p("/proj/b.sv")]);
    }

    /// `expand_includes` survives an unreadable file mid-chain.
    #[test]
    fn expand_includes_skips_unreadable() {
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (p("/proj/a.sv"), "`include \"missing.sv\"\n`include \"c.sv\"".into()),
            (p("/proj/c.sv"), "// leaf".into()),
        ]);
        let got = expand_includes(&[p("/proj/a.sv")], &[], |p| {
            texts.get(p).cloned()
        });
        assert_eq!(got, vec![p("/proj/a.sv"), p("/proj/c.sv")]);
    }

    /// `expand_includes` deduplicates repeat seeds and repeat resolutions.
    #[test]
    fn expand_includes_dedupes() {
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (p("/proj/a.sv"), "`include \"shared.sv\"".into()),
            (p("/proj/b.sv"), "`include \"shared.sv\"".into()),
            (p("/proj/shared.sv"), "// leaf".into()),
        ]);
        let got = expand_includes(
            &[p("/proj/a.sv"), p("/proj/b.sv"), p("/proj/a.sv")],
            &[],
            |p| texts.get(p).cloned(),
        );
        assert_eq!(
            got,
            vec![p("/proj/a.sv"), p("/proj/b.sv"), p("/proj/shared.sv")]
        );
    }

    /// `expand_includes` honours `include_dirs` when the rel path isn't
    /// next to the current file.
    #[test]
    fn expand_includes_uses_include_dirs() {
        let texts: HashMap<PathBuf, String> = HashMap::from([
            (p("/proj/main.sv"), "`include \"hdr.sv\"".into()),
            (p("/uvm/src/hdr.sv"), "// leaf".into()),
        ]);
        let got = expand_includes(
            &[p("/proj/main.sv")],
            &[p("/uvm/src")],
            |p| texts.get(p).cloned(),
        );
        assert_eq!(got, vec![p("/proj/main.sv"), p("/uvm/src/hdr.sv")]);
    }
}
