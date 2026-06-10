//! `textDocument/codeLens` — "overrides Base::method" lenses.
//!
//! Single responsibility: given a file's tree-sitter symbol index and the
//! workspace symbol index, produce one [`CodeLens`] per method that overrides
//! an ancestor method, with a command that jumps to the overridden
//! declaration. Tree-sitter only — no slang.
//!
//! The override target is found by walking the enclosing class's `extends`
//! chain (via [`Symbol::parent_class_name`] / the workspace index) for the
//! nearest ancestor that declares a method of the same name.

use serde_json::json;
use tower_lsp::lsp_types::{CodeLens, Command, Position, Range, Url};

use mimir_core::Range as MRange;
use mimir_syntax::{Symbol, SymbolKind};

use crate::workspace_index::WorkspaceIndex;

/// The `mimir.gotoLocation` client command the lens invokes: `[uri, position]`.
const GOTO_COMMAND: &str = "mimir.gotoLocation";

/// `[code_lens] overrides` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum OverrideLensMode {
    /// No override lenses (`"none"`).
    None,
    /// Only UVM phase-method overrides (`"uvm"`, the default).
    #[default]
    Uvm,
    /// Every method that overrides an ancestor (`"all"`).
    All,
}

impl OverrideLensMode {
    /// Parse the `[code_lens] overrides` string, defaulting to `Uvm` on any
    /// unrecognised value.
    pub(crate) fn from_config_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "all" => Self::All,
            "none" | "off" | "false" => Self::None,
            _ => Self::Uvm,
        }
    }
}

/// Build override CodeLenses for the file at `uri` (whose tree-sitter symbol
/// index is `index`), resolving targets against the workspace index `wi`.
pub(crate) fn override_lenses(
    index: &[Symbol],
    wi: &WorkspaceIndex,
    mode: OverrideLensMode,
) -> Vec<CodeLens> {
    if mode == OverrideLensMode::None {
        return Vec::new();
    }

    // Classes declared in this file, used to find each method's enclosing
    // class (the flat index doesn't store an owner back-reference).
    let classes: Vec<&Symbol> =
        index.iter().filter(|s| s.kind == SymbolKind::Class).collect();

    let mut out = Vec::new();
    for method in index.iter().filter(|s| s.kind == SymbolKind::Method) {
        if mode == OverrideLensMode::Uvm
            && !mimir_syntax::uvm::DEFAULT_UVM_PHASES.contains(&method.name.as_str())
        {
            continue;
        }
        let Some(class) = classes
            .iter()
            .find(|c| c.full_range.contains_range(method.name_range))
        else {
            continue;
        };
        let Some(parent) = class.parent_class_name.as_deref() else {
            continue;
        };
        if let Some((base_class, target_url, target)) =
            find_override(wi, parent, &method.name)
        {
            out.push(make_lens(method, &base_class, &target_url, &target));
        }
    }
    out
}

/// Walk from `start_class` up the inheritance chain looking for a method named
/// `method_name`. Returns `(declaring_class, file_url, method_symbol)` of the
/// nearest ancestor that declares it. Caps at 16 hops and cycle-detects.
fn find_override(
    wi: &WorkspaceIndex,
    start_class: &str,
    method_name: &str,
) -> Option<(String, Url, Symbol)> {
    let mut current = start_class.to_string();
    let mut visited = std::collections::HashSet::new();
    for _ in 0..16 {
        if !visited.insert(current.clone()) {
            return None; // inheritance cycle
        }
        let class_entry = wi
            .lookup(&current)
            .iter()
            .find(|e| e.symbol.kind == SymbolKind::Class)
            .cloned()?;
        let hit = wi.lookup(method_name).iter().find(|e| {
            e.url == class_entry.url
                && e.symbol.kind == SymbolKind::Method
                && class_entry.symbol.full_range.contains_range(e.symbol.full_range)
        });
        if let Some(entry) = hit {
            return Some((current, entry.url.clone(), entry.symbol.clone()));
        }
        current = class_entry.symbol.parent_class_name.clone()?;
    }
    None
}

/// Build a CodeLens at `method`'s name pointing at the overridden `target`.
fn make_lens(method: &Symbol, base_class: &str, target_url: &Url, target: &Symbol) -> CodeLens {
    CodeLens {
        range: to_lsp_range(method.name_range),
        command: Some(Command {
            title: format!("▷ overrides {base_class}::{}", method.name),
            command: GOTO_COMMAND.to_string(),
            arguments: Some(vec![
                json!(target_url.to_string()),
                json!({
                    "line": target.name_range.start.line,
                    "character": target.name_range.start.character,
                }),
            ]),
        }),
        data: None,
    }
}

fn to_lsp_range(r: MRange) -> Range {
    Range::new(
        Position::new(r.start.line, r.start.character),
        Position::new(r.end.line, r.end.character),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_syntax::{symbols::index, SyntaxParser};
    use ropey::Rope;

    /// Build a workspace index from one synthetic file and return both the
    /// file's symbols and the populated index.
    fn build(src: &str) -> (Vec<Symbol>, WorkspaceIndex, Url) {
        let mut p = SyntaxParser::new().unwrap();
        let t = p.parse(src, None).unwrap();
        let rope = Rope::from_str(t.source());
        let syms = index(&t, &rope);
        let url = Url::parse("file:///test.sv").unwrap();
        let mut wi = WorkspaceIndex::default();
        wi.update(url.clone(), &syms);
        (syms, wi, url)
    }

    const HIER: &str = "\
class base extends uvm_component;
  function void build_phase(uvm_phase phase);
  endfunction
  function void helper();
  endfunction
endclass
class derived extends base;
  function void build_phase(uvm_phase phase);
    super.build_phase(phase);
  endfunction
  function void helper();
  endfunction
endclass
";

    #[test]
    fn uvm_mode_lenses_only_phase_overrides() {
        let (syms, wi, _) = build(HIER);
        let lenses = override_lenses(&syms, &wi, OverrideLensMode::Uvm);
        // Only derived.build_phase overrides an ancestor phase; base's
        // build_phase has no parent declaring it, and helper isn't a phase.
        assert_eq!(lenses.len(), 1, "got {lenses:?}");
        let cmd = lenses[0].command.as_ref().unwrap();
        assert!(cmd.title.contains("overrides base::build_phase"), "title: {}", cmd.title);
        assert_eq!(cmd.command, GOTO_COMMAND);
    }

    #[test]
    fn all_mode_includes_non_phase_overrides() {
        let (syms, wi, _) = build(HIER);
        let lenses = override_lenses(&syms, &wi, OverrideLensMode::All);
        // derived overrides both build_phase and helper.
        let titles: Vec<&str> =
            lenses.iter().filter_map(|l| l.command.as_ref()).map(|c| c.title.as_str()).collect();
        assert_eq!(titles.len(), 2, "got {titles:?}");
        assert!(titles.iter().any(|t| t.contains("build_phase")));
        assert!(titles.iter().any(|t| t.contains("helper")));
    }

    #[test]
    fn none_mode_emits_nothing() {
        let (syms, wi, _) = build(HIER);
        assert!(override_lenses(&syms, &wi, OverrideLensMode::None).is_empty());
    }

    #[test]
    fn no_lens_for_non_overriding_method() {
        // A standalone class with no parent: its methods override nothing.
        let (syms, wi, _) = build(
            "class solo;\n  function void build_phase(uvm_phase phase);\n  endfunction\nendclass\n",
        );
        assert!(override_lenses(&syms, &wi, OverrideLensMode::All).is_empty());
    }

    #[test]
    fn target_points_at_base_declaration() {
        let (syms, wi, _) = build(HIER);
        let lenses = override_lenses(&syms, &wi, OverrideLensMode::Uvm);
        let args = lenses[0].command.as_ref().unwrap().arguments.as_ref().unwrap();
        // base.build_phase is declared on line 1 (0-indexed).
        assert_eq!(args[1]["line"], 1);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(OverrideLensMode::from_config_str("all"), OverrideLensMode::All);
        assert_eq!(OverrideLensMode::from_config_str("none"), OverrideLensMode::None);
        assert_eq!(OverrideLensMode::from_config_str("uvm"), OverrideLensMode::Uvm);
        assert_eq!(OverrideLensMode::from_config_str("garbage"), OverrideLensMode::Uvm);
    }
}
