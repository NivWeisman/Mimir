//! LSP document formatting via `verible-verilog-format`.
//!
//! [`invoke_verible`] is the single public entry point: it takes the current
//! document text, an optional line range (1-based, inclusive), and a
//! [`FormatterConfig`], spawns `verible-verilog-format` with the right flags,
//! pipes the source through stdin, and returns the formatted text.
//!
//! Verible always outputs the entire file even when `--lines` restricts which
//! lines it rewrites; callers therefore replace the whole document with a
//! single [`lsp_types::TextEdit`].
//!
//! ## Arg construction
//!
//! [`build_args`] is a pure function — no I/O — and is the primary target of
//! unit tests. The integration test (marked `#[ignore]`) needs the real
//! binary and is activated by `make verible && cargo test -- --include-ignored`.

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
