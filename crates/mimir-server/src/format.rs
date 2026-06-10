//! LSP document formatting via `verible-verilog-format`.
//!
//! [`invoke_verible`] is the single public entry point: it takes the current
//! document text, an optional line range (1-based, inclusive), and a
//! [`FormatterConfig`], spawns `verible-verilog-format` with the right flags,
//! pipes the source through stdin, and returns the formatted text.
//!
//! Verible always outputs the entire file even when `--lines` restricts which
//! lines it rewrites; callers therefore replace the whole document with a
//! single `lsp_types::TextEdit`.
//!
//! ## Preprocessor handling
//!
//! `ifdef`/`ifndef` blocks that span statement boundaries (e.g. simulator
//! guards where only the `if`-condition is inside the block) cause Verible
//! parse errors.  [`wrap_ifdefs`] inserts Verible format-off/on pragmas
//! around every such block before passing the text to the formatter, and
//! [`strip_mimir_pragmas`] removes them from the output.  Header guards
//! (a top-level `` `ifndef X `` / `` `define X `` / `` `endif `` spanning
//! the whole file) are handled specially so only the guard lines themselves
//! are frozen; the file body is still formatted normally.
//!
//! ## Arg construction
//!
//! [`build_args`] is a pure function — no I/O — and is the primary target of
//! unit tests. The integration test (marked `#[ignore]`) needs the real
//! binary and is activated by `make verible && cargo test -- --include-ignored`.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tracing::{debug, instrument, warn};

use crate::project::FormatterConfig;

/// Hard timeout for a single `verible-verilog-format` invocation.
/// Real SV files rarely exceed a few thousand lines; 5 s is generous.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Unique tag embedded in injected pragmas so [`strip_mimir_pragmas`] can
/// remove them without touching any user-written format-off/on comments.
const MIMIR_TAG: &str = "// mimir:ifdef-wrap";

/// Pragma inserted before an `ifdef/`ifndef block.
pub const FORMAT_OFF_PRAGMA: &str = "/* verilog_format: off */  // mimir:ifdef-wrap";

/// Pragma inserted after the matching `endif (or header-guard `define).
pub const FORMAT_ON_PRAGMA: &str = "/* verilog_format: on */  // mimir:ifdef-wrap";

// --------------------------------------------------------------------------
// Preprocessor detection helpers (pure, unit-testable)
// --------------------------------------------------------------------------

/// True when `trimmed` is a `` `ifdef `` or `` `ifndef `` directive opener.
fn is_ifdef_opener(trimmed: &str) -> bool {
    let Some(rest) = trimmed
        .strip_prefix("`ifdef")
        .or_else(|| trimmed.strip_prefix("`ifndef"))
    else {
        return false;
    };
    rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace())
}

/// True when `trimmed` is a `` `endif `` directive (possibly with a trailing comment).
fn is_endif(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("`endif") else {
        return false;
    };
    rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == '/')
}

/// Extract the macro identifier from a `` `ifdef X `` or `` `ifndef X `` line.
fn ifdef_identifier(trimmed: &str) -> &str {
    let rest = trimmed
        .strip_prefix("`ifndef")
        .or_else(|| trimmed.strip_prefix("`ifdef"))
        .unwrap_or("")
        .trim_start();
    rest.split(|c: char| c.is_whitespace()).next().unwrap_or("")
}

/// True when `trimmed` is a `` `define IDENT `` for the given `ident`.
fn is_define_for_ident(trimmed: &str, ident: &str) -> bool {
    trimmed
        .strip_prefix("`define")
        .map(|rest| rest.split_whitespace().next() == Some(ident))
        .unwrap_or(false)
}

// --------------------------------------------------------------------------
// Block-finding
// --------------------------------------------------------------------------

/// A paired `` `ifdef ``/`` `ifndef `` … `` `endif `` range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IfdefBlock {
    /// 0-based index of the `` `ifdef ``/`` `ifndef `` line.
    open_line: usize,
    /// 0-based index of the matching `` `endif `` line.
    close_line: usize,
    /// Nesting depth when this block was opened (0 = top-level in file).
    depth_at_open: u32,
}

/// Scan `lines` and return all matched `` `ifdef``/`` `endif`` pairs.
///
/// Unmatched `` `ifdef`` openers (depth > 0 at EOF) are silently discarded.
/// Spurious `` `endif`` lines (stack empty) are ignored.
/// `` `elsif ``/`` `else `` are transparent: they don't affect depth.
fn find_blocks(lines: &[&str]) -> Vec<IfdefBlock> {
    let mut blocks = Vec::new();
    let mut stack: Vec<(usize, u32)> = Vec::new(); // (open_line, depth_at_open)
    let mut depth: u32 = 0;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_ifdef_opener(trimmed) {
            stack.push((i, depth));
            depth += 1;
        } else if is_endif(trimmed) && !stack.is_empty() {
            let (open_line, depth_at_open) = stack.pop().unwrap();
            depth = depth_at_open; // restore (handles malformed nesting gracefully)
            blocks.push(IfdefBlock { open_line, close_line: i, depth_at_open });
        }
    }
    blocks
}

// --------------------------------------------------------------------------
// Header-guard detection
// --------------------------------------------------------------------------

/// A header guard: a depth-0 `` `ifndef X `` / `` `define X `` pair that spans
/// most of the file, with the matching `` `endif `` at the very end.
#[derive(Debug, Clone)]
struct HeaderGuard {
    /// 0-based index of the `` `ifndef `` / `` `ifdef `` line.
    open_line: usize,
    /// 0-based index of the `` `define `` line immediately following.
    define_line: usize,
    /// 0-based index of the matching `` `endif `` line.
    close_line: usize,
}

/// Attempt to find a header guard among `blocks`.
///
/// Criteria:
/// * There is exactly one depth-0 block (or the first depth-0 block dominates).
/// * Its opener is within the first 10 non-blank / non-comment lines.
/// * The first significant line after the opener is a `` `define `` for the
///   same identifier.
/// * The matching `` `endif `` is within the last 5 non-blank lines of the file.
fn detect_header_guard(lines: &[&str], blocks: &[IfdefBlock]) -> Option<HeaderGuard> {
    let outer = blocks.iter().find(|b| b.depth_at_open == 0)?;

    let open_line = outer.open_line;
    let close_line = outer.close_line;

    // Opener must be near the top of the file.
    let non_blank_before = lines[..open_line]
        .iter()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//") && !t.starts_with("/*")
        })
        .count();
    if non_blank_before > 10 {
        return None;
    }

    // Derive the macro identifier.
    let ident = ifdef_identifier(lines[open_line].trim());
    if ident.is_empty() {
        return None;
    }

    // The first significant line after the opener must be `define IDENT.
    let define_line = lines[open_line + 1..]
        .iter()
        .enumerate()
        .find(|(_, l)| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//")
        })
        .and_then(|(offset, l)| {
            if is_define_for_ident(l.trim(), ident) {
                Some(open_line + 1 + offset)
            } else {
                None
            }
        })?;

    // Closer must be near the end of the file.
    let non_blank_after = lines[close_line + 1..]
        .iter()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//")
        })
        .count();
    if non_blank_after > 5 {
        return None;
    }

    Some(HeaderGuard { open_line, define_line, close_line })
}

// --------------------------------------------------------------------------
// Wrapping / stripping
// --------------------------------------------------------------------------

/// Output of [`wrap_ifdefs`]: the pragma-wrapped text plus the bookkeeping
/// needed to translate original line numbers into wrapped-text line numbers.
///
/// The translation matters for range formatting: Verible's `--lines` flag
/// must reference the text it is actually given (the wrapped text), so a
/// selection made against the original document has to be shifted past any
/// pragma lines inserted above it. See [`Self::wrapped_line`].
#[derive(Debug, Clone)]
pub struct WrappedSource {
    /// The wrapped text. Identical to the input when `has_ifdefs` is `false`.
    pub text: String,
    /// Whether any `` `ifdef``/`` `ifndef `` blocks were found and wrapped.
    pub has_ifdefs: bool,
    /// One entry per inserted pragma line, holding the 0-based *original*
    /// line index the pragma was inserted before. Sorted ascending; empty
    /// when `has_ifdefs` is `false`.
    inserted_before: Vec<usize>,
}

impl WrappedSource {
    /// A pass-through result: `text` is `source` unchanged, no pragmas.
    /// Used when ifdef wrapping is disabled by configuration.
    #[must_use]
    pub fn unchanged(source: &str) -> Self {
        Self {
            text: source.to_owned(),
            has_ifdefs: false,
            inserted_before: Vec::new(),
        }
    }

    /// Translate a 0-based line index in the *original* source to the
    /// matching 0-based line index in [`Self::text`].
    ///
    /// Every pragma inserted before (or at) the original line shifts it
    /// down by one. O(log n) via binary search on the sorted insert list.
    #[must_use]
    pub fn wrapped_line(&self, original_line: u32) -> u32 {
        let shift = self
            .inserted_before
            .partition_point(|&before| before <= original_line as usize);
        original_line + shift as u32
    }
}

/// Wrap `` `ifdef``/`` `ifndef `` blocks in `source` with Verible format-off/on
/// pragmas so the formatter leaves them verbatim.
///
/// * **Without a header guard**: every depth-0 block is fully wrapped.
/// * **With a header guard**: only the header guard lines themselves
///   (`` `ifndef ``/`` `define ``) and the closing `` `endif `` are frozen
///   as single-line wraps; the file body is exposed for normal formatting.
///   Every depth-1 block inside the guard body is fully wrapped.
///
/// Returns a [`WrappedSource`]; when its `has_ifdefs` is `false` the text
/// is the source unchanged.
pub fn wrap_ifdefs(source: &str) -> WrappedSource {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return WrappedSource::unchanged(source);
    }

    let blocks = find_blocks(&lines);
    if blocks.is_empty() {
        return WrappedSource::unchanged(source);
    }

    let header_guard = detect_header_guard(&lines, &blocks);

    // Depth at which blocks are considered "effective top-level" and need wrapping.
    let wrap_depth: u32 = if header_guard.is_some() { 1 } else { 0 };

    // Each entry: (line_index, is_prepend, pragma_str).
    // BTreeMap keeps line indices sorted so we iterate in order.
    let mut prepend: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    let mut append: BTreeMap<usize, Vec<&str>> = BTreeMap::new();

    // Fully wrap every block at effective top-level depth.
    for block in &blocks {
        if block.depth_at_open == wrap_depth {
            prepend.entry(block.open_line).or_default().push(FORMAT_OFF_PRAGMA);
            append.entry(block.close_line).or_default().push(FORMAT_ON_PRAGMA);
        }
    }

    // Header guard: split-wrap the opener pair and the closer individually.
    if let Some(hg) = &header_guard {
        // Freeze `ifndef + `define as a unit.
        prepend.entry(hg.open_line).or_default().insert(0, FORMAT_OFF_PRAGMA);
        append.entry(hg.define_line).or_default().push(FORMAT_ON_PRAGMA);
        // Freeze the closing `endif.
        prepend.entry(hg.close_line).or_default().push(FORMAT_OFF_PRAGMA);
        append.entry(hg.close_line).or_default().push(FORMAT_ON_PRAGMA);
    }

    // Reconstruct the source with pragmas spliced in, recording each
    // insertion point so callers can translate line numbers afterwards.
    // A prepend at line `i` sits before original line `i`; an append at
    // line `i` sits before original line `i + 1`. The loop runs in
    // ascending `i`, so the list comes out sorted.
    let mut inserted_before: Vec<usize> = Vec::new();
    let mut out = String::with_capacity(source.len() + blocks.len() * 80);
    for (i, line) in lines.iter().enumerate() {
        for pragma in prepend.get(&i).into_iter().flatten() {
            out.push_str(pragma);
            out.push('\n');
            inserted_before.push(i);
        }
        out.push_str(line);
        out.push('\n');
        for pragma in append.get(&i).into_iter().flatten() {
            out.push_str(pragma);
            out.push('\n');
            inserted_before.push(i + 1);
        }
    }

    // Restore the original trailing-newline behaviour.
    if !source.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }

    WrappedSource {
        text: out,
        has_ifdefs: true,
        inserted_before,
    }
}

/// Remove every line that contains [`MIMIR_TAG`] from `text`.
///
/// Only lines injected by [`wrap_ifdefs`] carry the tag, so this never
/// disturbs user-written `/* verilog_format: off */` comments.
pub fn strip_mimir_pragmas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if !line.contains(MIMIR_TAG) {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

// --------------------------------------------------------------------------
// Error type
// --------------------------------------------------------------------------

/// Everything that can go wrong when invoking `verible-verilog-format`.
#[derive(Debug, Error)]
pub enum FormatError {
    /// The binary could not be found or the process could not be spawned.
    #[error("could not spawn formatter `{binary}`: {source}")]
    BinaryNotFound {
        /// The binary name or path that was attempted.
        binary: String,
        /// The OS-level error.
        #[source]
        source: std::io::Error,
    },

    /// A pipe read/write failed after the process started.
    #[error("I/O error communicating with formatter: {0}")]
    Io(#[from] std::io::Error),

    /// The formatter did not finish within [`FORMAT_TIMEOUT`].
    #[error("formatter timed out after {secs}s")]
    Timeout {
        /// The configured timeout in whole seconds.
        secs: u64,
    },

    /// The formatter exited with a non-zero status.
    #[error("formatter exited with code {exit_code}: {stderr}")]
    VeribleFailed {
        /// The process exit code.
        exit_code: i32,
        /// Content of the process's stderr (trimmed).
        stderr: String,
    },
}

// --------------------------------------------------------------------------
// Argument construction (pure, unit-testable)
// --------------------------------------------------------------------------

/// Build the `verible-verilog-format` argument list from `config` and an
/// optional `lines` range (1-based inclusive, `None` = whole file).
///
/// Each `Option` field in [`FormatterConfig`] only contributes an argument
/// when `Some`; absent fields are skipped so Verible uses its own defaults.
pub fn build_args(config: &FormatterConfig, lines: Option<RangeInclusive<u32>>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    if let Some(v) = config.column_limit {
        args.push(format!("--column_limit={v}"));
    }
    if let Some(v) = config.indentation_spaces {
        args.push(format!("--indentation_spaces={v}"));
    }
    if let Some(v) = config.wrap_spaces {
        args.push(format!("--wrap_spaces={v}"));
    }
    if let Some(v) = config.try_wrap_long_lines {
        args.push(format!("--try_wrap_long_lines={v}"));
    }
    if let Some(ref v) = config.port_declarations_alignment {
        args.push(format!("--port_declarations_alignment={v}"));
    }
    if let Some(ref v) = config.assignment_statement_alignment {
        args.push(format!("--assignment_statement_alignment={v}"));
    }
    if let Some(ref v) = config.named_parameter_alignment {
        args.push(format!("--named_parameter_alignment={v}"));
    }
    if let Some(ref v) = config.named_port_alignment {
        args.push(format!("--named_port_alignment={v}"));
    }
    if let Some(ref v) = config.module_net_variable_alignment {
        args.push(format!("--module_net_variable_alignment={v}"));
    }
    if let Some(ref v) = config.formal_parameters_alignment {
        args.push(format!("--formal_parameters_alignment={v}"));
    }
    if let Some(ref v) = config.class_member_variable_alignment {
        args.push(format!("--class_member_variable_alignment={v}"));
    }
    if let Some(ref v) = config.struct_union_members_alignment {
        args.push(format!("--struct_union_members_alignment={v}"));
    }

    if let Some(range) = lines {
        args.push(format!("--lines={}-{}", range.start(), range.end()));
    }

    args.extend(config.extra_args.iter().cloned());

    // Tell Verible to read from stdin. The `-` convention is standard for
    // POSIX tools; Verible supports it alongside named files.
    args.push("-".to_owned());

    args
}

// --------------------------------------------------------------------------
// Invocation
// --------------------------------------------------------------------------

/// Invoke `verible-verilog-format`, pipe `source` through stdin, and return
/// the formatter's stdout as the formatted text.
///
/// `lines` is a 1-based inclusive range (matching Verible's `--lines` flag).
/// Pass `None` to format the entire file.
#[instrument(level = "debug", skip(config, source), fields(binary = %config.binary))]
pub async fn invoke_verible(
    config: &FormatterConfig,
    source: &str,
    lines: Option<RangeInclusive<u32>>,
) -> Result<String, FormatError> {
    let args = build_args(config, lines.clone());

    debug!(args = ?args, "spawning verible-verilog-format");

    let mut child = Command::new(&config.binary)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| FormatError::BinaryNotFound {
            binary: config.binary.clone(),
            source,
        })?;

    // Write source to stdin then close the pipe so Verible sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(source.as_bytes()).await?;
        // `stdin` drops here, closing the fd.
    }

    // Wait for completion with a hard timeout.
    let output = tokio::time::timeout(FORMAT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| FormatError::Timeout {
            secs: FORMAT_TIMEOUT.as_secs(),
        })?
        .map_err(FormatError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let exit_code = output.status.code().unwrap_or(-1);
        warn!(exit_code, stderr = %stderr, "verible-verilog-format failed");
        return Err(FormatError::VeribleFailed { exit_code, stderr });
    }

    // Verible's --failsafe_success (on by default) causes it to exit 0 and
    // echo the original text unchanged when it encounters parse errors — for
    // example, files with `ifdef/`endif guards that span task bodies. Log any
    // stderr warnings so the user can see why formatting was a no-op.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        warn!(
            stderr = %stderr,
            "verible-verilog-format exited 0 but reported parse warnings; \
             output may be unchanged (preprocessor guards often cause this)",
        );
    }

    let formatted = String::from_utf8_lossy(&output.stdout).into_owned();

    // When Verible can't format a region it echoes it verbatim.  If the
    // entire output matches the input the caller will produce a no-op edit;
    // returning the formatted string is still correct, but callers can check
    // for this case with `formatted == source`.
    debug!(
        input_bytes = source.len(),
        output_bytes = formatted.len(),
        unchanged = (formatted == source),
        "verible-verilog-format succeeded",
    );
    Ok(formatted)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::FormatterConfig;

    fn default_cfg() -> FormatterConfig {
        FormatterConfig::default()
    }

    /// Default config with no range produces only the stdin sentinel `-`.
    #[test]
    fn build_args_defaults_produce_only_stdin_sentinel() {
        let args = build_args(&default_cfg(), None);
        assert_eq!(args, vec!["-"]);
    }

    /// Numeric options appear as `--flag=value`.
    #[test]
    fn build_args_numeric_options() {
        let cfg = FormatterConfig {
            column_limit: Some(120),
            indentation_spaces: Some(4),
            wrap_spaces: Some(8),
            ..Default::default()
        };
        let args = build_args(&cfg, None);
        assert!(args.contains(&"--column_limit=120".to_owned()), "{args:?}");
        assert!(args.contains(&"--indentation_spaces=4".to_owned()), "{args:?}");
        assert!(args.contains(&"--wrap_spaces=8".to_owned()), "{args:?}");
    }

    /// Boolean option serialises as `--flag=true` / `--flag=false`.
    #[test]
    fn build_args_bool_option() {
        let cfg = FormatterConfig {
            try_wrap_long_lines: Some(true),
            ..Default::default()
        };
        let args = build_args(&cfg, None);
        assert!(
            args.contains(&"--try_wrap_long_lines=true".to_owned()),
            "{args:?}"
        );
    }

    /// Alignment string options appear as `--flag=value`.
    #[test]
    fn build_args_alignment_options() {
        let cfg = FormatterConfig {
            port_declarations_alignment: Some("align".to_owned()),
            named_port_alignment: Some("preserve".to_owned()),
            ..Default::default()
        };
        let args = build_args(&cfg, None);
        assert!(
            args.contains(&"--port_declarations_alignment=align".to_owned()),
            "{args:?}"
        );
        assert!(
            args.contains(&"--named_port_alignment=preserve".to_owned()),
            "{args:?}"
        );
    }

    /// `--lines=N-M` is appended before `extra_args` and `-`.
    #[test]
    fn build_args_line_range() {
        let args = build_args(&default_cfg(), Some(5..=10));
        // Should be ["--lines=5-10", "-"]
        assert_eq!(args[0], "--lines=5-10");
        assert_eq!(args.last().unwrap(), "-");
    }

    /// A single-line range produces `--lines=N-N`.
    #[test]
    fn build_args_single_line_range() {
        let args = build_args(&default_cfg(), Some(7..=7));
        assert_eq!(args[0], "--lines=7-7");
    }

    /// `extra_args` are appended verbatim before the stdin sentinel.
    #[test]
    fn build_args_extra_args_before_sentinel() {
        let cfg = FormatterConfig {
            extra_args: vec!["--expand_coverpoints".to_owned()],
            ..Default::default()
        };
        let args = build_args(&cfg, None);
        let sentinel_pos = args.iter().position(|a| a == "-").unwrap();
        let extra_pos = args.iter().position(|a| a == "--expand_coverpoints").unwrap();
        assert!(extra_pos < sentinel_pos, "extra_args must come before sentinel: {args:?}");
    }

    /// The stdin sentinel `-` is always the last argument.
    #[test]
    fn build_args_sentinel_is_last() {
        let cfg = FormatterConfig {
            column_limit: Some(80),
            extra_args: vec!["--failsafe_success=false".to_owned()],
            ..Default::default()
        };
        let args = build_args(&cfg, Some(1..=5));
        assert_eq!(args.last().unwrap(), "-");
    }

    // ------------------------------------------------------------------
    // find_blocks / detect_header_guard / wrap_ifdefs / strip tests
    // ------------------------------------------------------------------

    fn lines_of(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    /// No ifdef directives → empty block list.
    #[test]
    fn find_blocks_empty_on_plain_code() {
        let src = "module foo;\nwire x;\nendmodule\n";
        assert!(find_blocks(&lines_of(src)).is_empty());
    }

    /// A single flat ifdef/endif pair is found at depth 0.
    #[test]
    fn find_blocks_single_flat_block() {
        let src = "`ifdef VCS\nassign x = 1;\n`endif\n";
        let blocks = find_blocks(&lines_of(src));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].open_line, 0);
        assert_eq!(blocks[0].close_line, 2);
        assert_eq!(blocks[0].depth_at_open, 0);
    }

    /// Nested ifdefs produce two blocks with correct depths.
    #[test]
    fn find_blocks_nested_depths() {
        let src = "`ifdef A\n`ifdef B\ncode;\n`endif\n`endif\n";
        let blocks = find_blocks(&lines_of(src));
        assert_eq!(blocks.len(), 2);
        // Inner block closes first (depth 1).
        let inner = blocks.iter().find(|b| b.depth_at_open == 1).unwrap();
        let outer = blocks.iter().find(|b| b.depth_at_open == 0).unwrap();
        assert_eq!(inner.open_line, 1);
        assert_eq!(inner.close_line, 3);
        assert_eq!(outer.open_line, 0);
        assert_eq!(outer.close_line, 4);
    }

    /// `elsif and `else don't affect depth counting.
    #[test]
    fn find_blocks_elsif_is_transparent() {
        let src = "`ifdef A\nassign x = 0;\n`elsif B\nassign x = 1;\n`else\nassign x = 2;\n`endif\n";
        let blocks = find_blocks(&lines_of(src));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].depth_at_open, 0);
    }

    /// A classic header guard is detected.
    #[test]
    fn detect_header_guard_recognises_classic_guard() {
        let src = "`ifndef MY_PKG_SV\n`define MY_PKG_SV\n// body\nmodule foo; endmodule\n`endif\n";
        let ls = lines_of(src);
        let blocks = find_blocks(&ls);
        let hg = detect_header_guard(&ls, &blocks);
        assert!(hg.is_some(), "should detect header guard");
        let hg = hg.unwrap();
        assert_eq!(hg.open_line, 0);
        assert_eq!(hg.define_line, 1);
        assert_eq!(hg.close_line, 4);
    }

    /// Plain `ifdef (not a guard) is not detected as a header guard.
    #[test]
    fn detect_header_guard_ignores_plain_ifdef() {
        let src = "module foo;\n`ifdef SIMULATION\nassert(x);\n`endif\nendmodule\n";
        let ls = lines_of(src);
        let blocks = find_blocks(&ls);
        let hg = detect_header_guard(&ls, &blocks);
        assert!(hg.is_none(), "should not detect header guard in mid-file ifdef");
    }

    /// Source without ifdefs is returned unchanged, flag = false.
    #[test]
    fn wrap_ifdefs_no_ifdefs_returns_unchanged() {
        let src = "module foo;\nwire x;\nendmodule\n";
        let w = wrap_ifdefs(src);
        assert!(!w.has_ifdefs);
        assert_eq!(w.text, src);
    }

    /// A plain depth-0 ifdef block is fully wrapped.
    #[test]
    fn wrap_ifdefs_wraps_depth0_block() {
        let src = "`ifdef VCS\nassign x = 1;\n`endif\n";
        let w = wrap_ifdefs(src);
        let (out, flag) = (w.text, w.has_ifdefs);
        assert!(flag, "should report has_ifdefs=true");
        assert!(
            out.contains(FORMAT_OFF_PRAGMA),
            "should contain format-off pragma:\n{out}"
        );
        assert!(
            out.contains(FORMAT_ON_PRAGMA),
            "should contain format-on pragma:\n{out}"
        );
        // Original ifdef/endif lines must still be present.
        assert!(out.contains("`ifdef VCS"), "original ifdef missing:\n{out}");
        assert!(out.contains("`endif"), "original endif missing:\n{out}");
    }

    /// Header guard: body lines are NOT wrapped; only guard lines are frozen.
    #[test]
    fn wrap_ifdefs_header_guard_exposes_body() {
        let src = concat!(
            "`ifndef MY_HDR\n",
            "`define MY_HDR\n",
            "module foo;\n",
            "wire x;\n",
            "endmodule\n",
            "`endif\n",
        );
        let w = wrap_ifdefs(src);
        let (out, flag) = (w.text, w.has_ifdefs);
        assert!(flag);
        // Body code must NOT be inside format-off (the pragma before `ifndef
        // and format-on after `define means body is exposed).
        // Check ordering: FORMAT_OFF appears before `ifndef, FORMAT_ON appears
        // after `define, then body, then FORMAT_OFF again before `endif.
        let off_positions: Vec<_> = out.match_indices(FORMAT_OFF_PRAGMA).map(|(i, _)| i).collect();
        let on_positions: Vec<_> = out.match_indices(FORMAT_ON_PRAGMA).map(|(i, _)| i).collect();
        // There should be exactly 2 format-off pragmas (before `ifndef and before `endif)
        // and 2 format-on pragmas (after `define and after `endif).
        assert_eq!(off_positions.len(), 2, "expected 2 format-off pragmas:\n{out}");
        assert_eq!(on_positions.len(), 2, "expected 2 format-on pragmas:\n{out}");
        // The body text (module foo) should appear between the first format-on and
        // the second format-off.
        let body_pos = out.find("module foo").unwrap();
        assert!(
            on_positions[0] < body_pos && body_pos < off_positions[1],
            "body should be between first format-on and second format-off:\n{out}"
        );
    }

    /// strip_mimir_pragmas removes only injected lines and preserves the rest.
    #[test]
    fn strip_mimir_pragmas_removes_injected_lines() {
        let src = concat!(
            "/* verilog_format: off */  // mimir:ifdef-wrap\n",
            "`ifdef VCS\n",
            "assign x = 1;\n",
            "`endif\n",
            "/* verilog_format: on */  // mimir:ifdef-wrap\n",
        );
        let stripped = strip_mimir_pragmas(src);
        assert!(!stripped.contains(MIMIR_TAG), "mimir tag should be gone:\n{stripped}");
        assert!(stripped.contains("`ifdef VCS"), "ifdef line should remain:\n{stripped}");
        assert!(stripped.contains("assign x = 1;"), "code line should remain:\n{stripped}");
    }

    /// strip_mimir_pragmas preserves user-written format-off comments that
    /// don't carry the mimir tag.
    #[test]
    fn strip_mimir_pragmas_preserves_user_pragmas() {
        let src = "/* verilog_format: off */\nassign x = 1;\n/* verilog_format: on */\n";
        let stripped = strip_mimir_pragmas(src);
        assert_eq!(stripped, src, "user pragmas should be untouched:\n{stripped}");
    }

    /// Round-trip: wrapping then stripping returns the original source.
    #[test]
    fn wrap_then_strip_is_identity() {
        let src = concat!(
            "`ifndef MY_HDR\n",
            "`define MY_HDR\n",
            "module foo;\n",
            "`ifdef VCS\n",
            "assign dbg = 1;\n",
            "`endif\n",
            "endmodule\n",
            "`endif\n",
        );
        let wrapped = wrap_ifdefs(src);
        let restored = strip_mimir_pragmas(&wrapped.text);
        assert_eq!(restored, src, "round-trip should be identity:\n{restored}");
    }

    // ------------------------------------------------------------------
    // WrappedSource line-mapping tests (regression for the range-format
    // `--lines` offset bug: original-source line numbers must be shifted
    // past inserted pragma lines before being handed to Verible).
    // ------------------------------------------------------------------

    /// With no ifdefs the mapping is the identity.
    #[test]
    fn wrapped_line_identity_without_ifdefs() {
        let w = wrap_ifdefs("module foo;\nwire x;\nendmodule\n");
        for line in 0..5 {
            assert_eq!(w.wrapped_line(line), line);
        }
        let pass = WrappedSource::unchanged("module foo;\nendmodule\n");
        assert_eq!(pass.wrapped_line(3), 3);
    }

    /// Lines below a fully-wrapped depth-0 block shift down by the two
    /// pragmas inserted around it; lines above it don't move.
    #[test]
    fn wrapped_line_shifts_past_wrapped_block() {
        // 0: module foo;
        // 1: `ifdef VCS      <- format-off prepended before this line
        // 2: assign d = 1;
        // 3: `endif          <- format-on appended after this line
        // 4: wire x;
        // 5: endmodule
        let src = "module foo;\n`ifdef VCS\nassign d = 1;\n`endif\nwire x;\nendmodule\n";
        let w = wrap_ifdefs(src);
        assert!(w.has_ifdefs);
        // Line 0 is before any insertion.
        assert_eq!(w.wrapped_line(0), 0);
        // Line 1 has one pragma before it.
        assert_eq!(w.wrapped_line(1), 2);
        // Lines past the `endif have both pragmas above them.
        assert_eq!(w.wrapped_line(4), 6);
        assert_eq!(w.wrapped_line(5), 7);
        // Cross-check against the actual wrapped text: the mapped lines
        // must hold the same content as the original lines.
        let orig: Vec<&str> = src.lines().collect();
        let wrapped: Vec<&str> = w.text.lines().collect();
        for (i, line) in orig.iter().enumerate() {
            assert_eq!(
                wrapped[w.wrapped_line(i as u32) as usize],
                *line,
                "original line {i} should map to identical wrapped line",
            );
        }
    }

    /// Header-guard wrapping inserts four pragmas; every original line must
    /// still map onto its identical counterpart in the wrapped text.
    #[test]
    fn wrapped_line_maps_through_header_guard() {
        let src = concat!(
            "`ifndef MY_HDR\n",
            "`define MY_HDR\n",
            "module foo;\n",
            "`ifdef VCS\n",
            "assign dbg = 1;\n",
            "`endif\n",
            "endmodule\n",
            "`endif\n",
        );
        let w = wrap_ifdefs(src);
        assert!(w.has_ifdefs);
        let orig: Vec<&str> = src.lines().collect();
        let wrapped: Vec<&str> = w.text.lines().collect();
        for (i, line) in orig.iter().enumerate() {
            assert_eq!(
                wrapped[w.wrapped_line(i as u32) as usize],
                *line,
                "original line {i} should map to identical wrapped line",
            );
        }
    }

    /// Integration test: requires a real `verible-verilog-format` binary.
    ///
    /// Run with:
    /// ```bash
    /// make verible
    /// VERIBLE_BIN=tools/verible/bin/verible-verilog-format \
    ///   cargo test -p mimir-server -- --include-ignored format::tests::integration
    /// ```
    #[tokio::test]
    #[ignore = "requires verible-verilog-format binary (run `make verible` first)"]
    async fn integration_formats_simple_module() {
        // Honour an override path so `make verible` + CI can set the binary.
        let binary = std::env::var("VERIBLE_BIN")
            .unwrap_or_else(|_| "verible-verilog-format".to_owned());
        let cfg = FormatterConfig {
            binary,
            indentation_spaces: Some(2),
            ..Default::default()
        };
        let source = "module foo(input logic clk);always_ff @(posedge clk) begin end endmodule\n";
        let result = invoke_verible(&cfg, source, None).await;
        let formatted = result.expect("verible should succeed on valid SV");
        // Verible should have split the always_ff onto its own line.
        assert!(
            formatted.contains("always_ff"),
            "formatted output missing always_ff:\n{formatted}"
        );
        assert!(
            formatted.contains('\n'),
            "formatted output should be multi-line:\n{formatted}"
        );
    }

    /// Integration test: range formatting only touches the specified lines.
    #[tokio::test]
    #[ignore = "requires verible-verilog-format binary (run `make verible` first)"]
    async fn integration_range_formats_partial_file() {
        let binary = std::env::var("VERIBLE_BIN")
            .unwrap_or_else(|_| "verible-verilog-format".to_owned());
        let cfg = FormatterConfig {
            binary,
            ..Default::default()
        };
        // A two-module file; ask Verible to only format lines 1-3.
        let source = "module a;\nwire x;\nendmodule\nmodule b;\nwire  y;\nendmodule\n";
        let result = invoke_verible(&cfg, source, Some(1..=3)).await;
        assert!(result.is_ok(), "range format should succeed: {result:?}");
    }
}
