//! Project configuration for slang elaboration and formatter integration.
//!
//! tree-sitter happily parses one file in isolation; slang can't. UVM
//! testbenches `` `include `` macros from `uvm_pkg`, pull `+incdir+`
//! directories the editor knows nothing about, and rely on `+define+`s
//! that vary per simulator/run. So before the [`crate::backend::Backend`]
//! can usefully call slang, it needs to know:
//!
//! * what set of source files makes up the compilation unit,
//! * what include search paths to use,
//! * what preprocessor macros to predefine, and
//! * (optionally) which top-level module/program to elaborate from.
//!
//! Two file formats describe that:
//!
//! * **`.mimir.toml`** — a small mimir-specific config at the workspace
//!   root. Lists include dirs / defines / a top, plus an optional path to
//!   a filelist. Discovered by walking up from `initialize`'s `rootUri`.
//! * **`.f` filelists** — the verification-industry standard. Used by
//!   every commercial simulator (VCS, Xcelium, Questa) and Verilator.
//!   Tokenization and path resolution live in [`crate::filelist`].
//!
//! Both feed a single [`ResolvedProject`] which Stage 3 reads to build an
//! [`mimir_slang::ElaborateParams`] envelope.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use mimir_slang::MacroDefine;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info};

use crate::filelist::{absolutise, expand_env_vars, expand_filelist_to_parts, parse_define};

/// Maximum number of parent directories to walk searching for
/// `.mimir.toml`. Bounds the cost of opening a single file from `/`.
const DISCOVER_MAX_PARENTS: usize = 8;

/// Default debounce window before slang re-elaborates after the user
/// stops typing. Stage 3 reads this. 350 ms is comfortable on a laptop
/// (slang elaboration of a UVM testbench is typically 100–300 ms once
/// the OS file cache is warm) without feeling laggy in the editor.
const DEFAULT_DEBOUNCE_MS: u64 = 350;

// --------------------------------------------------------------------------
// Errors
// --------------------------------------------------------------------------

/// Anything that can go wrong loading or expanding project config.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// `read_to_string` failed on a `.mimir.toml` or `.f` we tried to open.
    #[error("could not read project file `{path}`: {source}")]
    Io {
        /// The path we tried to read.
        path: PathBuf,
        /// The OS-level error.
        #[source]
        source: std::io::Error,
    },

    /// `.mimir.toml` parsed as TOML but didn't match our schema, or wasn't
    /// valid TOML at all.
    #[error("could not parse `{path}`: {source}")]
    Toml {
        /// The TOML file we failed to decode.
        path: PathBuf,
        /// The decoder's error.
        #[source]
        source: toml::de::Error,
    },

    /// A chain of `-f` directives nested too deeply. Almost always a
    /// misconfigured filelist; bail rather than blow the stack.
    #[error("filelist recursion too deep at `{path}` (limit {limit})")]
    FilelistTooDeep {
        /// The filelist that pushed us over the limit.
        path: PathBuf,
        /// The configured limit (i.e. [`FILELIST_MAX_DEPTH`]).
        limit: usize,
    },

    /// A filelist (transitively) `-f`-includes itself. Also almost
    /// certainly a misconfiguration.
    #[error("filelist `{path}` includes itself (cycle)")]
    FilelistCycle {
        /// The filelist that closed the cycle.
        path: PathBuf,
    },
}

// --------------------------------------------------------------------------
// Raw .mimir.toml schema
// --------------------------------------------------------------------------

/// Top-level `.mimir.toml` schema. Anything not present falls back to the
/// `Default` impl — a fully empty file is valid and means "tree-sitter only
/// for now, but please look here when slang gets enabled."
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Slang-specific settings. The single `[slang]` table keeps the door
    /// open for future top-level tables (e.g. `[lint]`, `[uvm]`).
    #[serde(default)]
    pub slang: SlangConfig,

    /// Formatter settings — controls how `textDocument/formatting` and
    /// `textDocument/rangeFormatting` invoke `verible-verilog-format`.
    /// See [`FormatterConfig`] and `docs/formatter.md` for the full
    /// option reference.
    ///
    /// ```toml
    /// [formatter]
    /// binary            = "/opt/verible/bin/verible-verilog-format"
    /// column_limit      = 120
    /// indentation_spaces = 4
    /// ```
    #[serde(default)]
    pub formatter: FormatterConfig,

    /// Extra environment variables for this workspace. Entries here are
    /// checked first when expanding `${VAR}` in filelist tokens and when
    /// looking up `MIMIR_SLANG_PATH`; the process environment provides
    /// fallbacks (so CI can still override by setting the real env var).
    ///
    /// Values may themselves reference other `[env]` keys or process env
    /// vars using the same `${VAR}` syntax (one level of indirection):
    ///
    /// ```toml
    /// [env]
    /// PROJECT_ROOT     = "/work/my_project"
    /// MIMIR_SLANG_PATH = "${PROJECT_ROOT}/bin/mimir-slang-sidecar"
    /// ```
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Optional per-feature on/off toggles. Lets a workspace turn off
    /// specific LSP-side helpers when they're unwanted (e.g. semantic
    /// tokens, format-spec sub-coloring inside strings, keyword hover
    /// help). All flags default to `true` — an empty `[features]`
    /// table is equivalent to omitting it.
    ///
    /// ```toml
    /// [features]
    /// semantic_tokens = false           # turn off LSP semantic highlighting entirely
    /// format_specs_in_strings = false   # whole-string color instead of per-`%fmt`
    /// keyword_hover = false             # no popup on `always_ff` / `$display` / …
    /// formatting    = false             # disable LSP formatting even if verible is present
    /// ```
    #[serde(default)]
    pub features: FeatureToggles,

    /// Inlay-hint display settings. Controls how call-site argument hints are
    /// labelled for methods, functions, and tasks.
    ///
    /// ```toml
    /// [inlay_hints]
    /// method_hint = "name+type"  # "name" (default) | "type" | "name+type"
    /// ```
    #[serde(default)]
    pub inlay_hints: InlayHintsConfig,

    /// Path-based diagnostic demote / ignore rules. Lets a workspace quiet
    /// diagnostics for vendored code it can't fix (UVM, third-party IP)
    /// without losing them entirely.
    ///
    /// ```toml
    /// [diagnostics]
    /// demote_paths    = ["uvm-1.2"]   # substring-match the file path
    /// demote_severity = "hint"        # cap matching diags at this severity
    /// ignore_paths    = []            # drop matching diags entirely
    /// ```
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,

    /// CodeLens settings — controls the "overrides Base::method" lens.
    ///
    /// ```toml
    /// [code_lens]
    /// overrides = "uvm"   # "uvm" (default) | "all" | "none"
    /// ```
    #[serde(default)]
    pub code_lens: CodeLensConfig,
}

/// `[diagnostics]` section of `.mimir.toml`. Controls per-path demote/ignore
/// of slang elaboration diagnostics — see [`crate::diag_policy`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// File paths (matched as substrings) whose diagnostics are capped at
    /// [`Self::demote_severity`]. E.g. `["uvm-1.2"]` quiets the whole UVM
    /// library.
    #[serde(default)]
    pub demote_paths: Vec<String>,

    /// Severity floor applied to files matching [`Self::demote_paths`].
    /// One of `error` / `warning` / `information` / `hint` (default `hint`).
    /// An unrecognised value logs a warning and falls back to `hint`.
    #[serde(default = "default_demote_severity")]
    pub demote_severity: String,

    /// File paths (matched as substrings) whose diagnostics are dropped
    /// entirely. Takes precedence over `demote_paths`.
    #[serde(default)]
    pub ignore_paths: Vec<String>,

    /// Enable the UVM "phase override forgets `super.<phase>()`" lint
    /// (a tree-sitter check, independent of slang). Default `true`.
    #[serde(default = "default_true")]
    pub uvm_phase_super_call: bool,

    /// Severity for the missing-`super` diagnostic. One of
    /// `error` / `warning` / `information` / `hint` (default `warning`).
    /// An unrecognised value falls back to `warning`.
    #[serde(default = "default_uvm_phase_super_severity")]
    pub uvm_phase_super_severity: String,

    /// Phase method names the missing-`super` check applies to. Defaults to
    /// the UVM common phases ([`mimir_syntax::uvm::DEFAULT_UVM_PHASES`]).
    #[serde(default = "default_uvm_phases")]
    pub uvm_phases: Vec<String>,
}

fn default_demote_severity() -> String {
    "hint".to_string()
}

fn default_uvm_phase_super_severity() -> String {
    "warning".to_string()
}

fn default_uvm_phases() -> Vec<String> {
    mimir_syntax::uvm::DEFAULT_UVM_PHASES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            demote_paths: Vec::new(),
            demote_severity: default_demote_severity(),
            ignore_paths: Vec::new(),
            uvm_phase_super_call: true,
            uvm_phase_super_severity: default_uvm_phase_super_severity(),
            uvm_phases: default_uvm_phases(),
        }
    }
}

/// Resolved UVM-lint settings, derived from [`DiagnosticsConfig`]. Kept
/// separate from [`crate::diag_policy::DiagnosticPolicy`] (which is purely
/// path-based demote/ignore of *slang* diagnostics) because UVM lint is its
/// own concern: a set of tree-sitter checks with their own severity.
#[derive(Debug, Clone)]
pub struct UvmLintConfig {
    /// Whether the missing-`super.<phase>()` check runs.
    pub phase_super_call: bool,
    /// Severity emitted for the missing-`super` diagnostic.
    pub phase_super_severity: mimir_syntax::DiagnosticSeverity,
    /// Phase method names the check applies to.
    pub phases: Vec<String>,
}

impl Default for UvmLintConfig {
    fn default() -> Self {
        Self {
            phase_super_call: true,
            phase_super_severity: mimir_syntax::DiagnosticSeverity::Warning,
            phases: default_uvm_phases(),
        }
    }
}

impl UvmLintConfig {
    /// Build the resolved form from the raw `[diagnostics]` table.
    fn from_config(cfg: &DiagnosticsConfig) -> Self {
        Self {
            phase_super_call: cfg.uvm_phase_super_call,
            phase_super_severity: parse_severity(&cfg.uvm_phase_super_severity),
            phases: cfg.uvm_phases.clone(),
        }
    }
}

/// Parse a severity string into [`mimir_syntax::DiagnosticSeverity`],
/// falling back to `Warning` on an unrecognised value.
fn parse_severity(s: &str) -> mimir_syntax::DiagnosticSeverity {
    use mimir_syntax::DiagnosticSeverity as S;
    match s.to_ascii_lowercase().as_str() {
        "error" => S::Error,
        "information" | "info" => S::Information,
        "hint" => S::Hint,
        _ => S::Warning,
    }
}

/// `[features]` section of `.mimir.toml`. Each field gates one
/// LSP-side helper; `Default::default()` returns "every feature on",
/// so existing projects that don't yet have the table pick up the
/// same behaviour they had before this section existed.
///
/// Toggles are honoured at *handler* time, not at `initialize`-time
/// capability-registration time — that way editing `.mimir.toml` to
/// flip a flag takes effect on the next request after re-hydration,
/// without needing the client to renegotiate capabilities.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureToggles {
    /// Master switch for `textDocument/semanticTokens` (full + range).
    /// When `false`, handlers return `None` so the client falls back
    /// to its TextMate grammar.
    #[serde(default = "default_true")]
    pub semantic_tokens: bool,

    /// Within string literals, emit a separate `regexp`-classified
    /// sub-token for each `%`-format specifier (`%0d`, `%h`, `%s`, …)
    /// so themes can color them distinctly from the surrounding
    /// string body. When `false`, each `string_literal` emits one
    /// whole-string token (the pre-feature behaviour). Has no effect
    /// when `semantic_tokens` is `false`.
    #[serde(default = "default_true")]
    pub format_specs_in_strings: bool,

    /// Keyword / system-task hover help fallback. When `false`,
    /// hovering on `always_ff` / `$display` / … returns no popup
    /// (matches the pre-feature behaviour).
    #[serde(default = "default_true")]
    pub keyword_hover: bool,

    /// LSP document formatting via `verible-verilog-format`. When `false`,
    /// mimir does not advertise `formattingProvider` or
    /// `rangeFormattingProvider` in `ServerCapabilities`, so the client
    /// never sends formatting requests. Useful when the user already runs a
    /// formatter through a different channel (e.g. conform.nvim, pre-commit)
    /// and wants to prevent double-formatting.
    #[serde(default = "default_true")]
    pub formatting: bool,
}

fn default_true() -> bool {
    true
}

impl Default for FeatureToggles {
    fn default() -> Self {
        Self {
            semantic_tokens: true,
            format_specs_in_strings: true,
            keyword_hover: true,
            formatting: true,
        }
    }
}

/// `[inlay_hints]` section of `.mimir.toml`. Controls what information is
/// displayed in call-site inlay hints for methods, functions, and tasks.
///
/// ```toml
/// [inlay_hints]
/// method_hint = "name+type"  # "name" (default) | "type" | "name+type"
/// ```
///
/// Macro call hints always show the parameter name only, regardless of this
/// setting — macros are a preprocessor construct and their parameter types
/// are always `text`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlayHintsConfig {
    /// Label format for method / function / task call inlay hints.
    ///
    /// | Value | Label example |
    /// |-------|---------------|
    /// | `"name"` (default) | `a ` |
    /// | `"type"` | `int ` |
    /// | `"name+type"` | `a: int ` |
    #[serde(default = "default_method_hint")]
    pub method_hint: String,
}

fn default_method_hint() -> String {
    "name".to_owned()
}

impl Default for InlayHintsConfig {
    fn default() -> Self {
        Self { method_hint: default_method_hint() }
    }
}

/// `[code_lens]` section of `.mimir.toml`. Controls the "overrides
/// Base::method" CodeLens.
///
/// ```toml
/// [code_lens]
/// overrides = "uvm"   # "uvm" (default) | "all" | "none"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeLensConfig {
    /// Which method overrides get a lens: `"uvm"` (UVM phase methods only,
    /// the default), `"all"` (every override), or `"none"` (disabled).
    #[serde(default = "default_code_lens_overrides")]
    pub overrides: String,
}

fn default_code_lens_overrides() -> String {
    "uvm".to_owned()
}

impl Default for CodeLensConfig {
    fn default() -> Self {
        Self { overrides: default_code_lens_overrides() }
    }
}

/// `[slang]` section of `.mimir.toml`.
///
/// The `Default` impl is hand-written rather than derived because we want
/// `debounce_ms` to default to [`DEFAULT_DEBOUNCE_MS`] both when the
/// *field* is missing (handled by `#[serde(default = "...")]`) **and**
/// when the whole `[slang]` table is missing from the TOML (which calls
/// `Default::default()` directly, bypassing serde's field default).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlangConfig {
    /// Path to a `.f` filelist, relative to the `.mimir.toml`. When
    /// present, [`ResolvedProject::load`] expands it and merges the
    /// result into `files` / `include_dirs` / `defines`.
    #[serde(default)]
    pub filelist: Option<PathBuf>,

    /// Source files to compile, listed directly in the TOML without a
    /// separate `.f` filelist. Relative paths are resolved against the
    /// `.mimir.toml`'s directory; `${VAR}` is expanded the same way as
    /// in filelist tokens.
    ///
    /// Inline entries are prepended before any filelist entries so you
    /// can mix a shared team filelist with per-workspace additions:
    ///
    /// ```toml
    /// [slang]
    /// files    = ["tb/my_extra_tb.sv", "${VERIF_HOME}/stubs/axi_stub.sv"]
    /// filelist = "sim/project.f"
    /// ```
    #[serde(default)]
    pub files: Vec<PathBuf>,

    /// Extra include search paths. Relative entries are resolved against
    /// the `.mimir.toml`'s directory.
    #[serde(default)]
    pub include_dirs: Vec<PathBuf>,

    /// Extra `+define+`s. Each entry is either `"NAME"` (defined to empty)
    /// or `"NAME=VALUE"`. Same syntax simulators take on the command line.
    #[serde(default)]
    pub defines: Vec<String>,

    /// Optional top module/program. When `None`, slang elaborates every
    /// top-level it finds — useful for "lint the whole project" mode.
    #[serde(default)]
    pub top: Option<String>,

    /// Quiet time (in milliseconds) before slang re-elaborates after the
    /// user stops editing. Read by Stage 3.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,

    /// Long-tail libslang flags parsed by the sidecar's `slang::driver::Driver`.
    /// Use for options that don't have a dedicated TOML key —
    /// `--allow-use-before-declare`, `--ignore-unknown-modules`, etc. For
    /// `--single-unit` and `--timescale` use the typed fields below; on
    /// conflict the typed field wins.
    ///
    /// ```toml
    /// [slang]
    /// extra_args = ["--allow-use-before-declare"]
    /// ```
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// When `true`, all `is_compilation_unit: true` files are parsed into a
    /// single shared compilation unit so `` `define `` macros leak across
    /// files in the order they were given. Mirrors slang's `--single-unit`
    /// CLI flag.
    ///
    /// This is the right knob for UVM-style flows where headers like
    /// `uvm_macros.svh` are included once and the macros are expected to
    /// be visible to every later file. Without it, each file is its own
    /// preprocessor scope and you get cascading `UnknownDirective` errors
    /// on macros defined in an earlier file.
    ///
    /// Default `false` preserves slang's per-file behaviour.
    #[serde(default)]
    pub single_unit: bool,

    /// Default timescale applied to design elements that don't declare
    /// their own (e.g. `"1ns/1ps"`). Parsed by slang's
    /// `TimeScale::fromString`; invalid strings are logged at the sidecar
    /// and dropped — never a hard error. Wins over a `--timescale` entry
    /// in `extra_args`.
    #[serde(default)]
    pub timescale: Option<String>,
}

fn default_debounce_ms() -> u64 {
    DEFAULT_DEBOUNCE_MS
}

impl Default for SlangConfig {
    fn default() -> Self {
        Self {
            filelist: None,
            files: Vec::new(),
            include_dirs: Vec::new(),
            defines: Vec::new(),
            top: None,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            extra_args: Vec::new(),
            single_unit: false,
            timescale: None,
        }
    }
}

/// `[formatter]` section of `.mimir.toml`.
///
/// Controls how `textDocument/formatting` and `textDocument/rangeFormatting`
/// invoke `verible-verilog-format`. Every field is optional (`Option<T>`):
/// when absent the flag is not passed to Verible, which then uses its own
/// built-in default. Use `extra_args` for any flag not listed here.
///
/// Full option reference: `docs/formatter.md`.
///
/// ```toml
/// [formatter]
/// binary             = "verible-verilog-format"   # or an absolute path
/// column_limit       = 100
/// indentation_spaces = 2
/// wrap_spaces        = 4
/// try_wrap_long_lines = false
/// port_declarations_alignment = "flush-left"      # or "align" / "preserve"
/// extra_args = ["--expand_coverpoints"]
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatterConfig {
    /// Path or name of the `verible-verilog-format` binary. Resolved via
    /// `PATH` when just a name is given. Default: `"verible-verilog-format"`.
    #[serde(default = "FormatterConfig::default_binary")]
    pub binary: String,

    /// Maximum column width (`--column_limit`). Verible default: 100.
    #[serde(default)]
    pub column_limit: Option<u32>,

    /// Spaces per indentation level (`--indentation_spaces`). Verible default: 2.
    #[serde(default)]
    pub indentation_spaces: Option<u32>,

    /// Extra indentation spaces for wrapped line continuations
    /// (`--wrap_spaces`). Verible default: 4.
    #[serde(default)]
    pub wrap_spaces: Option<u32>,

    /// When `true`, actively break lines that exceed `column_limit`
    /// (`--try_wrap_long_lines`). Verible default: false.
    #[serde(default)]
    pub try_wrap_long_lines: Option<bool>,

    /// Column alignment for port declaration lists
    /// (`--port_declarations_alignment`). One of `"flush-left"`, `"align"`,
    /// or `"preserve"`. Verible default: `"flush-left"`.
    #[serde(default)]
    pub port_declarations_alignment: Option<String>,

    /// Column alignment for assignment statements (`=`, `<=`)
    /// (`--assignment_statement_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub assignment_statement_alignment: Option<String>,

    /// Column alignment for named parameter connections (`.param(value)`)
    /// (`--named_parameter_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub named_parameter_alignment: Option<String>,

    /// Column alignment for named port connections (`.port(wire)`)
    /// (`--named_port_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub named_port_alignment: Option<String>,

    /// Column alignment for net/variable declarations inside modules
    /// (`--module_net_variable_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub module_net_variable_alignment: Option<String>,

    /// Column alignment for formal parameter lists (`#(…)`)
    /// (`--formal_parameters_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub formal_parameters_alignment: Option<String>,

    /// Column alignment for class member variable declarations
    /// (`--class_member_variable_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub class_member_variable_alignment: Option<String>,

    /// Column alignment for `struct`/`union` member declarations
    /// (`--struct_union_members_alignment`). Verible default: `"flush-left"`.
    #[serde(default)]
    pub struct_union_members_alignment: Option<String>,

    /// Raw flags appended verbatim to every Verible invocation.
    /// Values are passed as-is; quote shell-special characters yourself.
    ///
    /// ```toml
    /// extra_args = ["--expand_coverpoints", "--failsafe_success=false"]
    /// ```
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// When `true` (default), mimir wraps `` `ifdef ``/`` `ifndef `` blocks
    /// with `/* verilog_format: off/on */` pragmas before invoking Verible so
    /// the formatter can still reformat surrounding code even when preprocessor
    /// guards span statement boundaries.  Set to `false` to pass source text
    /// to Verible unmodified (the old behaviour; may produce no-op formatting
    /// on files with simulator guards).
    #[serde(default = "default_true")]
    pub wrap_ifdefs: bool,
}

impl FormatterConfig {
    fn default_binary() -> String {
        "verible-verilog-format".to_owned()
    }
}

impl Default for FormatterConfig {
    fn default() -> Self {
        Self {
            binary: Self::default_binary(),
            column_limit: None,
            indentation_spaces: None,
            wrap_spaces: None,
            try_wrap_long_lines: None,
            port_declarations_alignment: None,
            assignment_statement_alignment: None,
            named_parameter_alignment: None,
            named_port_alignment: None,
            module_net_variable_alignment: None,
            formal_parameters_alignment: None,
            class_member_variable_alignment: None,
            struct_union_members_alignment: None,
            extra_args: Vec::new(),
            wrap_ifdefs: true,
        }
    }
}

// --------------------------------------------------------------------------
// Resolved project (post-filelist-expansion)
// --------------------------------------------------------------------------

/// A `.mimir.toml` plus its expanded filelist, with all paths absolutised
/// and `+define+`s parsed into structured [`MacroDefine`]s.
///
/// This is what Stage 3 consumes to build the `ElaborateParams` for each
/// elaborate call. The `files` list is the *on-disk* set; the call site is
/// expected to swap in any in-memory document text for files the user is
/// editing (so unsaved changes participate in elaboration).
//
// `dead_code` is silenced because Stage 3 hasn't started reading these
// fields yet; the struct is constructed and held by the backend but not
// otherwise consumed.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ResolvedProject {
    /// Directory the `.mimir.toml` lives in. Used as the base for any
    /// follow-up relative-path operations.
    pub root: PathBuf,
    /// Source files that make up the compilation unit, in declaration
    /// order. Duplicates are preserved — the simulator-style `.f` format
    /// allows declaring the same file twice and we don't second-guess.
    pub files: Vec<PathBuf>,
    /// `+incdir+` paths in slang's search order (left-to-right).
    pub include_dirs: Vec<PathBuf>,
    /// `+define+` macros.
    pub defines: Vec<MacroDefine>,
    /// Optional top module name.
    pub top: Option<String>,
    /// Stage-3 debounce window.
    pub debounce_ms: u64,
    /// Config-provided environment variables (from `[env]` in `.mimir.toml`).
    /// Consulted before the process environment when expanding `${VAR}` and
    /// when looking up `MIMIR_SLANG_PATH`. Empty when no `[env]` table is
    /// present.
    pub env: HashMap<String, String>,
    /// Per-feature on/off toggles (from `[features]` in `.mimir.toml`).
    /// Every flag defaults to `true`, so a project without the table
    /// behaves exactly as it did before the table existed.
    pub features: FeatureToggles,
    /// Formatter settings (from `[formatter]` in `.mimir.toml`).
    /// Passed through verbatim to [`crate::format`] at request time.
    pub formatter: FormatterConfig,
    /// Inlay-hint label mode for method / function / task calls. Parsed from
    /// `[inlay_hints] method_hint` in `.mimir.toml`; defaults to `Name`.
    pub method_hint_mode: mimir_syntax::MethodHintMode,
    /// Raw flags forwarded to [`ElaborateParams::extra_args`] on each compile.
    /// Set from `[slang] extra_args` in `.mimir.toml`.
    pub slang_extra_args: Vec<String>,
    /// When `true`, the sidecar parses every `is_compilation_unit: true`
    /// file into one shared compilation unit (see [`SlangConfig::single_unit`]).
    pub single_unit: bool,
    /// Default timescale string forwarded to the sidecar (see
    /// [`SlangConfig::timescale`]). `None` when the project doesn't set one.
    pub timescale: Option<String>,
    /// Path-based diagnostic demote/ignore policy (from `[diagnostics]` in
    /// `.mimir.toml`). Applied to slang diagnostics at publish time. The
    /// default is a no-op (publish everything unchanged).
    pub diagnostics: crate::diag_policy::DiagnosticPolicy,
    /// Resolved UVM-lint settings (also from `[diagnostics]`). Applied to
    /// tree-sitter diagnostics on every reparse.
    pub uvm_lint: UvmLintConfig,
    /// Resolved CodeLens override mode (from `[code_lens] overrides`).
    pub code_lens_overrides: crate::code_lens::OverrideLensMode,
}

impl ResolvedProject {
    /// Walk up from `start` looking for `.mimir.toml`. Stops after
    /// [`DISCOVER_MAX_PARENTS`] parent directories (so opening a single
    /// `.sv` file from `/tmp` doesn't traipse the whole filesystem).
    ///
    /// `Ok(None)` is the "no config" case — the server logs at info and
    /// leaves slang inactive.
    pub fn discover(start: &Path) -> Result<Option<Self>, ProjectError> {
        let mut current = Some(start);
        for _ in 0..DISCOVER_MAX_PARENTS {
            let dir = match current {
                Some(d) => d,
                None => break,
            };
            let candidate = dir.join(".mimir.toml");
            if candidate.is_file() {
                debug!(path = %candidate.display(), "found .mimir.toml");
                return Self::load(&candidate).map(Some);
            }
            current = dir.parent();
        }
        Ok(None)
    }

    /// Read a `.mimir.toml` from `path` and resolve it to a
    /// [`ResolvedProject`]. Logs the resolved input counts at info so
    /// "did my filelist actually load" is visible in the server's stderr.
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let text = fs::read_to_string(path).map_err(|source| ProjectError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: ProjectConfig = toml::from_str(&text).map_err(|source| ProjectError::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        let root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        // Expand ${VAR} within env values so entries can reference sibling
        // keys (e.g. `INC = "${ROOT}/include"` where ROOT is also in [env]).
        // Multi-pass: iterate until stable so chains like
        //   A="/base", B="${A}/sub", C="${B}/deep"
        // fully resolve rather than stopping at one level of indirection.
        // Capped at 16 passes to prevent runaway expansion on circular refs.
        let mut env = cfg.env.clone();
        for _ in 0..16 {
            let next: HashMap<String, String> = env
                .iter()
                .map(|(k, v)| (k.clone(), expand_env_vars(v, &env)))
                .collect();
            if next == env {
                break;
            }
            env = next;
        }

        for (k, v) in &env {
            debug!(key = %k, value = %v, "toml env var");
        }

        // Inline files listed directly in [slang] files = [...] come first.
        let mut files: Vec<PathBuf> = cfg
            .slang
            .files
            .iter()
            .map(|p| absolutise(&root, Path::new(&expand_env_vars(&p.to_string_lossy(), &env))))
            .collect();
        let mut include_dirs: Vec<PathBuf> = cfg
            .slang
            .include_dirs
            .iter()
            .map(|p| absolutise(&root, Path::new(&expand_env_vars(&p.to_string_lossy(), &env))))
            .collect();
        let mut defines: Vec<MacroDefine> = cfg
            .slang
            .defines
            .iter()
            .map(|s| parse_define(&expand_env_vars(s, &env)))
            .collect();

        if let Some(filelist) = cfg.slang.filelist.as_deref() {
            let expanded = expand_env_vars(&filelist.to_string_lossy(), &env);
            let absolute = absolutise(&root, Path::new(&expanded));
            let parts = expand_filelist_to_parts(&absolute, &root, &env)?;
            files.extend(parts.files);
            include_dirs.extend(parts.include_dirs);
            defines.extend(parts.defines);
        }

        info!(
            root = %root.display(),
            files = files.len(),
            include_dirs = include_dirs.len(),
            defines = defines.len(),
            env_vars = env.len(),
            top = ?cfg.slang.top,
            debounce_ms = cfg.slang.debounce_ms,
            extra_args = cfg.slang.extra_args.len(),
            "resolved project config",
        );

        let method_hint_mode = match cfg.inlay_hints.method_hint.as_str() {
            "type" => mimir_syntax::MethodHintMode::Type,
            "name+type" => mimir_syntax::MethodHintMode::NameAndType,
            _ => mimir_syntax::MethodHintMode::Name,
        };

        // Resolve UVM lint settings before the demote/ignore fields are
        // moved into `DiagnosticPolicy::from_config` below (they're distinct
        // fields, so the partial moves don't conflict).
        let uvm_lint = UvmLintConfig::from_config(&cfg.diagnostics);
        let code_lens_overrides =
            crate::code_lens::OverrideLensMode::from_config_str(&cfg.code_lens.overrides);

        Ok(Self {
            root,
            files,
            include_dirs,
            defines,
            top: cfg.slang.top,
            debounce_ms: cfg.slang.debounce_ms,
            env,
            features: cfg.features,
            formatter: cfg.formatter,
            method_hint_mode,
            slang_extra_args: cfg.slang.extra_args,
            single_unit: cfg.slang.single_unit,
            timescale: cfg.slang.timescale,
            diagnostics: crate::diag_policy::DiagnosticPolicy::from_config(
                cfg.diagnostics.demote_paths,
                &cfg.diagnostics.demote_severity,
                cfg.diagnostics.ignore_paths,
            ),
            uvm_lint,
            code_lens_overrides,
        })
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::tempdir;

    /// Empty `.mimir.toml` decodes to all defaults — the "I'll fill this
    /// in later" case must be valid.
    #[test]
    fn project_config_empty_decodes_to_defaults() {
        let cfg: ProjectConfig = toml::from_str("").unwrap();
        assert!(cfg.slang.filelist.is_none());
        assert!(cfg.slang.include_dirs.is_empty());
        assert!(cfg.slang.defines.is_empty());
        assert!(cfg.slang.top.is_none());
        assert_eq!(cfg.slang.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert!(cfg.env.is_empty());
        // Every feature toggle defaults to ON.
        assert!(cfg.features.semantic_tokens);
        assert!(cfg.features.format_specs_in_strings);
        assert!(cfg.features.keyword_hover);
        assert!(cfg.features.formatting);
    }

    /// `[features]` table parses; missing fields keep their defaults.
    #[test]
    fn project_config_features_section_decodes() {
        let toml_text = r#"
            [features]
            semantic_tokens = false
            format_specs_in_strings = false
        "#;
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert!(!cfg.features.semantic_tokens);
        assert!(!cfg.features.format_specs_in_strings);
        // Not specified — picks up the default.
        assert!(cfg.features.keyword_hover);
    }

    /// Unknown keys inside `[features]` are rejected — same `deny_unknown_fields`
    /// policy the other tables use, so a typo'd toggle name fails loudly
    /// instead of silently doing nothing.
    #[test]
    fn project_config_features_rejects_unknown_keys() {
        let bad = r#"
            [features]
            semanitc_tokens = true
        "#;
        assert!(toml::from_str::<ProjectConfig>(bad).is_err());
    }

    /// `[env]` table parses into a key-value map.
    #[test]
    fn project_config_env_section_decodes() {
        let toml_text = r#"
            [env]
            MIMIR_SLANG_PATH = "/opt/mimir/sidecar"
            MY_ROOT = "/work/proj"
        "#;
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(
            cfg.env.get("MIMIR_SLANG_PATH").map(|s| s.as_str()),
            Some("/opt/mimir/sidecar")
        );
        assert_eq!(cfg.env.get("MY_ROOT").map(|s| s.as_str()), Some("/work/proj"));
    }

    /// `[env]` vars expand in filelist tokens (verify via `ResolvedProject::load`).
    #[test]
    fn env_vars_expand_in_filelist() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("project.f");
        fs::write(&f, "+incdir+${MY_INC}\n").unwrap();
        fs::write(
            dir.path().join(".mimir.toml"),
            &format!(
                "[env]\nMY_INC = \"{}\"\n\n[slang]\nfilelist = \"project.f\"\n",
                dir.path().join("verif").display()
            ),
        )
        .unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.include_dirs.len(), 1);
        assert!(resolved.include_dirs[0].ends_with("verif"));
        assert_eq!(resolved.env.get("MY_INC").map(|s| s.as_str()),
            Some(dir.path().join("verif").to_str().unwrap()));
    }

    /// A fully-populated `[slang]` section round-trips into our types.
    #[test]
    fn project_config_full_section_decodes() {
        let toml_text = r#"
            [slang]
            filelist     = "sim/uvm.f"
            include_dirs = ["rtl", "verif/inc"]
            defines      = ["UVM_NO_DPI", "BUS_WIDTH=32"]
            top          = "tb_top"
            debounce_ms  = 200
        "#;
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.slang.filelist.as_deref(), Some(Path::new("sim/uvm.f")));
        assert_eq!(cfg.slang.include_dirs.len(), 2);
        assert_eq!(cfg.slang.defines, vec!["UVM_NO_DPI", "BUS_WIDTH=32"]);
        assert_eq!(cfg.slang.top.as_deref(), Some("tb_top"));
        assert_eq!(cfg.slang.debounce_ms, 200);
    }

    /// `[slang] single_unit` and `[slang] timescale` decode as typed values
    /// and feed straight into `ElaborateParams` (via `ResolvedProject::load`).
    /// Omitting them keeps the existing per-CU / no-default-timescale
    /// behaviour.
    #[test]
    fn project_config_single_unit_and_timescale_decode() {
        let toml_text = r#"
            [slang]
            single_unit = true
            timescale   = "1ns/1ps"
        "#;
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert!(cfg.slang.single_unit);
        assert_eq!(cfg.slang.timescale.as_deref(), Some("1ns/1ps"));

        // Omitting both yields the safe defaults.
        let cfg2: ProjectConfig = toml::from_str("[slang]\n").unwrap();
        assert!(!cfg2.slang.single_unit);
        assert!(cfg2.slang.timescale.is_none());
    }

    /// `[diagnostics]` decodes its fields, and the resolved policy demotes a
    /// matching path while leaving others alone.
    #[test]
    fn project_config_diagnostics_section_decodes_and_resolves() {
        use crate::diag_policy::DiagAction;
        use mimir_ast::DiagSeverity;

        let toml_text = r#"
            [diagnostics]
            demote_paths    = ["uvm-1.2", "vendor/"]
            demote_severity = "hint"
            ignore_paths    = ["generated/"]
        "#;
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.diagnostics.demote_paths, vec!["uvm-1.2", "vendor/"]);
        assert_eq!(cfg.diagnostics.demote_severity, "hint");
        assert_eq!(cfg.diagnostics.ignore_paths, vec!["generated/"]);

        let policy = crate::diag_policy::DiagnosticPolicy::from_config(
            cfg.diagnostics.demote_paths,
            &cfg.diagnostics.demote_severity,
            cfg.diagnostics.ignore_paths,
        );
        assert!(matches!(
            policy.action_for("/x/uvm-1.2/src/uvm_pkg.sv"),
            DiagAction::DemoteFloor(DiagSeverity::Hint)
        ));
        assert_eq!(policy.action_for("/x/generated/a.sv"), DiagAction::Drop);
        assert_eq!(policy.action_for("/x/rtl/my_dut.sv"), DiagAction::Keep);
    }

    /// An omitted `[diagnostics]` table decodes to the no-op default.
    #[test]
    fn project_config_diagnostics_defaults_to_noop() {
        let cfg: ProjectConfig = toml::from_str("").unwrap();
        assert!(cfg.diagnostics.demote_paths.is_empty());
        assert!(cfg.diagnostics.ignore_paths.is_empty());
        assert_eq!(cfg.diagnostics.demote_severity, "hint");
    }

    /// Unknown keys in `.mimir.toml` are an error, not silently ignored —
    /// otherwise a typo (`includ_dirs`) would silently disable the user's
    /// intent.
    #[test]
    fn project_config_rejects_unknown_keys() {
        let bad = r#"[slang]
            includ_dirs = ["x"]
        "#;
        assert!(toml::from_str::<ProjectConfig>(bad).is_err());
    }

    /// Discovery finds `.mimir.toml` in an ancestor directory.
    #[test]
    fn discover_walks_up() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.path().join(".mimir.toml"), "").unwrap();

        let resolved = ResolvedProject::discover(&nested).unwrap();
        let resolved = resolved.expect("expected to find .mimir.toml");
        assert_eq!(resolved.root, dir.path());
    }

    /// No `.mimir.toml` anywhere up the tree → `Ok(None)`, not an error.
    #[test]
    fn discover_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("x");
        fs::create_dir_all(&nested).unwrap();
        let resolved = ResolvedProject::discover(&nested).unwrap();
        assert!(resolved.is_none());
    }

    /// `ResolvedProject::load` reads a `.mimir.toml`, follows its
    /// `filelist`, and merges the result with the inline include_dirs /
    /// defines. End-to-end test exercising the public entry point.
    #[test]
    fn resolved_project_load_merges_inline_and_filelist() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("uvm.f"),
            "tb_top.sv\n+incdir+verif/uvm\n+define+UVM_OBJECT_MUST_HAVE_CONSTRUCTOR\n",
        )
        .unwrap();
        fs::write(
            dir.path().join(".mimir.toml"),
            r#"
                [slang]
                filelist     = "uvm.f"
                include_dirs = ["rtl"]
                defines      = ["BUS=64"]
                top          = "tb_top"
                debounce_ms  = 250
            "#,
        )
        .unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        assert!(resolved.files[0].ends_with("tb_top.sv"));
        // include_dirs: inline `rtl` first, then filelist `verif/uvm`.
        assert_eq!(resolved.include_dirs.len(), 2);
        assert!(resolved.include_dirs[0].ends_with("rtl"));
        assert!(resolved.include_dirs[1].ends_with("verif/uvm"));
        // defines: inline `BUS=64` first, then filelist's UVM macro.
        assert_eq!(resolved.defines.len(), 2);
        assert_eq!(resolved.defines[0].name, "BUS");
        assert_eq!(resolved.defines[0].value.as_deref(), Some("64"));
        assert_eq!(resolved.defines[1].name, "UVM_OBJECT_MUST_HAVE_CONSTRUCTOR");
        assert_eq!(resolved.top.as_deref(), Some("tb_top"));
        assert_eq!(resolved.debounce_ms, 250);
    }

    /// Empty `[formatter]` table decodes to all defaults — binary name is
    /// `"verible-verilog-format"` and every option field is `None`.
    #[test]
    fn formatter_config_defaults() {
        let cfg: ProjectConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.formatter.binary, "verible-verilog-format");
        assert!(cfg.formatter.column_limit.is_none());
        assert!(cfg.formatter.indentation_spaces.is_none());
        assert!(cfg.formatter.wrap_spaces.is_none());
        assert!(cfg.formatter.try_wrap_long_lines.is_none());
        assert!(cfg.formatter.port_declarations_alignment.is_none());
        assert!(cfg.formatter.extra_args.is_empty());
        assert!(cfg.formatter.wrap_ifdefs, "wrap_ifdefs should default to true");
    }

    /// `[formatter]` fields round-trip correctly including `extra_args`.
    #[test]
    fn formatter_config_overrides_decode() {
        let toml_text = r#"
            [formatter]
            binary             = "/opt/verible/bin/verible-verilog-format"
            column_limit       = 120
            indentation_spaces = 4
            wrap_spaces        = 8
            try_wrap_long_lines = true
            port_declarations_alignment       = "align"
            assignment_statement_alignment    = "preserve"
            named_parameter_alignment         = "align"
            named_port_alignment              = "align"
            module_net_variable_alignment     = "align"
            formal_parameters_alignment       = "align"
            class_member_variable_alignment   = "align"
            struct_union_members_alignment    = "align"
            extra_args = ["--expand_coverpoints", "--failsafe_success=false"]
        "#;
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.formatter.binary, "/opt/verible/bin/verible-verilog-format");
        assert_eq!(cfg.formatter.column_limit, Some(120));
        assert_eq!(cfg.formatter.indentation_spaces, Some(4));
        assert_eq!(cfg.formatter.wrap_spaces, Some(8));
        assert_eq!(cfg.formatter.try_wrap_long_lines, Some(true));
        assert_eq!(
            cfg.formatter.port_declarations_alignment.as_deref(),
            Some("align")
        );
        assert_eq!(
            cfg.formatter.assignment_statement_alignment.as_deref(),
            Some("preserve")
        );
        assert_eq!(cfg.formatter.extra_args, ["--expand_coverpoints", "--failsafe_success=false"]);
        assert!(cfg.formatter.wrap_ifdefs, "wrap_ifdefs should default to true");
    }

    /// `wrap_ifdefs = false` round-trips correctly.
    #[test]
    fn formatter_config_wrap_ifdefs_can_be_disabled() {
        let toml_text = "[formatter]\nwrap_ifdefs = false\n";
        let cfg: ProjectConfig = toml::from_str(toml_text).unwrap();
        assert!(!cfg.formatter.wrap_ifdefs);
    }

    /// Unknown keys inside `[formatter]` are rejected the same way as other
    /// tables — a typo'd field name fails loudly.
    #[test]
    fn formatter_config_rejects_unknown_keys() {
        let bad = "[formatter]\ncolum_limit = 80\n";
        assert!(toml::from_str::<ProjectConfig>(bad).is_err());
    }

    /// `[slang] extra_args` round-trips through TOML parsing.
    #[test]
    fn slang_extra_args_round_trips() {
        let cfg: ProjectConfig = toml::from_str(r#"
            [slang]
            extra_args = ["--timescale", "1ns/1ps"]
        "#).unwrap();
        assert_eq!(cfg.slang.extra_args, ["--timescale", "1ns/1ps"]);
    }

    /// Omitting `[slang] extra_args` defaults to an empty vec.
    #[test]
    fn slang_extra_args_defaults_empty() {
        let cfg: ProjectConfig = toml::from_str("").unwrap();
        assert!(cfg.slang.extra_args.is_empty());
    }

    /// `[slang] files` entries are prepended before filelist entries.
    #[test]
    fn inline_files_prepended_before_filelist() {
        let dir = tempdir().unwrap();
        let extra = dir.path().join("extra.sv");
        fs::write(&extra, "").unwrap();
        fs::write(dir.path().join("tb_top.sv"), "").unwrap();
        fs::write(dir.path().join("project.f"), "tb_top.sv\n").unwrap();
        fs::write(
            dir.path().join(".mimir.toml"),
            r#"
                [slang]
                files    = ["extra.sv"]
                filelist = "project.f"
            "#,
        )
        .unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 2);
        assert!(resolved.files[0].ends_with("extra.sv"), "inline file must come first");
        assert!(resolved.files[1].ends_with("tb_top.sv"), "filelist file must come second");
    }

    /// Env values may reference sibling `[env]` keys with `${VAR}` syntax.
    #[test]
    fn env_vars_compose_within_env_section() {
        let toml_text = r#"
            [env]
            ROOT    = "/work/project"
            INC_DIR = "${ROOT}/include"
            SIDECAR = "${ROOT}/bin/mimir-slang-sidecar"
        "#;
        let dir = tempdir().unwrap();
        // Write TOML so we can call ResolvedProject::load (which does the expansion).
        let toml_path = dir.path().join(".mimir.toml");
        fs::write(&toml_path, toml_text).unwrap();
        let resolved = ResolvedProject::load(&toml_path).unwrap();
        assert_eq!(resolved.env.get("ROOT").map(|s| s.as_str()), Some("/work/project"));
        assert_eq!(resolved.env.get("INC_DIR").map(|s| s.as_str()), Some("/work/project/include"));
        assert_eq!(resolved.env.get("SIDECAR").map(|s| s.as_str()), Some("/work/project/bin/mimir-slang-sidecar"));
    }

    /// `[features] formatting` defaults to `true` and can be set to `false`.
    #[test]
    fn feature_toggle_formatting_defaults_true() {
        let cfg: ProjectConfig = toml::from_str("").unwrap();
        assert!(cfg.features.formatting);
    }

    #[test]
    fn feature_toggle_formatting_can_be_disabled() {
        let cfg: ProjectConfig = toml::from_str("[features]\nformatting = false\n").unwrap();
        assert!(!cfg.features.formatting);
        // Other toggles stay at their defaults.
        assert!(cfg.features.semantic_tokens);
        assert!(cfg.features.keyword_hover);
    }

    // ------------------------------------------------------------------
    // Multi-level env-var expansion
    // ------------------------------------------------------------------

    /// Two-level chain: B references A, C references B.
    /// After expansion C must resolve all the way to /base/sub/deep.
    #[test]
    fn env_vars_two_level_chain_expands_correctly() {
        let dir = tempdir().unwrap();
        let toml_text = r#"
            [env]
            A = "/base"
            B = "${A}/sub"
            C = "${B}/deep"
        "#;
        let toml_path = dir.path().join(".mimir.toml");
        fs::write(&toml_path, toml_text).unwrap();
        let resolved = ResolvedProject::load(&toml_path).unwrap();

        assert_eq!(resolved.env.get("A").map(|s| s.as_str()), Some("/base"));
        assert_eq!(resolved.env.get("B").map(|s| s.as_str()), Some("/base/sub"));
        assert_eq!(resolved.env.get("C").map(|s| s.as_str()), Some("/base/sub/deep"));
    }

    /// Three-level chain: D references C which references B which references A.
    #[test]
    fn env_vars_three_level_chain_expands_correctly() {
        let dir = tempdir().unwrap();
        let toml_text = r#"
            [env]
            A = "/root"
            B = "${A}/tier1"
            C = "${B}/tier2"
            D = "${C}/tier3"
        "#;
        let toml_path = dir.path().join(".mimir.toml");
        fs::write(&toml_path, toml_text).unwrap();
        let resolved = ResolvedProject::load(&toml_path).unwrap();

        assert_eq!(resolved.env.get("D").map(|s| s.as_str()), Some("/root/tier1/tier2/tier3"));
    }

    /// Multi-level env vars whose final expanded value is an absolute path
    /// are used verbatim in filelist file entries (not prefixed with the
    /// filelist's directory).
    #[test]
    fn env_vars_multi_level_expand_in_filelist_file_entries() {
        let dir = tempdir().unwrap();
        // Create the actual source file at the absolute path.
        let src_dir = dir.path().join("ip/rtl");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("dut.sv"), "").unwrap();

        let toml_text = format!(
            r#"
            [env]
            BASE     = "{base}"
            IP_ROOT  = "${{BASE}}/ip"
            RTL_DIR  = "${{IP_ROOT}}/rtl"

            [slang]
            filelist = "project.f"
            "#,
            base = dir.path().display()
        );
        fs::write(dir.path().join(".mimir.toml"), &toml_text).unwrap();
        // Reference the file using a multi-level-expanded var.
        fs::write(dir.path().join("project.f"), "${RTL_DIR}/dut.sv\n").unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        let got = &resolved.files[0];
        assert!(
            got.ends_with("ip/rtl/dut.sv"),
            "expected ip/rtl/dut.sv in path, got {:?}",
            got
        );
        assert!(got.is_absolute(), "resolved path must be absolute");
    }

    /// Multi-level env vars in +incdir+ tokens expand fully.
    #[test]
    fn env_vars_multi_level_expand_in_incdir_tokens() {
        let dir = tempdir().unwrap();
        let inc_dir = dir.path().join("verif/uvm/src");
        fs::create_dir_all(&inc_dir).unwrap();

        let toml_text = format!(
            r#"
            [env]
            PROJ     = "{proj}"
            VERIF    = "${{PROJ}}/verif"
            UVM_SRC  = "${{VERIF}}/uvm/src"

            [slang]
            filelist = "project.f"
            "#,
            proj = dir.path().display()
        );
        fs::write(dir.path().join(".mimir.toml"), &toml_text).unwrap();
        fs::write(dir.path().join("project.f"), "+incdir+${UVM_SRC}\n").unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.include_dirs.len(), 1);
        let got = &resolved.include_dirs[0];
        assert!(
            got.ends_with("verif/uvm/src"),
            "expected verif/uvm/src, got {:?}",
            got
        );
        assert!(got.is_absolute());
    }

    /// Multi-level env vars in +define+ values also expand fully.
    #[test]
    fn env_vars_multi_level_expand_in_define_values() {
        let dir = tempdir().unwrap();
        let toml_text = r#"
            [env]
            VER_MAJOR = "4"
            VER_MINOR = "2"
            VERSION   = "${VER_MAJOR}.${VER_MINOR}"

            [slang]
            filelist = "project.f"
        "#;
        fs::write(dir.path().join(".mimir.toml"), toml_text).unwrap();
        fs::write(dir.path().join("project.f"), "+define+VERSION=${VERSION}\n").unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.defines.len(), 1);
        assert_eq!(resolved.defines[0].name, "VERSION");
        assert_eq!(resolved.defines[0].value.as_deref(), Some("4.2"));
    }

    /// An env value that references a process environment variable and is
    /// then referenced by another env key still fully resolves.
    #[test]
    fn env_vars_chain_through_process_env() {
        let dir = tempdir().unwrap();
        // Set a real process env var.
        std::env::set_var("MIMIR_TEST_BASE_DIR", dir.path().to_str().unwrap());
        let toml_text = r#"
            [env]
            PROJECT = "${MIMIR_TEST_BASE_DIR}/project"
            SRC     = "${PROJECT}/src"
        "#;
        let toml_path = dir.path().join(".mimir.toml");
        fs::write(&toml_path, toml_text).unwrap();
        let resolved = ResolvedProject::load(&toml_path).unwrap();
        std::env::remove_var("MIMIR_TEST_BASE_DIR");

        let expected_src = format!("{}/project/src", dir.path().display());
        assert_eq!(resolved.env.get("SRC").map(|s| s.as_str()), Some(expected_src.as_str()));
    }

    // ------------------------------------------------------------------
    // Path-resolution fallback: full-path / absolute paths are preserved
    // ------------------------------------------------------------------

    /// When a filelist entry resolves to an absolute path via env expansion,
    /// the full-path result is returned even when that file doesn't exist yet.
    #[test]
    fn filelist_absolute_path_via_env_not_prefixed() {
        let dir = tempdir().unwrap();
        // Note: the file does NOT exist on disk.
        let toml_text = r#"
            [env]
            ABS_FILE = "/some/absolute/path/tb_top.sv"

            [slang]
            filelist = "project.f"
        "#;
        fs::write(dir.path().join(".mimir.toml"), toml_text).unwrap();
        fs::write(dir.path().join("project.f"), "${ABS_FILE}\n").unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        assert_eq!(
            resolved.files[0],
            PathBuf::from("/some/absolute/path/tb_top.sv"),
            "env-expanded absolute path must not be prefixed with the filelist dir"
        );
    }

    /// When a relative path in the filelist doesn't exist relative to the
    /// filelist directory but DOES exist relative to the TOML root, the
    /// TOML-root version is returned (not the raw relative path joined to CWD).
    #[test]
    fn filelist_falls_back_to_toml_root_not_cwd() {
        let dir = tempdir().unwrap();
        // Filelist is in a sub-directory; file lives at the project root level.
        let sub = dir.path().join("sim");
        fs::create_dir_all(&sub).unwrap();
        let rtl = dir.path().join("rtl");
        fs::create_dir_all(&rtl).unwrap();
        fs::write(rtl.join("chip.sv"), "").unwrap();

        fs::write(sub.join("tb.f"), "rtl/chip.sv\n").unwrap();
        let toml_text = "[slang]\nfilelist = \"sim/tb.f\"\n";
        fs::write(dir.path().join(".mimir.toml"), toml_text).unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        let got = &resolved.files[0];
        // Must be the TOML-root-relative version, not an arbitrary CWD join.
        assert_eq!(*got, rtl.join("chip.sv"), "expected TOML-root fallback");
        assert!(got.is_absolute());
    }

    /// A path that is not found relative to the filelist dir, not found
    /// relative to the TOML root, and is not absolute falls back to the
    /// filelist-base join (forward reference — the file may not exist yet).
    #[test]
    fn filelist_unknown_relative_path_defaults_to_filelist_base() {
        let dir = tempdir().unwrap();
        // File does NOT exist anywhere.
        fs::write(dir.path().join("project.f"), "nonexistent/future.sv\n").unwrap();
        fs::write(dir.path().join(".mimir.toml"), "[slang]\nfilelist = \"project.f\"\n").unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        let got = &resolved.files[0];
        // Should be filelist_dir/nonexistent/future.sv, not a raw relative path.
        assert_eq!(*got, dir.path().join("nonexistent/future.sv"));
        assert!(got.is_absolute(), "default join must produce an absolute path");
    }

    /// Absolute paths listed directly in `[slang] files` (not through a filelist)
    /// are preserved without being re-joined to the TOML root.
    #[test]
    fn inline_absolute_file_path_preserved() {
        let dir = tempdir().unwrap();
        let toml_text = r#"
            [slang]
            files = ["/absolute/path/to/a.sv"]
        "#;
        fs::write(dir.path().join(".mimir.toml"), toml_text).unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        assert_eq!(resolved.files[0], PathBuf::from("/absolute/path/to/a.sv"));
    }

    /// An env-expanded absolute path in `[slang] files` survives the
    /// absolutise pass unchanged.
    #[test]
    fn inline_env_expanded_absolute_file_path_preserved() {
        let dir = tempdir().unwrap();
        let toml_text = r#"
            [env]
            ROOT = "/opt/verif"

            [slang]
            files = ["${ROOT}/tb/top.sv"]
        "#;
        fs::write(dir.path().join(".mimir.toml"), toml_text).unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();
        assert_eq!(resolved.files.len(), 1);
        assert_eq!(resolved.files[0], PathBuf::from("/opt/verif/tb/top.sv"));
    }

    /// End-to-end: a deeply nested env hierarchy feeds both the filelist path
    /// and entries inside the filelist, all resolving to correct absolute paths.
    #[test]
    fn end_to_end_deep_env_hierarchy_with_filelist() {
        let dir = tempdir().unwrap();

        // Build a realistic directory tree.
        let rtl = dir.path().join("hw/rtl");
        let tb = dir.path().join("hw/tb");
        let inc = dir.path().join("hw/inc");
        let sim = dir.path().join("sim");
        for d in &[&rtl, &tb, &inc, &sim] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(rtl.join("dut.sv"), "").unwrap();
        fs::write(tb.join("tb_top.sv"), "").unwrap();

        // Filelist lives in sim/; uses multi-level vars for all paths.
        let filelist_content = format!(
            "${{{RTL}}}/dut.sv\n${{{TB}}}/tb_top.sv\n+incdir+${{{INC}}}\n+define+SIM_BUILD\n",
            RTL = "RTL_DIR",
            TB = "TB_DIR",
            INC = "INC_DIR",
        );
        fs::write(sim.join("project.f"), &filelist_content).unwrap();

        let toml_text = format!(
            r#"
            [env]
            HW_ROOT = "{hw}"
            RTL_DIR = "${{HW_ROOT}}/rtl"
            TB_DIR  = "${{HW_ROOT}}/tb"
            INC_DIR = "${{HW_ROOT}}/inc"

            [slang]
            filelist = "sim/project.f"
            "#,
            hw = dir.path().join("hw").display()
        );
        fs::write(dir.path().join(".mimir.toml"), &toml_text).unwrap();

        let resolved = ResolvedProject::load(&dir.path().join(".mimir.toml")).unwrap();

        assert_eq!(resolved.files.len(), 2, "should have dut.sv and tb_top.sv");
        assert!(resolved.files[0].ends_with("hw/rtl/dut.sv"), "got {:?}", resolved.files[0]);
        assert!(resolved.files[1].ends_with("hw/tb/tb_top.sv"), "got {:?}", resolved.files[1]);
        assert!(resolved.files[0].is_absolute());
        assert!(resolved.files[1].is_absolute());

        assert_eq!(resolved.include_dirs.len(), 1);
        assert!(resolved.include_dirs[0].ends_with("hw/inc"), "got {:?}", resolved.include_dirs[0]);
        assert!(resolved.include_dirs[0].is_absolute());

        assert_eq!(resolved.defines.len(), 1);
        assert_eq!(resolved.defines[0].name, "SIM_BUILD");
    }

    // ── example-workspace smoke tests ─────────────────────────────────────
    // These tests load the real .mimir.toml files from examples/ to confirm
    // they parse, resolve, and yield at least one source file.
    //
    // Marked #[ignore] because the repos live in gitignored directories and
    // are NOT present in CI.  Run manually with:
    //   cargo test -p mimir-server -- --ignored example_
    // after cloning the repos into examples/.

    #[test]
    #[ignore = "requires locally cloned examples/riscv-dv (gitignored, not in CI)"]
    fn example_riscv_dv_toml_loads_clean() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap()
            .join("examples/riscv-dv/.mimir.toml");
        if !path.exists() {
            eprintln!("SKIP: examples/riscv-dv not cloned — run `git clone` to enable");
            return;
        }
        let proj = ResolvedProject::load(&path)
            .expect("examples/riscv-dv/.mimir.toml should load without error");
        assert!(!proj.files.is_empty(), "expected at least one resolved source file");
        eprintln!(
            "riscv-dv: {} files, {} include_dirs, {} defines",
            proj.files.len(),
            proj.include_dirs.len(),
            proj.defines.len()
        );
    }

    #[test]
    #[ignore = "requires locally cloned examples/ibex (gitignored, not in CI)"]
    fn example_ibex_toml_loads_clean() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap()
            .join("examples/ibex/.mimir.toml");
        if !path.exists() {
            eprintln!("SKIP: examples/ibex not cloned — run `git clone` to enable");
            return;
        }
        let proj = ResolvedProject::load(&path)
            .expect("examples/ibex/.mimir.toml should load without error");
        assert!(!proj.files.is_empty(), "expected at least one resolved source file");
        eprintln!(
            "ibex: {} files, {} include_dirs, {} defines",
            proj.files.len(),
            proj.include_dirs.len(),
            proj.defines.len()
        );
    }
}
