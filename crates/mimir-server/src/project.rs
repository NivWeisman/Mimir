//! Project configuration for slang elaboration.
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
//! | `${VAR}` anywhere        | Expanded from the process environment.             |
//!
//! Recursion is bounded ([`FILELIST_MAX_DEPTH`]) and cycles are detected
//! by canonical path so a malformed `-f a.f -f a.f` doesn't loop forever.

use std::collections::HashSet;
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
            include_dirs: Vec::new(),
            defines: Vec::new(),
            top: None,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
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
        let cfg: ProjectConfig =
            toml::from_str(&text).map_err(|source| ProjectError::Toml {
                path: path.to_path_buf(),
                source,
            })?;
        let root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let mut files: Vec<PathBuf> = Vec::new();
        let mut include_dirs: Vec<PathBuf> = cfg
            .slang
            .include_dirs
            .iter()
            .map(|p| absolutise(&root, p))
            .collect();
        let mut defines: Vec<MacroDefine> =
            cfg.slang.defines.iter().map(|s| parse_define(s)).collect();

        if let Some(filelist) = cfg.slang.filelist.as_deref() {
            let absolute = absolutise(&root, filelist);
            let mut visited = HashSet::new();
            expand_filelist(
                &absolute,
                0,
                &mut visited,
                &mut files,
                &mut include_dirs,
                &mut defines,
            )?;
        }

        info!(
            root = %root.display(),
            files = files.len(),
            include_dirs = include_dirs.len(),
            defines = defines.len(),
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

/// Expand `${VAR}` references using the process environment. Unknown
/// variables expand to the empty string (matches GNU `make`'s behaviour
/// and what most simulators do). Bare `$VAR` (without braces) is left
/// alone — too easy to false-positive on a literal `$` in a path.
fn expand_env_vars(s: &str) -> String {
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
                if let Ok(value) = std::env::var(&name) {
                    out.push_str(&value);
                }
                // Unknown var → empty string.
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
/// unchanged. Doesn't canonicalise — that requires the path to exist on
/// disk and we're sometimes building paths ahead of `read_to_string`.
fn absolutise(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Recursively expand a filelist. Pushes results into the accumulator
/// vectors so a top-level filelist with five `-f` includes builds a
/// single flat output rather than a tree the caller has to walk.
///
/// `visited` carries the canonical paths we've already opened in this
/// expansion; a repeat visit fails with [`ProjectError::FilelistCycle`].
/// `depth` is checked against [`FILELIST_MAX_DEPTH`] before any work.
fn expand_filelist(
    path: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    include_dirs: &mut Vec<PathBuf>,
    defines: &mut Vec<MacroDefine>,
) -> Result<(), ProjectError> {
    if depth >= FILELIST_MAX_DEPTH {
        return Err(ProjectError::FilelistTooDeep {
            path: path.to_path_buf(),
            limit: FILELIST_MAX_DEPTH,
        });
    }

    // Canonicalise for cycle detection; fall back to the raw path on
    // platforms / cases where canonicalize fails (e.g. symlink loops we
    // didn't make ourselves), where the cycle check just devolves into
    // "exact path repeat."
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
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
                include_dirs.push(absolutise(&base, Path::new(&expand_env_vars(dir))));
            }
            i += 1;
        } else if let Some(rest) = token.strip_prefix("+define+") {
            for d in rest.split('+').filter(|s| !s.is_empty()) {
                defines.push(parse_define(&expand_env_vars(d)));
            }
            i += 1;
        } else if token == "-f" || token == "-F" {
            // Two-token form: `-f nested.f`.
            let Some(next) = tokens.get(i + 1) else {
                warn!("trailing `-f` with no filelist path; ignoring");
                break;
            };
            let nested = absolutise(&base, Path::new(&expand_env_vars(next)));
            expand_filelist(&nested, depth + 1, visited, files, include_dirs, defines)?;
            i += 2;
        } else if let Some(rest) = token.strip_prefix("-f") {
            // One-token form: `-fnested.f`.
            let nested = absolutise(&base, Path::new(&expand_env_vars(rest)));
            expand_filelist(&nested, depth + 1, visited, files, include_dirs, defines)?;
            i += 1;
        } else if let Some(rest) = token.strip_prefix("-F") {
            let nested = absolutise(&base, Path::new(&expand_env_vars(rest)));
            expand_filelist(&nested, depth + 1, visited, files, include_dirs, defines)?;
            i += 1;
        } else {
            files.push(absolutise(&base, Path::new(&expand_env_vars(token))));
            i += 1;
        }
    }
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

    /// `${VAR}` interpolates from the env; unknown vars become empty.
    /// `$BARE` is left alone (we only recognise the braced form).
    #[test]
    fn expand_env_vars_basic() {
        std::env::set_var("MIMIR_TEST_FOO", "hello");
        assert_eq!(expand_env_vars("${MIMIR_TEST_FOO}/x"), "hello/x");
        assert_eq!(expand_env_vars("${MIMIR_NOPE_NOPE}/y"), "/y");
        assert_eq!(expand_env_vars("$LITERAL"), "$LITERAL");
        assert_eq!(expand_env_vars("plain"), "plain");
        std::env::remove_var("MIMIR_TEST_FOO");
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
        let mut visited = HashSet::new();
        expand_filelist(&f, 0, &mut visited, &mut files, &mut incs, &mut defs).unwrap();

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
        let mut visited = HashSet::new();
        expand_filelist(&outer, 0, &mut visited, &mut files, &mut incs, &mut defs).unwrap();

        // Order is: outer.sv, inner.sv (from nested), after.sv. The nested
        // include lands between the outer files that bracket the `-f`.
        assert_eq!(files.len(), 3);
        assert!(files[0].ends_with("outer.sv"));
        assert!(files[1].ends_with("inner.sv"));
        assert!(files[2].ends_with("after.sv"));
        assert_eq!(incs.len(), 1);
        assert!(incs[0].ends_with("nested_inc"));
    }

    /// A filelist that `-f`-includes itself fails with `FilelistCycle`,
    /// not stack overflow.
    #[test]
    fn expand_filelist_cycle_detected() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("loop.f");
        fs::write(&f, "loop.sv\n-f loop.f\n").unwrap();

        let mut files = Vec::new();
        let mut incs = Vec::new();
        let mut defs = Vec::new();
        let mut visited = HashSet::new();
        let err = expand_filelist(&f, 0, &mut visited, &mut files, &mut incs, &mut defs)
            .expect_err("self-include should fail");
        assert!(matches!(err, ProjectError::FilelistCycle { .. }));
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
}
