//! Filelist (`.f` file) tokenization, path resolution, and `${VAR}` expansion.
//!
//! The `.f` format is the verification-industry standard, used by VCS, Xcelium,
//! Questa, and Verilator. Each whitespace-separated token is one of:
//!
//! | Token                    | Meaning                                            |
//! | ------------------------ | -------------------------------------------------- |
//! | `path/to/file.sv`        | Source file to compile. Relative to the `.f`'s dir. |
//! | `+incdir+A[+B+...]`      | One or more include search paths, `+`-separated.   |
//! | `+define+NAME[=VALUE]`   | Predefine a macro (multiple `+`-separated allowed).|
//! | `-f nested.f`            | Recursively read another filelist.                 |
//! | other `-flag`/`+plusarg` | Simulator option we don't consume — skipped with a warning. |
//! | `// rest of line`        | Comment — only when it *starts* a token (`a//b.sv` is a path). |
//! | `# rest of line`         | Comment (alternate); same token-start rule.        |
//! | trailing `\` + newline   | Line continuation.                                 |
//! | `${VAR}` anywhere        | Expanded from config `[env]`, then the process environment. |
//!
//! Recursion is bounded ([`FILELIST_MAX_DEPTH`]) and cycles are detected
//! by canonical path, so a malformed `-f a.f -f a.f` doesn't loop forever.
//!
//! Every resolved path is lexically normalised ([`normalize_lexically`]) so
//! it compares equal to the path an editor reports for the same file.
//!
//! The public entry point is [`expand_filelist_to_parts`]. Lower-level
//! primitives ([`expand_env_vars`], [`absolutise`], [`parse_define`]) are
//! `pub(crate)` so [`crate::project::ResolvedProject::load`] can apply them
//! to inline TOML entries as well.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use mimir_slang::MacroDefine;
use tracing::{debug, warn};

use crate::project::ProjectError;

/// Maximum nesting depth for `-f` recursion. Real projects rarely nest
/// more than two or three levels; 16 is a comfortable ceiling that still
/// catches misconfiguration before we exhaust the stack.
pub(crate) const FILELIST_MAX_DEPTH: usize = 16;

/// Simulator flags that take a separate argument token. When one of these
/// is skipped, its argument is skipped with it so the argument isn't
/// misread as a source file (`-y ./libs` would otherwise add `./libs`).
const FLAGS_WITH_ARG: &[&str] = &["-y", "-v", "-top", "-l", "-sv_lib"];

/// True when a glued `-fPATH`/`-FPATH` token is a nested-filelist
/// reference rather than a simulator flag. The two-token `-f path` form is
/// canonical; the glued form is accepted only when the remainder names a
/// `.f`/`.F` file, so flags like `-full64` are not misread as `-f ull64`
/// (which used to fail the whole project load with a missing-file error).
fn glued_filelist_path(token: &str) -> Option<&str> {
    let rest = token
        .strip_prefix("-f")
        .or_else(|| token.strip_prefix("-F"))?;
    if rest.ends_with(".f") || rest.ends_with(".F") {
        Some(rest)
    } else {
        None
    }
}

/// Tokenise a `.f` filelist body. Handles `//` and `#` line comments,
/// backslash-newline line continuation, and ASCII whitespace as the token
/// separator. Quoted strings aren't recognised — they're rare in `.f`
/// files and we'd need to make a call about whether `+`-splitting still
/// applies. Easy to extend later if real projects need it.
pub(crate) fn tokenise_filelist(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // `//` comment to EOL — only at a token boundary. A `//` in the
            // middle of a token is part of a path (`$ROOT//rtl/top.sv`).
            '/' if current.is_empty() && chars.peek() == Some(&'/') => {
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            // `#` comment to EOL. Common in hand-written filelists. Same
            // token-boundary rule as `//`.
            '#' if current.is_empty() => {
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
pub(crate) fn expand_env_vars(s: &str, env: &HashMap<String, String>) -> String {
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
/// structured [`MacroDefine`] the wire protocol carries. Splits on the
/// *first* `=` so `EXPR=A=B` → name=`EXPR`, value=`A=B`.
pub(crate) fn parse_define(s: &str) -> MacroDefine {
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

/// Remove `.` components and resolve `..` against the preceding component,
/// purely textually (no filesystem access, symlinks not followed).
///
/// Project paths come out of `.mimir.toml` and filelists as written —
/// `../../src/uvm.sv` joined onto a base directory. Editors identify the
/// same file by its normalised path, and everything in the server that
/// pairs "a project file" with "an open buffer" does so by comparing paths:
/// the open-buffer override in the elaborate request, the AST lookup by
/// file, the URL diagnostics are published under. An un-normalised path
/// makes all of those miss.
pub(crate) fn normalize_lexically(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                match out.components().next_back() {
                    // `a/b/..` → `a`
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    // `/..` → `/` (can't climb above the root)
                    Some(Component::RootDir | Component::Prefix(_)) => {}
                    // Relative path that starts with (or has run out of
                    // components to cancel against) `..`: keep it.
                    Some(Component::ParentDir | Component::CurDir) | None => out.push(".."),
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Normalise a resolved project path, unless doing so would change which
/// file it names.
///
/// Lexical `..` removal and the OS disagree when a symlink sits in front of
/// the `..` (`/a/link/../b` is `/<link target's parent>/b` on disk but
/// `/a/b` lexically). In that one case we keep the path as written — a
/// working-but-unnormalised path beats a normalised one pointing nowhere.
fn normalized_if_equivalent(p: PathBuf) -> PathBuf {
    let norm = normalize_lexically(&p);
    if norm.exists() || !p.exists() {
        norm
    } else {
        debug!(
            path = %p.display(),
            "keeping un-normalised path: lexical normalisation would miss the file (symlink + `..`?)",
        );
        p
    }
}

/// Make `p` absolute relative to `base` (the `.mimir.toml` directory) and
/// normalise it (see [`normalize_lexically`] for why that matters).
///
/// Falls back to the path as written (relative to the server's CWD) when the
/// joined path doesn't exist but the raw one does.
pub(crate) fn absolutise(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return normalized_if_equivalent(p.to_path_buf());
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
        normalized_if_equivalent(joined)
    }
}

/// Resolve a path found inside a filelist: relative to the filelist's own
/// directory first, then the `.mimir.toml` root, then as written. The result
/// is normalised like [`absolutise`]'s.
pub(crate) fn absolutise_filelist(filelist_base: &Path, toml_root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return normalized_if_equivalent(p.to_path_buf());
    }
    let joined = filelist_base.join(p);
    if joined.exists() {
        return normalized_if_equivalent(joined);
    }
    if filelist_base != toml_root {
        let via_root = toml_root.join(p);
        if via_root.exists() {
            debug!(
                path = %p.display(),
                via = %via_root.display(),
                "path not found relative to filelist dir; resolved via TOML root"
            );
            return normalized_if_equivalent(via_root);
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
    normalized_if_equivalent(joined)
}

/// Expanded results from a `.f` filelist tree.
#[derive(Debug)]
pub(crate) struct FilelistParts {
    /// Source files in declaration order.
    pub files: Vec<PathBuf>,
    /// `+incdir+` paths in declaration order.
    pub include_dirs: Vec<PathBuf>,
    /// `+define+` macros in declaration order.
    pub defines: Vec<MacroDefine>,
}

/// Expand a filelist at `path` and collect all source files, include dirs,
/// and defines into a [`FilelistParts`].
///
/// `toml_root` is the directory containing the `.mimir.toml` — used as a
/// secondary search base when a token resolves relative to the project root
/// rather than the `.f`'s own directory (common in team-shared filelists).
/// `env` is the config-provided `[env]` map (already multi-pass expanded).
pub(crate) fn expand_filelist_to_parts(
    path: &Path,
    toml_root: &Path,
    env: &HashMap<String, String>,
) -> Result<FilelistParts, ProjectError> {
    let mut files = Vec::new();
    let mut include_dirs = Vec::new();
    let mut defines = Vec::new();
    let mut in_progress = HashSet::new();
    let mut done = HashSet::new();
    expand_filelist(
        path,
        0,
        toml_root,
        &mut FilelistWalkState {
            in_progress: &mut in_progress,
            done: &mut done,
            files: &mut files,
            include_dirs: &mut include_dirs,
            defines: &mut defines,
        },
        env,
    )?;
    Ok(FilelistParts {
        files,
        include_dirs,
        defines,
    })
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
                state.include_dirs.push(absolutise_filelist(
                    &base,
                    toml_root,
                    Path::new(&expand_env_vars(dir, env)),
                ));
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
            let nested = absolutise_filelist(
                &base,
                toml_root,
                Path::new(&expand_env_vars(next, env)),
            );
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
            i += 2;
        } else if let Some(rest) = glued_filelist_path(token) {
            // One-token form: `-fnested.f` (only when the rest names a
            // `.f` file — see `glued_filelist_path`).
            let nested = absolutise_filelist(
                &base,
                toml_root,
                Path::new(&expand_env_vars(rest, env)),
            );
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
            i += 1;
        } else if token.starts_with('-') {
            // A simulator flag we don't consume (`-full64`, `-sverilog`,
            // `-y <dir>`, …). Skip it — and its argument for flags known
            // to take one — rather than misreading it as a source file.
            let takes_arg = FLAGS_WITH_ARG.contains(&token.as_str());
            warn!(
                token = %token,
                skipped_argument = takes_arg,
                "unrecognised simulator flag in filelist; skipping"
            );
            i += if takes_arg { 2 } else { 1 };
        } else if token.starts_with('+') {
            // A plusarg we don't consume (`+libext+.v+.sv`,
            // `+systemverilogext+`, …). Same treatment as unknown flags.
            warn!(token = %token, "unrecognised plusarg in filelist; skipping");
            i += 1;
        } else {
            state.files.push(absolutise_filelist(
                &base,
                toml_root,
                Path::new(&expand_env_vars(token, env)),
            ));
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
    use crate::project::ProjectError;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

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

    // ------------------------------------------------------------------
    // Regression tests
    // ------------------------------------------------------------------

    /// Regression: project paths were kept exactly as written, `..` and all
    /// (`/proj/tb/../rtl/a.sv`). The editor identifies the same file by its
    /// normalised path (`/proj/rtl/a.sv`), so the two never compared equal:
    /// the open buffer wasn't recognised as a project file (slang compiled
    /// the stale on-disk text *and* got the buffer as a second, duplicate
    /// file), the AST entry couldn't be found by the editor's path, and
    /// diagnostics went out under a URL containing `..`.
    #[test]
    fn regression_project_paths_are_lexically_normalised() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj").join("tb");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(tmp.path().join("proj").join("rtl")).unwrap();
        fs::write(tmp.path().join("proj").join("rtl").join("a.sv"), "").unwrap();
        let want = tmp.path().join("proj").join("rtl").join("a.sv");

        let via_toml = absolutise(&root, Path::new("../rtl/./a.sv"));
        assert_eq!(via_toml.to_str(), want.to_str(), "absolutise must resolve `..` and `.`");

        let via_filelist = absolutise_filelist(&root, &root, Path::new("./../rtl/a.sv"));
        assert_eq!(via_filelist.to_str(), want.to_str());

        // Already-absolute paths are normalised too.
        let abs = root.join("..").join("rtl").join("a.sv");
        assert_eq!(absolutise(&root, &abs).to_str(), want.to_str());

        // `..` can't climb above the filesystem root.
        assert_eq!(normalize_lexically(Path::new("/../../x.sv")), PathBuf::from("/x.sv"));
    }

    /// End to end through a real filelist: `+incdir+` and source entries
    /// written with `..` come out normalised.
    #[test]
    fn regression_filelist_entries_with_dotdot_are_normalised() {
        let tmp = tempdir().unwrap();
        let tb = tmp.path().join("tb");
        let src = tmp.path().join("src");
        fs::create_dir_all(&tb).unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("uvm.sv"), "").unwrap();
        let f = tb.join("files.f");
        fs::write(&f, "+incdir+../src\n../src/uvm.sv\n").unwrap();

        let parts = expand_filelist_to_parts(&f, &tb, &HashMap::new()).unwrap();
        assert_eq!(parts.files, vec![src.join("uvm.sv")]);
        assert_eq!(parts.files[0].to_str(), src.join("uvm.sv").to_str(), "no `..` left in the text");
        assert_eq!(parts.include_dirs[0].to_str(), src.to_str());
    }

    /// Regression: a `//` *inside* a token (`$ROOT//rtl/top.sv`, a doubled
    /// slash from joining path pieces) was taken as the start of a comment
    /// and truncated the path; likewise `#` inside a token. Comments start
    /// only at a token boundary.
    #[test]
    fn regression_tokeniser_keeps_double_slash_inside_a_path() {
        let tokens = tokenise_filelist("rtl//core/top.sv // real comment\nlib#1/a.sv # note\n");
        assert_eq!(tokens, vec!["rtl//core/top.sv".to_string(), "lib#1/a.sv".to_string()]);
    }

    /// Tokeniser recognises whitespace, both comment styles, and
    /// backslash-newline continuation.
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

    /// `absolutise` falls back to the path as-is when the base-relative
    /// joined path does not exist but the path itself does.
    #[test]
    fn absolutise_falls_back_when_joined_missing() {
        let dir = tempdir().unwrap();
        let fake_base = dir.path().join("nonexistent_subdir");
        let real_file = dir.path().join("real.sv");
        fs::write(&real_file, "").unwrap();
        let result = absolutise(&fake_base, &real_file);
        assert_eq!(result, real_file);
    }

    /// `absolutise_filelist` returns an absolute path unchanged even when
    /// the file doesn't exist on disk — callers use it as a forward reference.
    #[test]
    fn absolutise_filelist_absolute_path_returned_unchanged() {
        let dir = tempdir().unwrap();
        let abs = PathBuf::from("/nonexistent/absolute/path/file.sv");
        let result = absolutise_filelist(dir.path(), dir.path(), &abs);
        assert_eq!(result, abs, "absolute path must pass through unchanged");
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

        let parts = expand_filelist_to_parts(&f, dir.path(), &HashMap::new()).unwrap();

        assert_eq!(parts.files.len(), 2);
        assert!(parts.files[0].ends_with("a.sv"));
        assert!(parts.files[1].ends_with("sub/b.sv"));
        assert_eq!(parts.include_dirs.len(), 2);
        assert!(parts.include_dirs[0].ends_with("inc"));
        assert!(parts.include_dirs[1].ends_with("other"));
        assert_eq!(parts.defines.len(), 2);
        assert_eq!(parts.defines[0].name, "UVM_NO_DPI");
        assert!(parts.defines[0].value.is_none());
        assert_eq!(parts.defines[1].name, "BUS");
        assert_eq!(parts.defines[1].value.as_deref(), Some("32"));
    }

    /// `-f nested.f` includes nested directives in declaration order.
    #[test]
    fn expand_filelist_recursion() {
        let dir = tempdir().unwrap();
        let outer = dir.path().join("outer.f");
        let inner = dir.path().join("inner.f");
        fs::write(&inner, "inner.sv\n+incdir+nested_inc\n").unwrap();
        fs::write(&outer, "outer.sv\n-f inner.f\nafter.sv\n").unwrap();

        let parts = expand_filelist_to_parts(&outer, dir.path(), &HashMap::new()).unwrap();

        // Order is: outer.sv, inner.sv (from nested), after.sv.
        assert_eq!(parts.files.len(), 3);
        assert!(parts.files[0].ends_with("outer.sv"));
        assert!(parts.files[1].ends_with("inner.sv"));
        assert!(parts.files[2].ends_with("after.sv"));
        assert_eq!(parts.include_dirs.len(), 1);
        assert!(parts.include_dirs[0].ends_with("nested_inc"));
    }

    /// Paths in a filelist that don't exist relative to the `.f`'s directory
    /// but do exist relative to the TOML root are resolved via the TOML root.
    #[test]
    fn expand_filelist_falls_back_to_toml_root() {
        let dir = tempdir().unwrap();
        let sim = dir.path().join("sim");
        fs::create_dir_all(&sim).unwrap();
        let rtl = dir.path().join("rtl");
        fs::create_dir_all(&rtl).unwrap();
        fs::write(rtl.join("dut.sv"), "").unwrap();

        let f = sim.join("project.f");
        fs::write(&f, "rtl/dut.sv\n").unwrap();

        let parts = expand_filelist_to_parts(&f, dir.path(), &HashMap::new()).unwrap();

        assert_eq!(parts.files.len(), 1);
        assert!(
            parts.files[0].ends_with("rtl/dut.sv"),
            "expected TOML-root fallback path, got {:?}",
            parts.files[0]
        );
    }

    /// A filelist that `-f`-includes itself (direct self-loop) fails with
    /// `FilelistCycle`, not stack overflow.
    #[test]
    fn expand_filelist_direct_cycle_is_error() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("loop.f");
        fs::write(&f, "loop.sv\n-f loop.f\n").unwrap();

        let err = expand_filelist_to_parts(&f, dir.path(), &HashMap::new())
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

        let err = expand_filelist_to_parts(&a, dir.path(), &HashMap::new())
            .expect_err("indirect cycle should fail");
        assert!(matches!(err, ProjectError::FilelistCycle { .. }));
    }

    /// Two sibling filelists that both `-f` the same shared filelist is a
    /// valid diamond reference — the second occurrence warns and skips rather
    /// than erroring. Files from the shared filelist appear exactly once.
    #[test]
    fn expand_filelist_diamond_repeat_warns_and_skips() {
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared.f");
        fs::write(&shared, "shared.sv\n+incdir+shared_inc\n").unwrap();

        let left = dir.path().join("left.f");
        let right = dir.path().join("right.f");
        fs::write(&left, "left.sv\n-f shared.f\n").unwrap();
        fs::write(&right, "right.sv\n-f shared.f\n").unwrap();

        let root = dir.path().join("root.f");
        fs::write(&root, "-f left.f\n-f right.f\n").unwrap();

        let parts = expand_filelist_to_parts(&root, dir.path(), &HashMap::new())
            .expect("diamond reference should succeed");

        // left.sv, shared.sv (first visit), right.sv — shared.f skipped on second visit.
        assert_eq!(parts.files.len(), 3, "got {:?}", parts.files);
        assert!(parts.files.iter().any(|p| p.ends_with("left.sv")));
        assert!(parts.files.iter().any(|p| p.ends_with("shared.sv")));
        assert!(parts.files.iter().any(|p| p.ends_with("right.sv")));
        // shared_inc appears exactly once.
        assert_eq!(parts.include_dirs.len(), 1);
        assert!(parts.include_dirs[0].ends_with("shared_inc"));
    }

    /// Simulator flags that merely *start* with `-f` (e.g. VCS's `-full64`)
    /// are skipped — they must not be misread as a nested filelist named
    /// `ull64` (which used to fail the entire project load) nor as a
    /// source file.
    #[test]
    fn expand_filelist_skips_simulator_flags() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("project.f");
        fs::write(&f, "-full64\n-sverilog\na.sv\n--some-long-flag\n").unwrap();

        let parts = expand_filelist_to_parts(&f, dir.path(), &HashMap::new())
            .expect("flags must not break the filelist");

        assert_eq!(parts.files.len(), 1, "got {:?}", parts.files);
        assert!(parts.files[0].ends_with("a.sv"));
    }

    /// Flags known to take a separate argument (`-y`, `-v`, …) skip the
    /// argument too, so a library path isn't misread as a source file.
    #[test]
    fn expand_filelist_skips_flag_arguments() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("project.f");
        fs::write(&f, "-y ./libs\n-v lib_cells.v\na.sv\n").unwrap();

        let parts = expand_filelist_to_parts(&f, dir.path(), &HashMap::new()).unwrap();

        assert_eq!(parts.files.len(), 1, "got {:?}", parts.files);
        assert!(parts.files[0].ends_with("a.sv"));
    }

    /// Unrecognised plusargs (`+libext+.v`, …) are skipped, not pushed as
    /// source files. `+incdir+`/`+define+` keep working alongside.
    #[test]
    fn expand_filelist_skips_unknown_plusargs() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("project.f");
        fs::write(&f, "+libext+.v+.sv\n+incdir+inc\na.sv\n").unwrap();

        let parts = expand_filelist_to_parts(&f, dir.path(), &HashMap::new()).unwrap();

        assert_eq!(parts.files.len(), 1, "got {:?}", parts.files);
        assert!(parts.files[0].ends_with("a.sv"));
        assert_eq!(parts.include_dirs.len(), 1);
        assert!(parts.include_dirs[0].ends_with("inc"));
    }

    /// The glued one-token form `-fnested.f` still recurses when the rest
    /// names a `.f` file.
    #[test]
    fn expand_filelist_glued_form_still_recurses() {
        let dir = tempdir().unwrap();
        let inner = dir.path().join("inner.f");
        fs::write(&inner, "inner.sv\n").unwrap();
        let outer = dir.path().join("outer.f");
        fs::write(&outer, "-finner.f\nouter.sv\n").unwrap();

        let parts = expand_filelist_to_parts(&outer, dir.path(), &HashMap::new()).unwrap();

        assert_eq!(parts.files.len(), 2, "got {:?}", parts.files);
        assert!(parts.files[0].ends_with("inner.sv"));
        assert!(parts.files[1].ends_with("outer.sv"));
    }

    /// The same filelist referenced twice at the top level also warns-and-skips
    /// on the second reference.
    #[test]
    fn expand_filelist_top_level_repeat_warns_and_skips() {
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared.f");
        fs::write(&shared, "shared.sv\n").unwrap();

        let root = dir.path().join("root.f");
        fs::write(&root, "-f shared.f\n-f shared.f\n").unwrap();

        let parts = expand_filelist_to_parts(&root, dir.path(), &HashMap::new())
            .expect("repeat top-level reference should succeed");

        // shared.sv must appear exactly once.
        assert_eq!(parts.files.len(), 1);
        assert!(parts.files[0].ends_with("shared.sv"));
    }
}
