//! Path-based diagnostic policy.
//!
//! Decides whether a diagnostic should be published unchanged, demoted to a
//! lower severity, or dropped — based on the file path it belongs to. The
//! motivating case: a project that pulls in a vendored UVM library gets
//! elaboration diagnostics for every transitively-`` `include ``d header, and
//! those drown out the diagnostics that matter (the user's own code). The
//! user can't (and won't) fix the library, but still wants its diagnostics
//! visible — just quiet.
//!
//! Configured under `[diagnostics]` in `.mimir.toml`:
//!
//! ```toml
//! [diagnostics]
//! # Files whose path contains any of these substrings get their diagnostics
//! # capped at `demote_severity` (default "hint") — still visible, but they
//! # don't show up as errors/warnings in the Problems panel.
//! demote_paths    = ["uvm-1.2", "vendor/"]
//! demote_severity = "hint"        # error | warning | information | hint
//!
//! # Files whose path contains any of these substrings get their diagnostics
//! # dropped entirely.
//! ignore_paths    = ["third_party/generated/"]
//! ```
//!
//! Patterns are matched as plain **substrings** of the file path (not globs):
//! `"uvm-1.2"` matches every file whose path contains `uvm-1.2`. `ignore`
//! takes precedence over `demote` when a path matches both.

use mimir_ast::DiagSeverity;
use tracing::warn;

/// What to do with a diagnostic, decided from its file path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagAction {
    /// Publish unchanged.
    Keep,
    /// Drop entirely — the path matched an `ignore_paths` pattern.
    Drop,
    /// Cap the severity at this floor — the path matched a `demote_paths`
    /// pattern. A diagnostic already this severe or less is left as-is.
    DemoteFloor(DiagSeverity),
}

/// Resolved `[diagnostics]` policy. Built once when the project config loads
/// and consulted per diagnostic at publish time.
#[derive(Debug, Clone)]
pub(crate) struct DiagnosticPolicy {
    demote_patterns: Vec<String>,
    demote_floor: DiagSeverity,
    ignore_patterns: Vec<String>,
}

impl Default for DiagnosticPolicy {
    fn default() -> Self {
        Self {
            demote_patterns: Vec::new(),
            demote_floor: DiagSeverity::Hint,
            ignore_patterns: Vec::new(),
        }
    }
}

impl DiagnosticPolicy {
    /// Build a policy from the raw `[diagnostics]` config fields. An
    /// unrecognised `demote_severity` string logs a warning and falls back
    /// to `Hint`.
    pub(crate) fn from_config(
        demote_paths: Vec<String>,
        demote_severity: &str,
        ignore_paths: Vec<String>,
    ) -> Self {
        let demote_floor = parse_severity(demote_severity).unwrap_or_else(|| {
            if !demote_severity.is_empty() {
                warn!(
                    value = demote_severity,
                    "[diagnostics] demote_severity not one of error/warning/information/hint; \
                     defaulting to hint",
                );
            }
            DiagSeverity::Hint
        });
        Self {
            demote_patterns: demote_paths.into_iter().filter(|p| !p.is_empty()).collect(),
            demote_floor,
            ignore_patterns: ignore_paths.into_iter().filter(|p| !p.is_empty()).collect(),
        }
    }

    /// Decide what to do with a diagnostic on `path`. `ignore` wins over
    /// `demote` when both match.
    pub(crate) fn action_for(&self, path: &str) -> DiagAction {
        if self.ignore_patterns.iter().any(|p| path.contains(p.as_str())) {
            return DiagAction::Drop;
        }
        if self.demote_patterns.iter().any(|p| path.contains(p.as_str())) {
            return DiagAction::DemoteFloor(self.demote_floor);
        }
        DiagAction::Keep
    }
}

/// Severity rank — lower is more severe (matches LSP's numeric ordering).
fn rank(s: DiagSeverity) -> u8 {
    match s {
        DiagSeverity::Error => 0,
        DiagSeverity::Warning => 1,
        DiagSeverity::Information => 2,
        DiagSeverity::Hint => 3,
    }
}

/// Cap `severity` at `floor`: if `severity` is more severe than `floor`,
/// return `floor`; otherwise return `severity` unchanged. (So demoting an
/// error to a hint floor yields a hint, but a hint stays a hint.)
pub(crate) fn demoted_severity(severity: DiagSeverity, floor: DiagSeverity) -> DiagSeverity {
    if rank(severity) < rank(floor) {
        floor
    } else {
        severity
    }
}

fn parse_severity(s: &str) -> Option<DiagSeverity> {
    match s.trim().to_ascii_lowercase().as_str() {
        "error" => Some(DiagSeverity::Error),
        "warning" | "warn" => Some(DiagSeverity::Warning),
        "information" | "info" => Some(DiagSeverity::Information),
        "hint" => Some(DiagSeverity::Hint),
        _ => None,
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_keeps_everything() {
        let p = DiagnosticPolicy::default();
        assert_eq!(p.action_for("/proj/uvm-1.2/src/base/uvm_object.svh"), DiagAction::Keep);
    }

    #[test]
    fn demote_path_caps_severity_at_floor() {
        let p = DiagnosticPolicy::from_config(vec!["uvm-1.2".into()], "hint", vec![]);
        match p.action_for("/work/vendor/uvm-1.2/src/uvm_pkg.sv") {
            DiagAction::DemoteFloor(f) => assert_eq!(f, DiagSeverity::Hint),
            other => panic!("expected DemoteFloor, got {other:?}"),
        }
        // A non-matching path is untouched.
        assert_eq!(p.action_for("/work/rtl/my_dut.sv"), DiagAction::Keep);
    }

    #[test]
    fn ignore_path_drops_and_wins_over_demote() {
        let p = DiagnosticPolicy::from_config(
            vec!["vendor/".into()],
            "warning",
            vec!["vendor/generated/".into()],
        );
        // Matches both demote (`vendor/`) and ignore (`vendor/generated/`) —
        // ignore wins.
        assert_eq!(p.action_for("/x/vendor/generated/foo.sv"), DiagAction::Drop);
        // Matches only demote.
        assert!(matches!(
            p.action_for("/x/vendor/uvm/bar.sv"),
            DiagAction::DemoteFloor(DiagSeverity::Warning)
        ));
    }

    #[test]
    fn demoted_severity_only_lowers_never_raises() {
        // Error → Hint floor = Hint (demoted).
        assert_eq!(demoted_severity(DiagSeverity::Error, DiagSeverity::Hint), DiagSeverity::Hint);
        // Hint → Warning floor = Hint (already less severe; not raised).
        assert_eq!(
            demoted_severity(DiagSeverity::Hint, DiagSeverity::Warning),
            DiagSeverity::Hint
        );
        // Warning → Warning floor = Warning (unchanged).
        assert_eq!(
            demoted_severity(DiagSeverity::Warning, DiagSeverity::Warning),
            DiagSeverity::Warning
        );
    }

    #[test]
    fn unknown_demote_severity_falls_back_to_hint() {
        let p = DiagnosticPolicy::from_config(vec!["x".into()], "bogus", vec![]);
        assert!(matches!(
            p.action_for("/x/file.sv"),
            DiagAction::DemoteFloor(DiagSeverity::Hint)
        ));
    }
}
