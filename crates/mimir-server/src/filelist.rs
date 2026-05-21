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
//! | `// rest of line`        | Comment.                                           |
//! | `# rest of line`         | Comment (alternate).                               |
//! | trailing `\` + newline   | Line continuation.                                 |
//! | `${VAR}` anywhere        | Expanded from config `[env]`, then the process environment. |
//!
//! Recursion is bounded ([`FILELIST_MAX_DEPTH`]) and cycles are detected
//! by canonical path, so a malformed `-f a.f -f a.f` doesn't loop forever.
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

/// Absolutise `p` against `base`. Already-absolute paths are returned
/// unchanged. For relative paths, tries `base.join(p)` first; if that path
/// does not exist on disk but `p` itself does (e.g. the raw string is
/// reachable from the current working directory or becomes absolute after
/// env-var expansion), the raw path is returned so callers don't silently
/// swallow a misconfigured TOML-relative prefix.
pub(crate) fn absolutise(base: &Path, p: &Path) -> PathBuf {
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
pub(crate) fn absolutise_filelist(filelist_base: &Path, toml_root: &Path, p: &Path) -> PathBuf {
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
        } else if let Some(rest) = token.strip_prefix("-f") {
            // One-token form: `-fnested.f`.
            let nested = absolutise_filelist(
                &base,
                toml_root,
                Path::new(&expand_env_vars(rest, env)),
            );
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
            i += 1;
        } else if let Some(rest) = token.strip_prefix("-F") {
            let nested = absolutise_filelist(
                &base,
                toml_root,
                Path::new(&expand_env_vars(rest, env)),
            );
            expand_filelist(&nested, depth + 1, toml_root, state, env)?;
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
