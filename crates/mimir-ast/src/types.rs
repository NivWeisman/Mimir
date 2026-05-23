//! Core AST types used by all Mimir backends.

use serde::{Deserialize, Serialize};

/// The complete elaboration result for a compilation unit.
///
/// A backend produces one `MimirAst` per successful compile call. Mimir caches
/// this and uses it to answer every LSP query until the next compile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MimirAst {
    /// One entry per file included in the compilation.
    pub files: Vec<MimirFile>,
}

impl MimirAst {
    /// Find the file entry for the given absolute path, if present.
    pub fn find_file(&self, uri: &str) -> Option<&MimirFile> {
        self.files.iter().find(|f| f.uri == uri)
    }
}

/// All data Mimir extracted for a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MimirFile {
    /// Absolute filesystem path as reported by the backend.
    pub uri: String,
    /// Parse and elaboration diagnostics for this file.
    pub diagnostics: Vec<MimirDiag>,
    /// Root lexical scope — contains the file's top-level declarations.
    pub top_scope: MimirScope,
}

/// A lexical scope: a region of source text that introduces a declaration
/// namespace.
///
/// Scopes nest: a class body contains scopes for each method body; a module
/// body contains scopes for each `begin`/`end` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MimirScope {
    /// Source range over which this scope is active.
    pub range: MimirRange,
    /// Symbols declared directly at this scope level.
    pub declarations: Vec<MimirDecl>,
    /// Nested child scopes (method bodies, begin/end blocks, …).
    pub children: Vec<MimirScope>,
    /// Packages imported at this scope level (`import pkg::*`).
    pub imported_packages: Vec<String>,
}

impl MimirScope {
    /// Return the innermost child scope whose range contains `pos`, or `self`
    /// if no child matches.
    pub fn innermost_at(&self, pos: MimirPos) -> &MimirScope {
        for child in &self.children {
            if child.range.contains(pos) {
                return child.innermost_at(pos);
            }
        }
        self
    }
}

/// A declaration: any named symbol introduced into a scope.
///
/// `members` holds nested declarations (class fields, module ports, enum
/// values, function parameters). This makes `MimirDecl` a recursive type,
/// but `Vec` already provides the necessary indirection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MimirDecl {
    /// Declared identifier text.
    pub name: String,
    /// What kind of declaration this is.
    pub kind: DeclKind,
    /// Source range of the name token.
    pub range: MimirRange,
    /// Source range of the entire declaration (for hover context).
    pub full_range: MimirRange,
    /// String representation of the declared type, e.g. `"logic [7:0]"`,
    /// `"MyClass"`, `"int unsigned"`. `None` for declarations that have no
    /// declared type (modules, packages, …).
    pub type_str: Option<String>,
    /// Nested declarations: class members, port lists, enum values, …
    pub members: Vec<MimirDecl>,
    /// For classes: the name of the base class (`extends X`).
    pub parent_class: Option<String>,
    /// Access visibility. Defaults to `Public` for non-class members.
    pub visibility: Visibility,
    /// Leading doc comment, if the backend extracted one.
    pub doc: Option<String>,
}

/// The syntactic category of a declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeclKind {
    /// `module`
    Module,
    /// `interface`
    Interface,
    /// `program`
    Program,
    /// `package`
    Package,
    /// `class`
    Class,
    /// `function`
    Function,
    /// `task`
    Task,
    /// `property`
    Property,
    /// `sequence`
    Sequence,
    /// `covergroup`
    Covergroup,
    /// Module/interface port.
    Port,
    /// `parameter` (overridable).
    Parameter,
    /// `localparam` (not overridable).
    LocalParam,
    /// Variable (`logic`, `reg`, `wire`, …).
    Variable,
    /// Class field.
    Field,
    /// `typedef`.
    Typedef,
    /// `enum` type declaration.
    Enum,
    /// An `enum` member value.
    EnumMember,
    /// `` `define `` macro.
    Macro,
}

/// Access visibility for class members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    /// Visible to all code.
    #[default]
    Public,
    /// Visible within the class and its subclasses.
    Protected,
    /// Visible only within the declaring class.
    Local,
}

/// A single diagnostic emitted by a backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MimirDiag {
    /// Source range the diagnostic squiggles cover.
    pub range: MimirRange,
    /// How severe the diagnostic is.
    pub severity: DiagSeverity,
    /// Stable diagnostic code (e.g. `"UnknownModule"`).
    pub code: String,
    /// Human-readable description.
    pub message: String,
}

/// Severity of a [`MimirDiag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagSeverity {
    /// Compilation-blocking error.
    Error,
    /// Non-fatal but likely wrong.
    Warning,
    /// Informational note.
    Information,
    /// Suggestion or style hint.
    Hint,
}

/// A half-open source range `[start, end)`.
///
/// Both positions use UTF-16 code units for the `character` field, matching
/// the LSP wire format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimirRange {
    /// Inclusive start.
    pub start: MimirPos,
    /// Exclusive end.
    pub end: MimirPos,
}

impl MimirRange {
    /// Return `true` if `pos` falls within `[start, end)`.
    pub fn contains(self, pos: MimirPos) -> bool {
        (self.start.line < pos.line
            || (self.start.line == pos.line && self.start.character <= pos.character))
            && (pos.line < self.end.line
                || (pos.line == self.end.line && pos.character < self.end.character))
    }
}

/// A zero-width point in source text.
///
/// `line` and `character` are both 0-based. `character` counts UTF-16 code
/// units (the LSP convention), not bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimirPos {
    /// 0-based line number.
    pub line: u32,
    /// 0-based column in UTF-16 code units.
    pub character: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, ch: u32) -> MimirPos {
        MimirPos { line, character: ch }
    }

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> MimirRange {
        MimirRange { start: pos(sl, sc), end: pos(el, ec) }
    }

    #[test]
    fn range_contains_start() {
        assert!(range(1, 5, 3, 10).contains(pos(1, 5)));
    }

    #[test]
    fn range_excludes_end() {
        assert!(!range(1, 5, 3, 10).contains(pos(3, 10)));
    }

    #[test]
    fn range_contains_interior() {
        assert!(range(1, 0, 5, 0).contains(pos(3, 7)));
    }

    #[test]
    fn range_excludes_before_start() {
        assert!(!range(1, 5, 3, 10).contains(pos(1, 4)));
    }

    #[test]
    fn roundtrip_serialize() {
        let ast = MimirAst {
            files: vec![MimirFile {
                uri: "/tmp/foo.sv".to_string(),
                diagnostics: vec![],
                top_scope: MimirScope {
                    range: range(0, 0, 10, 0),
                    declarations: vec![MimirDecl {
                        name: "foo".to_string(),
                        kind: DeclKind::Module,
                        range: range(0, 7, 0, 10),
                        full_range: range(0, 0, 9, 0),
                        type_str: None,
                        members: vec![],
                        parent_class: None,
                        visibility: Visibility::Public,
                        doc: None,
                    }],
                    children: vec![],
                    imported_packages: vec![],
                },
            }],
        };
        let json = serde_json::to_string(&ast).expect("serialize");
        let back: MimirAst = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].top_scope.declarations[0].name, "foo");
    }

    #[test]
    fn innermost_scope_finds_child() {
        let inner = MimirScope {
            range: range(2, 0, 4, 0),
            declarations: vec![],
            children: vec![],
            imported_packages: vec![],
        };
        let outer = MimirScope {
            range: range(0, 0, 10, 0),
            declarations: vec![],
            children: vec![inner],
            imported_packages: vec![],
        };
        let found = outer.innermost_at(pos(3, 5));
        assert_eq!(found.range, range(2, 0, 4, 0));
    }

    #[test]
    fn innermost_scope_returns_self_when_no_child_matches() {
        let outer = MimirScope {
            range: range(0, 0, 10, 0),
            declarations: vec![],
            children: vec![],
            imported_packages: vec![],
        };
        let found = outer.innermost_at(pos(5, 0));
        assert_eq!(found.range, range(0, 0, 10, 0));
    }

    #[test]
    fn find_file_returns_none_for_unknown_uri() {
        let ast = MimirAst::default();
        assert!(ast.find_file("/nonexistent.sv").is_none());
    }
}
