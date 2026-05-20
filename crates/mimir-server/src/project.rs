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
//!   a [filelist](#filelists). Discovered by walking up from
//!   `initialize`'s `rootUri`.
//! * **`.f` filelists** — the verification-industry standard. Used by
//!   every commercial simulator (VCS, Xcelium, Questa) and Verilator. A
//!   line-oriented mix of source paths, `+incdir+`, `+define+`, and
//!   recursive `-f`.
//!
//! Both feed a single [`ResolvedProject`] which Stage 3 reads to build an
//! [`mimir_slang::ElaborateParams`] envelope.
//!
//! # Filelists
//!
//! Each whitespace-separated token is one of:
//!
//! | Token                    | Meaning                                            |
//! | ------------------------ | -------------------------------------------------- |
//! | `path/to/file.sv`        | Source file to compile. Relative to the `.f`'s dir. |
//! | `+incdir+A[+B+...]`      | One or more include search paths, `+`-separated.   |
//! | `+define+NAME[=VALUE]`   | Predefine a macro (multiple `+`-separated allowed).|
//! | `-f nested.f`            | Recursively read another filelist.                 |
//! | `// rest of line`        | Comment.                                           |
//! | `# rest of line`         | Comment (alternate).                               |
//! | trailing `\` + newline   | Line continuation.                                 |
//! | `${VAR}` anywhere        | Expanded from config `[env]`, then the process environment. |
//!
//! Recursion is bounded ([`FILELIST_MAX_DEPTH`]) and cycles are detected
//! by canonical path so a malformed `-f a.f -f a.f` doesn't loop forever.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use mimir_slang::MacroDefine;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Maximum nesting depth for `-f` recursion. Real projects rarely nest
/// more than two or three levels; 16 is a comfortable ceiling that still
/// catches misconfiguration before we exhaust the stack.
pub const FILELIST_MAX_DEPTH: usize = 16;

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
            let mut in_progress = HashSet::new();
            let mut done = HashSet::new();
            expand_filelist(
                &absolute,
                0,
                &root,
                &mut FilelistWalkState {
                    in_progress: &mut in_progress,
                    done: &mut done,
                    files: &mut files,
                    include_dirs: &mut include_dirs,
                    defines: &mut defines,
                },
                &env,
            )?;
        }

        info!(
            root = %root.display(),
            files = files.len(),
            include_dirs = include_dirs.len(),
            defines = defines.len(),
            env_vars = env.len(),
            top = ?cfg.slang.top,
            debounce_ms = cfg.slang.debounce_ms,
            "resolved project config",
        );

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
        })
    }
}

// --------------------------------------------------------------------------
// Filelist parsing
// --------------------------------------------------------------------------

/// Tokenise a `.f` filelist body. Handles `//` and `#` line comments,
/// backslash-newline line continuation, and ASCII whitespace as the token
/// separator. Quoted strings aren't recognised — they're rare in `.f`
/// files and we'd need to make a call about whether `+`-splitting still
/// applies. Easy to extend later if real projects need it.
fn tokenise_filelist(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // `//` comment to EOL.
            '/' if chars.peek() == Some(&'/') => {
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            // `#` comment to EOL. Common in hand-written filelists.
            '#' => {
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            // Backslash-newline continuation: drop both, the next line
            // becomes part of the same logical line.
            '\\' if chars.peek() == Some(&'\n') => {
                chars.next();
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Expand `${VAR}` references. `env` (config-provided) is checked first;
/// the process environment is the fallback. Unknown variables expand to the
/// empty string (matches GNU `make`'s behaviour and what most simulators
/// do). Bare `$VAR` (without braces) is left alone — too easy to
/// false-positive on a literal `$` in a path.
fn expand_env_vars(s: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut name = String::new();
            let mut closed = false;
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '}' {
                    closed = true;
                    break;
                }
                name.push(n);
            }
            if closed {
                // Config env first, then process env; unknown → empty.
                if let Some(value) = env.get(&name) {
                    out.push_str(value);
                } else if let Ok(value) = std::env::var(&name) {
                    out.push_str(&value);
                }
            } else {
                // Unterminated `${`; emit it literally so we don't lose data.
                out.push('$');
                out.push('{');
                out.push_str(&name);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a single `+define+` value (`"NAME"` or `"NAME=VALUE"`) into the
/// structured [`MacroDefine`] the wire protocol carries.
fn parse_define(s: &str) -> MacroDefine {
    if let Some((name, value)) = s.split_once('=') {
        MacroDefine {
            name: name.to_string(),
            value: Some(value.to_string()),
        }
    } else {
        MacroDefine {
            name: s.to_string(),
            value: None,
        }
    }
}

/// Absolutise `p` against `base`. Already-absolute paths are returned
/// unchanged. For relative paths, tries `base.join(p)` first; if that path
/// does not exist on disk but `p` itself does (e.g. the raw string is
/// reachable from the current working directory or becomes absolute after
/// env-var expansion), the raw path is returned so callers don't silently
/// swallow a misconfigured TOML-relative prefix.
fn absolutise(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let joined = base.join(p);
    if !joined.exists() && p.exists() {
        debug!(
            path = %p.display(),
            tried = %joined.display(),
            "path not found relative to TOML root; using path as written"
        );
        p.to_path_buf()
    } else {
        joined
    }
}

/// Absolutise `p` inside a filelist, with a three-level fallback chain:
///
/// 1. Already absolute → return as-is.
/// 2. `filelist_base.join(p)` exists → use it (normal case: path relative to the `.f`).
/// 3. `toml_root.join(p)` exists → use it (filelist written relative to the project root).
/// 4. `p` exists as written (CWD-relative or absolute after env expansion) → use it.
/// 5. Default: `filelist_base.join(p)` (path doesn't exist yet; forward-reference is OK).
fn absolutise_filelist(filelist_base: &Path, toml_root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let joined = filelist_base.join(p);
    if joined.exists() {
        return joined;
    }
    if filelist_base != toml_root {
        let via_root = toml_root.join(p);
        if via_root.exists() {
            debug!(
                path = %p.display(),
                via = %via_root.display(),
                "path not found relative to filelist dir; resolved via TOML root"
            );
            return via_root;
        }
    }
    if p.exists() {
        debug!(
            path = %p.display(),
            tried = %joined.display(),
            "path not found relative to filelist dir or TOML root; using path as written"
        );
        return p.to_path_buf();
    }
    joined
}

/// Mutable accumulator threaded through the recursive [`expand_filelist`]
/// walk. Grouping the five out-parameters into one struct keeps the
/// function signature under the lint threshold and makes the recursive
/// calls self-documenting.
struct FilelistWalkState<'a> {
    /// Gray set — canonical paths currently on the call stack.
    in_progress: &'a mut HashSet<PathBuf>,
    /// Black set — canonical paths fully processed in a prior branch.
    done: &'a mut HashSet<PathBuf>,
    /// Accumulated source file paths in declaration order.
    files: &'a mut Vec<PathBuf>,
    /// Accumulated `+incdir+` directories in declaration order.
    include_dirs: &'a mut Vec<PathBuf>,
    /// Accumulated `+define+` macros in declaration order.
    defines: &'a mut Vec<MacroDefine>,
}

/// Recursively expand a filelist. Pushes results into `state` so a
/// top-level filelist with five `-f` includes builds a single flat
/// output rather than a tree the caller has to walk.
///
/// Two-set DFS coloring distinguishes repeat references from true cycles:
///
/// * `state.in_progress` — canonical paths currently on the call stack
///   (gray nodes). A hit here is a back-edge (`a.f → b.f → a.f`) and
///   returns [`ProjectError::FilelistCycle`].
/// * `state.done` — canonical paths fully processed in a prior branch
///   (black nodes). A hit here is a diamond/shared reference, which is
///   valid; we log a warning and skip the duplicate.
///
/// `depth` is checked against [`FILELIST_MAX_DEPTH`] before any work.
fn expand_filelist(
    path: &Path,
    depth: usize,
    toml_root: &Path,
    state: &mut FilelistWalkState<'_>,
    env: &HashMap<String, String>,
) -> Result<(), ProjectError> {
    if depth >= FILELIST_MAX_DEPTH {
        return Err(ProjectError::FilelistTooDeep {
            path: path.to_path_buf(),
            limit: FILELIST_MAX_DEPTH,
        });
    }

    // Canonicalise for cycle/repeat detection; fall back to the raw path on
    // platforms / cases where canonicalize fails (e.g. symlink loops we
    // didn't make ourselves).
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Already fully processed in a sibling branch — valid diamond reference.
    if state.done.contains(&canonical) {
        warn!(
            path = %path.display(),
            "filelist referenced more than once; skipping duplicate"
        );
        return Ok(());
    }

    // Currently on the call stack — this is a true cycle.
    if !state.in_progress.insert(canonical.clone()) {
        return Err(ProjectError::FilelistCycle {
            path: path.to_path_buf(),
        });
    }

    let text = fs::read_to_string(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let base = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let tokens = tokenise_filelist(&text);
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if let Some(rest) = token.strip_prefix("+incdir+") {
            for dir in rest.split('+').filter(|s| !s.is_empty()) {
                state.include_dirs.push(absolutise_filelist(&base, toml_root, Path::new(&expand_env_vars(dir, env))));
            }
            i += 1;
        } else if let Some(rest) = token.strip_prefix("+define+") {
            for d in rest.split('+').filter(|s| !s.is_empty()) {
                state.defines.push(parse_define(&expand_env_vars(d, env)));
            }
            i += 1;
        } else if token == "-f" || token == "-F" {
            // Two-token form: `-f nested.f`.
            let Some(next) = tokens.get(i + 1) else {
                warn!("trailing `-f` with no filelist path; ignoring");
                break;
            };
            let nested = absolutise_filelist(&base, toml_root, Path::new(&expand_env_vars(next, env)));
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
            i += 2;
        } else if let Some(rest) = token.strip_prefix("-f") {
            // One-token form: `-fnested.f`.
            let nested = absolutise_filelist(&base, toml_root, Path::new(&expand_env_vars(rest, env)));
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
            i += 1;
        } else if let Some(rest) = token.strip_prefix("-F") {
            let nested = absolutise_filelist(&base, toml_root, Path::new(&expand_env_vars(rest, env)));
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
            i += 1;
        } else {
            state.files.push(absolutise_filelist(&base, toml_root, Path::new(&expand_env_vars(token, env))));
            i += 1;
        }
    }

    // Transition from gray → black: no longer on the active call stack.
    state.in_progress.remove(&canonical);
    state.done.insert(canonical);

    Ok(())
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

    /// `parse_define` covers both flavours: `NAME` and `NAME=VALUE`.
    /// Splits on the *first* `=` so `BUS=A=B` → name=BUS, value=A=B.
    #[test]
    fn parse_define_handles_both_forms() {
        let d = parse_define("FOO");
        assert_eq!(d.name, "FOO");
        assert!(d.value.is_none());

        let d = parse_define("BUS_WIDTH=32");
        assert_eq!(d.name, "BUS_WIDTH");
        assert_eq!(d.value.as_deref(), Some("32"));

        let d = parse_define("EXPR=A=B");
        assert_eq!(d.name, "EXPR");
        assert_eq!(d.value.as_deref(), Some("A=B"));
    }

    /// Tokeniser recognises whitespace, both comment styles, and
    /// backslash-newline continuation. Tokens that span a continuation
    /// land as separate tokens once the surrounding whitespace is
    /// re-introduced — `\<NL>` is purely a "join lines" directive.
    #[test]
    fn tokenise_handles_comments_and_continuation() {
        let text = "\
            // header comment\n\
            a.sv b.sv  # trailing comment\n\
            +incdir+inc/a+inc/b\n\
            -f \\\n\
            nested.f\n\
        ";
        let tokens = tokenise_filelist(text);
        assert_eq!(
            tokens,
            vec![
                "a.sv".to_string(),
                "b.sv".to_string(),
                "+incdir+inc/a+inc/b".to_string(),
                "-f".to_string(),
                "nested.f".to_string(),
            ],
        );
    }

    /// `${VAR}` interpolates: config env first, then process env; unknown → empty.
    /// `$BARE` is left alone (we only recognise the braced form).
    #[test]
    fn expand_env_vars_basic() {
        let empty: HashMap<String, String> = HashMap::new();
        std::env::set_var("MIMIR_TEST_FOO", "hello");
        assert_eq!(expand_env_vars("${MIMIR_TEST_FOO}/x", &empty), "hello/x");
        assert_eq!(expand_env_vars("${MIMIR_NOPE_NOPE}/y", &empty), "/y");
        assert_eq!(expand_env_vars("$LITERAL", &empty), "$LITERAL");
        assert_eq!(expand_env_vars("plain", &empty), "plain");
        std::env::remove_var("MIMIR_TEST_FOO");
    }

    /// Config env takes precedence over the process environment.
    #[test]
    fn expand_env_vars_config_overrides_process() {
        std::env::set_var("MIMIR_TEST_OVERRIDE", "from_process");
        let mut env = HashMap::new();
        env.insert("MIMIR_TEST_OVERRIDE".into(), "from_config".into());
        assert_eq!(
            expand_env_vars("${MIMIR_TEST_OVERRIDE}", &env),
            "from_config"
        );
        std::env::remove_var("MIMIR_TEST_OVERRIDE");
    }

    /// Unknown in config → falls back to process env.
    #[test]
    fn expand_env_vars_config_fallback_to_process() {
        std::env::set_var("MIMIR_TEST_FALLBACK", "from_process");
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(
            expand_env_vars("${MIMIR_TEST_FALLBACK}", &env),
            "from_process"
        );
        std::env::remove_var("MIMIR_TEST_FALLBACK");
    }

    /// Single-file expansion: every directive type, paths absolutised
    /// against the filelist's directory, defines structured.
    #[test]
    fn expand_filelist_basic_directives() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("project.f");
        fs::write(
            &f,
            "\
            // top-of-file comment\n\
            ./a.sv\n\
            sub/b.sv  # inline\n\
            +incdir+inc+other\n\
            +define+UVM_NO_DPI+BUS=32\n\
        ",
        )
        .unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        expand_filelist(&f, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new()).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("a.sv"));
        assert!(files[1].ends_with("sub/b.sv"));
        assert_eq!(incs.len(), 2);
        assert!(incs[0].ends_with("inc"));
        assert!(incs[1].ends_with("other"));
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "UVM_NO_DPI");
        assert!(defs[0].value.is_none());
        assert_eq!(defs[1].name, "BUS");
        assert_eq!(defs[1].value.as_deref(), Some("32"));
    }

    /// `-f nested.f` includes nested directives in declaration order.
    #[test]
    fn expand_filelist_recursion() {
        let dir = tempdir().unwrap();
        let outer = dir.path().join("outer.f");
        let inner = dir.path().join("inner.f");
        fs::write(&inner, "inner.sv\n+incdir+nested_inc\n").unwrap();
        fs::write(&outer, "outer.sv\n-f inner.f\nafter.sv\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        expand_filelist(&outer, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new()).unwrap();

        // Order is: outer.sv, inner.sv (from nested), after.sv. The nested
        // include lands between the outer files that bracket the `-f`.
        assert_eq!(files.len(), 3);
        assert!(files[0].ends_with("outer.sv"));
        assert!(files[1].ends_with("inner.sv"));
        assert!(files[2].ends_with("after.sv"));
        assert_eq!(incs.len(), 1);
        assert!(incs[0].ends_with("nested_inc"));
    }

    /// Paths in a filelist that don't exist relative to the `.f`'s directory
    /// but do exist relative to the TOML root are resolved via the TOML root.
    #[test]
    fn expand_filelist_falls_back_to_toml_root() {
        let dir = tempdir().unwrap();
        // .mimir.toml lives in dir; the filelist lives in dir/sim/
        let sim = dir.path().join("sim");
        fs::create_dir_all(&sim).unwrap();
        // Source file is at dir/rtl/dut.sv — relative to TOML root, not to sim/
        let rtl = dir.path().join("rtl");
        fs::create_dir_all(&rtl).unwrap();
        fs::write(rtl.join("dut.sv"), "").unwrap();

        let f = sim.join("project.f");
        // "rtl/dut.sv" is relative to the project root, not to sim/ — the
        // filelist writer assumed the tool runs from the project root.
        fs::write(&f, "rtl/dut.sv\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        expand_filelist(&f, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new()).unwrap();

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("rtl/dut.sv"), "expected TOML-root fallback path, got {:?}", files[0]);
    }

    /// A filelist that `-f`-includes itself (direct self-loop) fails with
    /// `FilelistCycle`, not stack overflow.
    #[test]
    fn expand_filelist_direct_cycle_is_error() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("loop.f");
        fs::write(&f, "loop.sv\n-f loop.f\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        let err = expand_filelist(&f, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new())
            .expect_err("self-include should fail");
        assert!(matches!(err, ProjectError::FilelistCycle { .. }));
    }

    /// An indirect cycle (`a.f → b.f → a.f`) also fails with `FilelistCycle`.
    #[test]
    fn expand_filelist_indirect_cycle_is_error() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.f");
        let b = dir.path().join("b.f");
        fs::write(&a, "a.sv\n-f b.f\n").unwrap();
        fs::write(&b, "b.sv\n-f a.f\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        let err = expand_filelist(&a, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new())
            .expect_err("indirect cycle should fail");
        assert!(matches!(err, ProjectError::FilelistCycle { .. }));
    }

    /// Two sibling filelists that both `-f` the same shared filelist is a
    /// valid diamond reference — the second occurrence warns and skips rather
    /// than erroring.  Files from the shared filelist appear exactly once.
    #[test]
    fn expand_filelist_diamond_repeat_warns_and_skips() {
        let dir = tempdir().unwrap();
        // shared.f contributes one file and one incdir.
        let shared = dir.path().join("shared.f");
        fs::write(&shared, "shared.sv\n+incdir+shared_inc\n").unwrap();

        // left.f and right.f both include shared.f.
        let left = dir.path().join("left.f");
        let right = dir.path().join("right.f");
        fs::write(&left, "left.sv\n-f shared.f\n").unwrap();
        fs::write(&right, "right.sv\n-f shared.f\n").unwrap();

        // root.f includes both siblings.
        let root = dir.path().join("root.f");
        fs::write(&root, "-f left.f\n-f right.f\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        // Must succeed — not an error.
        expand_filelist(&root, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new())
            .expect("diamond reference should succeed");

        // left.sv, shared.sv (first visit), right.sv — shared.f skipped on second visit.
        assert_eq!(files.len(), 3, "got {:?}", files);
        assert!(files.iter().any(|p| p.ends_with("left.sv")));
        assert!(files.iter().any(|p| p.ends_with("shared.sv")));
        assert!(files.iter().any(|p| p.ends_with("right.sv")));
        // shared_inc appears exactly once.
        assert_eq!(incs.len(), 1);
        assert!(incs[0].ends_with("shared_inc"));
    }

    /// The same filelist referenced twice at the top level (not via nesting)
    /// also warns-and-skips on the second reference.
    #[test]
    fn expand_filelist_top_level_repeat_warns_and_skips() {
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared.f");
        fs::write(&shared, "shared.sv\n").unwrap();

        let root = dir.path().join("root.f");
        fs::write(&root, "-f shared.f\n-f shared.f\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut in_progress = HashSet::new();
        let mut done = HashSet::new();
        expand_filelist(&root, 0, dir.path(), &mut FilelistWalkState { in_progress: &mut in_progress, done: &mut done, files: &mut files, include_dirs: &mut incs, defines: &mut defs }, &HashMap::new())
            .expect("repeat top-level reference should succeed");

        // shared.sv must appear exactly once.
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("shared.sv"));
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

    /// `absolutise` falls back to the path as-is when the TOML-relative
    /// joined path does not exist but the path itself does (e.g. an absolute
    /// path that was still registered as relative text).
    #[test]
    fn absolutise_falls_back_when_joined_missing() {
        let dir = tempdir().unwrap();
        let fake_base = dir.path().join("nonexistent_subdir");
        // Create a file in `dir` directly; `fake_base/file.sv` won't exist.
        let real_file = dir.path().join("real.sv");
        fs::write(&real_file, "").unwrap();
        // Pass the absolute path as a relative-looking path object.
        let result = absolutise(&fake_base, &real_file);
        // Should return the path itself, not fake_base/real_file.
        assert_eq!(result, real_file);
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

    /// `absolutise_filelist` returns an absolute path unchanged even when
    /// the file doesn't exist on disk — callers use the path as a forward
    /// reference and we must not prefix it with the filelist directory.
    #[test]
    fn absolutise_filelist_absolute_path_returned_unchanged() {
        let dir = tempdir().unwrap();
        // Use a path that definitely does not exist.
        let abs = PathBuf::from("/nonexistent/absolute/path/file.sv");
        let result = absolutise_filelist(dir.path(), dir.path(), &abs);
        assert_eq!(result, abs, "absolute path must pass through unchanged");
    }

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
    // they parse, resolve, and yield at least one source file.  The tests
    // are skipped gracefully when the repos have not been cloned (e.g. in CI
    // that doesn't carry the gitignored directories).

    #[test]
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
