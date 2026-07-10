//! `mimir/uvmDb` grouping + wire response for the UVM db viewer.
//!
//! The custom `mimir/uvmDb` request returns every
//! `uvm_config_db#(T)::set/get/…` and `uvm_resource_db#(T)::…` call in the
//! workspace, grouped by `(database, key)` so writers pair up with readers.
//! Two verification flags fall out of the grouping for free and are the
//! real product value:
//!
//! * `unwritten_read` — somebody `get`s a key nobody `set`s (classic
//!   silent-miss: the `get` returns 0 and the test limps on misconfigured).
//! * `type_mismatch` — the same key is accessed with different `#(T)`
//!   parameters (a `set` the `get` will never see: config_db lookups are
//!   type-keyed).
//!
//! All functions are sync — the handler in `backend.rs` snapshots trees
//! under locks, drops the locks, then delegates here (same contract as
//! `hierarchy_features`). The wire structs mirror the response shape the
//! VS Code tree view consumes; the scan itself lives in
//! `mimir_syntax::config_db`.

use std::collections::BTreeMap;

use mimir_syntax::{
    config_db::{db_calls, DbAccess, DbCall, DbKind},
    SyntaxTree,
};
use ropey::Rope;
use tower_lsp::lsp_types::{Range, Url};

use crate::slang_service::m_range_to_lsp;

/// Hard cap on total entries in one response. A pathological workspace
/// (generated testbenches) can hold tens of thousands of db calls; past
/// this point the tree view is unusable anyway, so we stop and set
/// `truncated` instead of building an unbounded payload.
const MAX_DB_ENTRIES: usize = 5_000;

// --------------------------------------------------------------------------
// Wire types
// --------------------------------------------------------------------------

/// Params for `mimir/uvmDb`. Empty today; a struct (rather than no params)
/// so future filters — key substring, kind — stay backward-compatible.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct UvmDbParams {}

/// Response for `mimir/uvmDb`: the workspace's db calls, grouped.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UvmDbResponse {
    /// One group per `(database, key)`, deterministically ordered
    /// (config before resource, then key-sorted).
    pub groups: Vec<UvmDbGroup>,
    /// Calls that can't be grouped: non-literal key expressions and
    /// `read_by_type` (which has no name argument).
    pub ungrouped: Vec<UvmDbEntry>,
    /// True when [`MAX_DB_ENTRIES`] was hit and the scan stopped early.
    pub truncated: bool,
}

/// All writers and readers of one `(database, key)` pair.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UvmDbGroup {
    /// `"config"` or `"resource"`.
    pub db: String,
    /// The shared key: config_db `field_name` / resource_db `name`.
    pub key: String,
    /// Distinct `#(T)` texts seen across the group, sorted.
    pub type_params: Vec<String>,
    /// True when more than one distinct `#(T)` accesses this key — a read
    /// with a different type parameter never sees the write.
    pub type_mismatch: bool,
    /// True when the key has readers but no writer anywhere in the scan.
    pub unwritten_read: bool,
    /// Display-ready summary, e.g. `config_db #(int) — 1 writer / 2 readers`.
    pub detail: String,
    /// `set` / `write_by_name` calls, sorted by (uri, line, character).
    pub writers: Vec<UvmDbEntry>,
    /// `get` / `exists` / `wait_modified` / `read_by_name` calls, same order.
    pub readers: Vec<UvmDbEntry>,
}

/// One db call site, display-ready for the tree view.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UvmDbEntry {
    /// `"config"` or `"resource"`.
    pub db: String,
    /// Method name as written: `set`, `get`, `read_by_name`, ….
    pub method: String,
    /// `"write"` or `"read"`.
    pub access: String,
    /// Source text of `#(T)`; empty when absent.
    pub type_param: String,
    /// Pairing key when it was a string literal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// config_db instance-path argument when it was a string literal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inst_name: Option<String>,
    /// resource_db scope argument when it was a string literal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// config_db context argument, raw text (`this`, `null`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cntxt: Option<String>,
    /// `file://` URI of the file containing the call.
    pub uri: String,
    /// Range of the method-name token — the jump target.
    pub range: Range,
    /// Range of the whole call expression.
    pub call_range: Range,
    /// Display-ready leaf label, e.g. `get  "env.agent"`.
    pub label: String,
    /// Display-ready location, e.g. `apb_env.sv:27` (basename, 1-based).
    pub file_line: String,
}

// --------------------------------------------------------------------------
// Collection
// --------------------------------------------------------------------------

/// Scan every tree for db calls and fold them into the grouped response.
pub(crate) fn collect_uvm_db(trees: &[(Url, SyntaxTree)]) -> UvmDbResponse {
    // BTreeMap keys give the deterministic group order the tree view (and
    // the tests) rely on: DbKind derives Ord with ConfigDb < ResourceDb.
    let mut groups: BTreeMap<(DbKind, String), (Vec<UvmDbEntry>, Vec<UvmDbEntry>)> =
        BTreeMap::new();
    let mut ungrouped: Vec<UvmDbEntry> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;

    'trees: for (url, tree) in trees {
        let rope = Rope::from_str(tree.source());
        for call in db_calls(tree, &rope) {
            if total >= MAX_DB_ENTRIES {
                truncated = true;
                break 'trees;
            }
            total += 1;

            let entry = entry_from_call(&call, url);
            match &call.key {
                Some(key) => {
                    let slot = groups
                        .entry((call.kind, key.clone()))
                        .or_insert_with(|| (Vec::new(), Vec::new()));
                    match call.access {
                        DbAccess::Write => slot.0.push(entry),
                        DbAccess::Read => slot.1.push(entry),
                    }
                }
                None => ungrouped.push(entry),
            }
        }
    }

    let groups = groups
        .into_iter()
        .map(|((kind, key), (mut writers, mut readers))| {
            sort_entries(&mut writers);
            sort_entries(&mut readers);

            let mut type_params: Vec<String> = writers
                .iter()
                .chain(readers.iter())
                .map(|e| e.type_param.clone())
                .collect();
            type_params.sort();
            type_params.dedup();

            let type_mismatch = type_params.len() > 1;
            let unwritten_read = writers.is_empty() && !readers.is_empty();
            let detail = format!(
                "{} #({}) — {} writer{} / {} reader{}",
                db_str(kind),
                type_params.join(", "),
                writers.len(),
                if writers.len() == 1 { "" } else { "s" },
                readers.len(),
                if readers.len() == 1 { "" } else { "s" },
            );

            UvmDbGroup {
                db: db_wire_str(kind).to_string(),
                key,
                type_params,
                type_mismatch,
                unwritten_read,
                detail,
                writers,
                readers,
            }
        })
        .collect();

    sort_entries(&mut ungrouped);

    UvmDbResponse {
        groups,
        ungrouped,
        truncated,
    }
}

// --------------------------------------------------------------------------
// Internal helpers
// --------------------------------------------------------------------------

/// The database's display name (`config_db` / `resource_db`).
fn db_str(kind: DbKind) -> &'static str {
    match kind {
        DbKind::ConfigDb => "config_db",
        DbKind::ResourceDb => "resource_db",
    }
}

/// The database's wire discriminator (`config` / `resource`).
fn db_wire_str(kind: DbKind) -> &'static str {
    match kind {
        DbKind::ConfigDb => "config",
        DbKind::ResourceDb => "resource",
    }
}

/// Stable within-group order: file, then position.
fn sort_entries(entries: &mut [UvmDbEntry]) {
    entries.sort_by(|a, b| {
        (&a.uri, a.range.start.line, a.range.start.character).cmp(&(
            &b.uri,
            b.range.start.line,
            b.range.start.character,
        ))
    });
}

/// Convert one scanned [`DbCall`] into its display-ready wire entry.
fn entry_from_call(call: &DbCall, url: &Url) -> UvmDbEntry {
    // The leaf label shows the method plus the most discriminating string
    // the call carries: config_db's instance path or resource_db's scope.
    // The key itself is the group label, so repeating it here is noise.
    let qualifier = match call.kind {
        DbKind::ConfigDb => call.inst_name.as_deref(),
        DbKind::ResourceDb => call.scope.as_deref(),
    };
    let label = match qualifier {
        Some(q) => format!("{}  \"{}\"", call.method, q),
        None => call.method.clone(),
    };

    let basename = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("")
        .to_string();
    let file_line = format!("{}:{}", basename, call.name_range.start.line + 1);

    UvmDbEntry {
        db: db_wire_str(call.kind).to_string(),
        method: call.method.clone(),
        access: match call.access {
            DbAccess::Write => "write".to_string(),
            DbAccess::Read => "read".to_string(),
        },
        type_param: call.type_param.clone(),
        key: call.key.clone(),
        inst_name: call.inst_name.clone(),
        scope: call.scope.clone(),
        cntxt: call.cntxt.clone(),
        uri: url.to_string(),
        range: m_range_to_lsp(call.name_range),
        call_range: m_range_to_lsp(call.call_range),
        label,
        file_line,
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_syntax::SyntaxParser;

    /// Parse one fixture into a `(Url, SyntaxTree)` pair.
    fn tree_for(url: &str, body: &str) -> (Url, SyntaxTree) {
        let src = format!(
            "class c;\n  function void f();\n{body}\n  endfunction\nendclass\n"
        );
        let mut parser = SyntaxParser::new().unwrap();
        let tree = parser.parse(&src, None).expect("parse");
        (Url::parse(url).unwrap(), tree)
    }

    #[test]
    fn set_and_get_across_files_pair_into_one_group() {
        let a = tree_for(
            "file:///a.sv",
            r#"    uvm_config_db#(int)::set(this, "env.agent", "cfg", 42);"#,
        );
        let b = tree_for(
            "file:///b.sv",
            r#"    void'(uvm_config_db#(int)::get(this, "", "cfg", x));"#,
        );
        let resp = collect_uvm_db(&[a, b]);

        assert_eq!(resp.groups.len(), 1, "{:?}", resp.groups);
        let g = &resp.groups[0];
        assert_eq!(g.db, "config");
        assert_eq!(g.key, "cfg");
        assert_eq!(g.writers.len(), 1);
        assert_eq!(g.readers.len(), 1);
        assert!(!g.unwritten_read);
        assert!(!g.type_mismatch);
        assert!(resp.ungrouped.is_empty());
        assert!(!resp.truncated);
    }

    #[test]
    fn get_without_set_flags_unwritten_read() {
        let a = tree_for(
            "file:///a.sv",
            r#"    void'(uvm_config_db#(string)::get(this, "", "mode", m));"#,
        );
        let resp = collect_uvm_db(&[a]);
        assert_eq!(resp.groups.len(), 1);
        assert!(resp.groups[0].unwritten_read);
    }

    #[test]
    fn differing_type_params_flag_type_mismatch() {
        let a = tree_for(
            "file:///a.sv",
            r#"    uvm_config_db#(int)::set(this, "", "cfg", 1);
    void'(uvm_config_db#(bit)::get(this, "", "cfg", b));"#,
        );
        let resp = collect_uvm_db(&[a]);
        assert_eq!(resp.groups.len(), 1);
        let g = &resp.groups[0];
        assert!(g.type_mismatch);
        assert_eq!(g.type_params, vec!["bit".to_string(), "int".to_string()]);
    }

    #[test]
    fn non_literal_key_and_read_by_type_land_in_ungrouped() {
        let a = tree_for(
            "file:///a.sv",
            r#"    uvm_config_db#(int)::set(this, "", name_var, 1);
    void'(uvm_resource_db#(int)::read_by_type("scope", v, this));"#,
        );
        let resp = collect_uvm_db(&[a]);
        assert!(resp.groups.is_empty(), "{:?}", resp.groups);
        assert_eq!(resp.ungrouped.len(), 2);
    }

    #[test]
    fn config_and_resource_with_same_key_stay_separate() {
        let a = tree_for(
            "file:///a.sv",
            r#"    uvm_config_db#(int)::set(this, "", "cfg", 1);
    uvm_resource_db#(int)::set("s", "cfg", 1, this);"#,
        );
        let resp = collect_uvm_db(&[a]);
        assert_eq!(resp.groups.len(), 2, "{:?}", resp.groups);
        // DbKind::ConfigDb < DbKind::ResourceDb — config group comes first.
        assert_eq!(resp.groups[0].db, "config");
        assert_eq!(resp.groups[1].db, "resource");
    }

    #[test]
    fn ordering_is_deterministic_regardless_of_input_order() {
        let mk = || {
            vec![
                tree_for(
                    "file:///b.sv",
                    r#"    uvm_config_db#(int)::set(this, "", "beta", 1);"#,
                ),
                tree_for(
                    "file:///a.sv",
                    r#"    uvm_config_db#(int)::set(this, "", "alpha", 1);"#,
                ),
            ]
        };
        let forward = collect_uvm_db(&mk());
        let mut reversed_input = mk();
        reversed_input.reverse();
        let reversed = collect_uvm_db(&reversed_input);

        let keys = |r: &UvmDbResponse| r.groups.iter().map(|g| g.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&forward), keys(&reversed));
        assert_eq!(keys(&forward), vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn labels_and_detail_are_display_ready() {
        let a = tree_for(
            "file:///dir/apb_env.sv",
            r#"    uvm_config_db#(int)::set(this, "env.agent", "cfg", 42);
    void'(uvm_config_db#(int)::get(this, "", "cfg", x));"#,
        );
        let resp = collect_uvm_db(&[a]);
        let g = &resp.groups[0];
        assert_eq!(g.detail, "config_db #(int) — 1 writer / 1 reader");

        let w = &g.writers[0];
        assert_eq!(w.label, "set  \"env.agent\"");
        // Fixture body starts on line 3 (1-based) of the class shell.
        assert_eq!(w.file_line, "apb_env.sv:3");
        assert_eq!(w.access, "write");
        assert_eq!(w.uri, "file:///dir/apb_env.sv");

        let r = &g.readers[0];
        assert_eq!(r.label, "get  \"\"");
        assert_eq!(r.access, "read");
    }
}
