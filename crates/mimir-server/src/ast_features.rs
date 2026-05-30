//! LSP feature implementations that operate on [`MimirAst`].
//!
//! Every public function in this module takes an [`Arc<MimirAst>`] plus
//! whatever context the feature needs (cursor position, document rope, …)
//! and returns an `Option` LSP response type. Returning `None` signals
//! "no result from this path; fall through to the next".
//!
//! The tree-sitter fallback (in [`crate::backend`]) runs whenever
//! `cached_ast` is `None` or when a function here returns `None`.

use std::sync::Arc;

use mimir_ast::{DeclKind, MimirAst, MimirDecl, MimirFile, MimirPos, MimirRange, MimirRef, MimirScope};
use ropey::Rope;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, GotoDefinitionResponse, Hover,
    HoverContents, Location, MarkupContent, MarkupKind, ParameterInformation,
    ParameterLabel, Position, Range, SignatureHelp, SignatureInformation, Url,
};
use tracing::debug;

// --------------------------------------------------------------------------
// Conversion helpers
// --------------------------------------------------------------------------

/// Convert an LSP [`Position`] to a [`MimirPos`] (both use line + UTF-16 char).
pub(crate) fn lsp_to_mimir_pos(pos: Position) -> MimirPos {
    MimirPos { line: pos.line, character: pos.character }
}

/// Convert a [`MimirPos`] back to an LSP [`Position`].
pub(crate) fn mimir_to_lsp_pos(pos: MimirPos) -> Position {
    Position { line: pos.line, character: pos.character }
}

/// Convert a [`MimirRange`] to an LSP [`Range`].
pub(crate) fn mimir_to_lsp_range(r: MimirRange) -> Range {
    Range { start: mimir_to_lsp_pos(r.start), end: mimir_to_lsp_pos(r.end) }
}

/// Map a [`DeclKind`] to the closest [`CompletionItemKind`].
fn decl_kind_to_completion_kind(kind: DeclKind) -> CompletionItemKind {
    match kind {
        DeclKind::Module | DeclKind::Interface | DeclKind::Program => CompletionItemKind::MODULE,
        DeclKind::Package => CompletionItemKind::MODULE,
        DeclKind::Class => CompletionItemKind::CLASS,
        DeclKind::Function => CompletionItemKind::FUNCTION,
        DeclKind::Task => CompletionItemKind::FUNCTION,
        DeclKind::Property | DeclKind::Sequence => CompletionItemKind::PROPERTY,
        DeclKind::Covergroup => CompletionItemKind::CLASS,
        DeclKind::Port => CompletionItemKind::VARIABLE,
        DeclKind::Parameter | DeclKind::LocalParam => CompletionItemKind::CONSTANT,
        DeclKind::Variable | DeclKind::Field => CompletionItemKind::VARIABLE,
        DeclKind::Typedef | DeclKind::Enum => CompletionItemKind::INTERFACE,
        DeclKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        DeclKind::Macro => CompletionItemKind::SNIPPET,
    }
}

// --------------------------------------------------------------------------
// Word extraction (rope-only, no tree-sitter)
// --------------------------------------------------------------------------

/// Extract the identifier token at `pos` from `rope`, without needing a
/// tree-sitter parse. Returns `None` when the cursor is on whitespace,
/// punctuation, or past the end of the line.
///
/// Handles UTF-16 column encoding conservatively: for ASCII source (the
/// typical case in SystemVerilog) UTF-16 columns equal char columns.
pub(crate) fn word_at_rope(rope: &Rope, pos: MimirPos) -> Option<String> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }
    let line = rope.line(pos.line as usize);

    // Collect chars up to and past the cursor, tracking UTF-16 column.
    let mut chars: Vec<(u32, char)> = Vec::new(); // (utf16_col, char)
    let mut utf16: u32 = 0;
    for ch in line.chars() {
        if ch == '\n' || ch == '\r' {
            break;
        }
        chars.push((utf16, ch));
        utf16 += ch.len_utf16() as u32;
    }

    // Find the char index that corresponds to the cursor column.
    let cursor_idx = chars.iter().position(|(col, _)| *col >= pos.character)?;
    let (_, cursor_ch) = chars[cursor_idx];
    if !is_id_char(cursor_ch) {
        return None;
    }

    // Scan backward to the start of the identifier.
    let start = chars[..cursor_idx]
        .iter()
        .rposition(|(_, c)| !is_id_char(*c))
        .map(|i| i + 1)
        .unwrap_or(0);

    // Scan forward to the end of the identifier.
    let end = chars[cursor_idx..]
        .iter()
        .position(|(_, c)| !is_id_char(*c))
        .map(|i| cursor_idx + i)
        .unwrap_or(chars.len());

    let word: String = chars[start..end].iter().map(|(_, c)| *c).collect();
    if word.is_empty() { None } else { Some(word) }
}

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

// --------------------------------------------------------------------------
// Scope-walk helpers
// --------------------------------------------------------------------------

/// Collect all declarations visible from `pos` in `file`.
///
/// Walks the scope chain from the root down to the innermost scope
/// containing `pos`, gathering every declaration along the way. Outermost
/// scope declarations are first; innermost last (innermost takes priority).
fn visible_in_file(file: &MimirFile, pos: MimirPos) -> Vec<&MimirDecl> {
    let mut out = Vec::new();
    collect_scope_chain(&file.top_scope, pos, &mut out);
    out
}

fn collect_scope_chain<'a>(scope: &'a MimirScope, pos: MimirPos, out: &mut Vec<&'a MimirDecl>) {
    for decl in &scope.declarations {
        out.push(decl);
        // When the cursor is inside a declaration's body (e.g. a class body),
        // its members (methods, fields) are visible at the call site.
        if decl.full_range.contains(pos) {
            out.extend(decl.members.iter());
        }
    }
    for child in &scope.children {
        if child.range.contains(pos) {
            collect_scope_chain(child, pos, out);
            return;
        }
    }
}

/// All declarations visible at `(file_uri, pos)`, including top-level
/// declarations from every other file in the AST.
pub(crate) fn visible_decls<'a>(ast: &'a MimirAst, file_uri: &str, pos: MimirPos) -> Vec<&'a MimirDecl> {
    let mut out = Vec::new();

    if let Some(file) = ast.find_file(file_uri) {
        collect_scope_chain(&file.top_scope, pos, &mut out);
    }

    for file in &ast.files {
        if file.uri == file_uri {
            continue;
        }
        out.extend(file.top_scope.declarations.iter());
    }

    out
}

/// Search every scope in the AST (recursively) for the first declaration
/// whose name matches `name`. Returns `(decl, file_uri)` when found.
pub(crate) fn find_decl_global<'a>(ast: &'a MimirAst, name: &str) -> Option<(&'a MimirDecl, &'a str)> {
    for file in &ast.files {
        if let Some(d) = find_in_scope(&file.top_scope, name) {
            return Some((d, &file.uri));
        }
    }
    None
}

fn find_in_scope<'a>(scope: &'a MimirScope, name: &str) -> Option<&'a MimirDecl> {
    for decl in &scope.declarations {
        if decl.name == name {
            return Some(decl);
        }
    }
    for child in &scope.children {
        if let Some(d) = find_in_scope(child, name) {
            return Some(d);
        }
    }
    None
}

/// Find the first declaration named `name` in the visible scope at `pos`
/// inside `file_uri`, then fall back to a global AST search.
pub(crate) fn find_decl<'a>(
    ast: &'a MimirAst,
    file_uri: &'a str,
    pos: MimirPos,
    name: &str,
) -> Option<(&'a MimirDecl, &'a str)> {
    if let Some(file) = ast.find_file(file_uri) {
        let local = visible_in_file(file, pos)
            .into_iter()
            .find(|d| d.name == name);
        if let Some(d) = local {
            return Some((d, file_uri));
        }
    }
    find_decl_global(ast, name)
}

// --------------------------------------------------------------------------
// Reference-map lookup
// --------------------------------------------------------------------------

/// Half-open `a <= b` over [`MimirPos`] (line major, then UTF-16 char).
///
/// `MimirPos` deliberately doesn't implement `Ord` — comparing positions
/// only makes sense relative to a known file's line structure — so the
/// helper lives here, scoped to the reference-map binary search.
fn pos_le(a: MimirPos, b: MimirPos) -> bool {
    a.line < b.line || (a.line == b.line && a.character <= b.character)
}

/// Find the [`MimirRef`] whose `use_range` contains `pos`, if any.
///
/// Assumes `file.references` is sorted by `use_range.start` (sidecar
/// invariant). Use-site ranges for distinct identifier tokens do not
/// overlap, so at most one entry can match — we binary-search to the
/// rightmost candidate and check `contains` on it.
fn find_ref_at(file: &MimirFile, pos: MimirPos) -> Option<&MimirRef> {
    let idx = file
        .references
        .partition_point(|r| pos_le(r.use_range.start, pos));
    if idx == 0 {
        return None;
    }
    let candidate = &file.references[idx - 1];
    candidate.use_range.contains(pos).then_some(candidate)
}

/// Find the declaration whose name-token `range` equals `target`, searching
/// `file`'s scope tree and descending into each decl's `members` (methods
/// and fields live as members of their enclosing class decl, so a flat
/// scope walk would miss them).
fn find_decl_at(file: &MimirFile, target: MimirRange) -> Option<&MimirDecl> {
    fn search_decls(decls: &[MimirDecl], target: MimirRange) -> Option<&MimirDecl> {
        for d in decls {
            if d.range == target {
                return Some(d);
            }
            if let Some(found) = search_decls(&d.members, target) {
                return Some(found);
            }
        }
        None
    }
    fn search_scope(scope: &MimirScope, target: MimirRange) -> Option<&MimirDecl> {
        if let Some(found) = search_decls(&scope.declarations, target) {
            return Some(found);
        }
        scope.children.iter().find_map(|c| search_scope(c, target))
    }
    search_scope(&file.top_scope, target)
}

/// Resolve the method call whose name token is at `pos` to its formal
/// parameters, via the slang reference map plus the target decl's `Port`
/// members.
///
/// Returns `(name, type_str)` pairs in declaration order, or `None` when
/// `pos` isn't on a resolved function/task call or the target decl can't
/// be located in the AST. Powers the slang-first path of `inlay_hint`,
/// which needs the callee's parameters to label call arguments — and is
/// receiver-aware, so it resolves methods inherited from base classes the
/// tree-sitter workspace index never indexed (e.g. UVM bases).
pub(crate) fn method_params_at(
    ast: &MimirAst,
    file_uri: &str,
    pos: MimirPos,
) -> Option<Vec<(String, Option<String>)>> {
    let file = ast.find_file(file_uri)?;
    let r = find_ref_at(file, pos)?;
    if !matches!(r.target_kind, DeclKind::Function | DeclKind::Task) {
        return None;
    }
    let target_file = ast.find_file(&r.target_path)?;
    let decl = find_decl_at(target_file, r.target_range)?;
    let params = decl
        .members
        .iter()
        .filter(|m| m.kind == DeclKind::Port)
        .map(|m| (m.name.clone(), m.type_str.clone()))
        .collect();
    Some(params)
}

// --------------------------------------------------------------------------
// goto-definition
// --------------------------------------------------------------------------

/// Resolve the declaration of the identifier at `pos` in `file_uri`.
///
/// Returns `None` when the cursor isn't on an identifier we can find in
/// the MimirAst; the caller should fall through to the slang IPC or
/// tree-sitter paths.
///
/// Resolution order:
///   1. **Reference map** — the sidecar's resolved-reference table for
///      `file_uri`. This is receiver-aware (slang's name binder did the
///      work) so it handles inherited fields, typedef chains, and
///      package-imported symbols.
///   2. **Name lookup fallback** — `word_at_rope` + `find_decl`, kept
///      for sidecars that pre-date the reference map (the field
///      decodes as empty) and for use sites the visitor doesn't yet
///      cover (e.g. `` `define `` macros).
pub(crate) fn definition(
    ast: &Arc<MimirAst>,
    file_uri: &str,
    pos: MimirPos,
    rope: &Rope,
) -> Option<GotoDefinitionResponse> {
    match ast.find_file(file_uri) {
        Some(file) => {
            let refs_in_file = file.references.len();
            match find_ref_at(file, pos) {
                Some(r) => {
                    let uri = Url::parse(&format!("file://{}", r.target_path)).ok()?;
                    let location =
                        Location { uri, range: mimir_to_lsp_range(r.target_range) };
                    debug!(
                        file_uri,
                        refs_in_file,
                        target_path = %r.target_path,
                        target_kind = ?r.target_kind,
                        "ast definition: gate=ref_map_hit",
                    );
                    return Some(GotoDefinitionResponse::Scalar(location));
                }
                None => {
                    // File is in the AST but no ref covers the cursor. Most
                    // common causes: (a) refs_in_file == 0 → slang didn't
                    // elaborate this file *under this path* (often because
                    // it's open in the editor + reached via `` `include `` and
                    // slang resolved the include to a different path string,
                    // creating a second buffer whose refs key elsewhere), or
                    // (b) refs_in_file > 0 but the visitor didn't emit one
                    // for this exact token shape.
                    //
                    // When refs_in_file == 0, also dump the top-5 paths that
                    // *did* receive refs so a path-mismatch is easy to spot.
                    if refs_in_file == 0 {
                        let mut by_count: Vec<(&str, usize)> = ast
                            .files
                            .iter()
                            .filter(|f| !f.references.is_empty())
                            .map(|f| (f.uri.as_str(), f.references.len()))
                            .collect();
                        by_count.sort_by_key(|b| std::cmp::Reverse(b.1));
                        by_count.truncate(5);

                        // Smoking gun for include-path-key mismatch: a file
                        // whose basename matches the cursor's file and has
                        // refs > 0 means slang elaborated the same content
                        // under a different path string than what the editor
                        // sent — usually because the parent CU's
                        // `` `include "x" `` resolved via +incdir+ to a path
                        // string that differs from the editor's open-file
                        // URI. The per-file ref emit keys by the resolved
                        // path; our find_file lookup uses the editor URI →
                        // miss → "0 refs".
                        let target_basename =
                            std::path::Path::new(file_uri)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("");
                        let basename_matches: Vec<(&str, usize)> = ast
                            .files
                            .iter()
                            .filter(|f| {
                                !f.references.is_empty()
                                    && std::path::Path::new(&f.uri)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .map(|n| n == target_basename)
                                        .unwrap_or(false)
                            })
                            .map(|f| (f.uri.as_str(), f.references.len()))
                            .collect();

                        debug!(
                            file_uri,
                            pos_line = pos.line,
                            pos_char = pos.character,
                            refs_in_file,
                            top_paths_with_refs = ?by_count,
                            basename_matches = ?basename_matches,
                            "ast definition: gate=ref_not_at_pos (falling through to name lookup)",
                        );
                    } else {
                        debug!(
                            file_uri,
                            pos_line = pos.line,
                            pos_char = pos.character,
                            refs_in_file,
                            "ast definition: gate=ref_not_at_pos (falling through to name lookup)",
                        );
                    }
                }
            }
        }
        None => {
            // The cursor's file URI isn't in the AST at all. Usually a path
            // mismatch (symlink resolution, `/private/var` vs `/var`,
            // trailing slash) — slang knows the file under a different
            // string than the LSP URI maps to. Log the first few URIs the
            // AST does know so the user can diff.
            let known: Vec<&str> = ast
                .files
                .iter()
                .take(5)
                .map(|f| f.uri.as_str())
                .collect();
            debug!(
                file_uri,
                total_files_in_ast = ast.files.len(),
                first_known_uris = ?known,
                "ast definition: gate=file_not_in_ast (falling through to name lookup)",
            );
        }
    }

    let name = word_at_rope(rope, pos)?;
    let (decl, decl_file) = find_decl(ast, file_uri, pos, &name)?;
    let uri = Url::parse(&format!("file://{decl_file}")).ok()?;
    let location = Location { uri, range: mimir_to_lsp_range(decl.range) };
    debug!(name, file = decl_file, "ast definition: gate=name_lookup_hit");
    Some(GotoDefinitionResponse::Scalar(location))
}

// --------------------------------------------------------------------------
// type-definition
// --------------------------------------------------------------------------

/// Resolve the declaration of the *type* of the symbol at `pos`.
///
/// Finds the declaration at the cursor, reads its `type_str`, and
/// searches for a class/typedef/enum declaration with that name.
/// Returns `None` when the type cannot be resolved from the AST.
pub(crate) fn type_definition(
    ast: &Arc<MimirAst>,
    file_uri: &str,
    pos: MimirPos,
    rope: &Rope,
) -> Option<GotoDefinitionResponse> {
    let name = word_at_rope(rope, pos)?;
    let (decl, _) = find_decl(ast, file_uri, pos, &name)?;

    let type_name = decl.type_str.as_deref()?;
    let bare_type = type_name.split_whitespace().last().unwrap_or(type_name);

    let (type_decl, type_file) = find_decl_global(ast, bare_type)?;

    let interesting = matches!(
        type_decl.kind,
        DeclKind::Class | DeclKind::Typedef | DeclKind::Enum | DeclKind::Interface
    );
    if !interesting {
        return None;
    }

    let uri = Url::parse(&format!("file://{type_file}")).ok()?;
    let location = Location { uri, range: mimir_to_lsp_range(type_decl.range) };
    debug!(name, type_name = bare_type, "ast type_definition resolved");
    Some(GotoDefinitionResponse::Scalar(location))
}

// --------------------------------------------------------------------------
// hover
// --------------------------------------------------------------------------

/// Build a [`Hover`] response for the symbol at `pos`.
pub(crate) fn hover(
    ast: &Arc<MimirAst>,
    file_uri: &str,
    pos: MimirPos,
    rope: &Rope,
) -> Option<Hover> {
    let name = word_at_rope(rope, pos)?;
    let (decl, _) = find_decl(ast, file_uri, pos, &name)?;

    let mut parts: Vec<String> = Vec::new();

    let kind_label = match decl.kind {
        DeclKind::Module => "module",
        DeclKind::Interface => "interface",
        DeclKind::Program => "program",
        DeclKind::Package => "package",
        DeclKind::Class => "class",
        DeclKind::Function => "function",
        DeclKind::Task => "task",
        DeclKind::Property => "property",
        DeclKind::Sequence => "sequence",
        DeclKind::Covergroup => "covergroup",
        DeclKind::Port => "port",
        DeclKind::Parameter => "parameter",
        DeclKind::LocalParam => "localparam",
        DeclKind::Variable => "variable",
        DeclKind::Field => "field",
        DeclKind::Typedef => "typedef",
        DeclKind::Enum => "enum",
        DeclKind::EnumMember => "enum member",
        DeclKind::Macro => "macro",
    };

    let type_part = match &decl.type_str {
        Some(t) => format!("{t} {name}"),
        None => format!("{kind_label} {name}"),
    };

    if decl.kind == DeclKind::Function || decl.kind == DeclKind::Task {
        let params: Vec<String> = decl
            .members
            .iter()
            .filter(|m| m.kind == DeclKind::Port)
            .map(|m| {
                match &m.type_str {
                    Some(t) => format!("{t} {}", m.name),
                    None => m.name.clone(),
                }
            })
            .collect();
        let sig = format!("{kind_label} {name}({})", params.join(", "));
        parts.push(mimir_syntax::hover_format::format_sv_signature(&sig));
    } else {
        parts.push(format!("```systemverilog\n{type_part}\n```"));
    }

    if let Some(doc) = &decl.doc {
        if !doc.is_empty() {
            parts.push(doc.clone());
        }
    }

    let value = parts.join("\n\n");
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
        range: Some(mimir_to_lsp_range(decl.range)),
    })
}

// --------------------------------------------------------------------------
// identifier completion
// --------------------------------------------------------------------------

/// Completion candidates for a plain identifier at `pos`.
///
/// Returns all declarations visible in scope, scored by the partial prefix
/// already typed. The caller is responsible for adding keywords on top.
pub(crate) fn identifier_completion(
    ast: &Arc<MimirAst>,
    file_uri: &str,
    pos: MimirPos,
) -> Vec<CompletionItem> {
    let all = visible_decls(ast, file_uri, pos);
    let mut items: Vec<CompletionItem> = all
        .iter()
        .map(|d| {
            let detail = d.type_str.as_deref().map(str::to_owned).or_else(|| {
                d.parent_class.as_ref().map(|pc| format!("extends {pc}"))
            });
            CompletionItem {
                label: d.name.clone(),
                kind: Some(decl_kind_to_completion_kind(d.kind)),
                detail,
                documentation: d.doc.as_deref().map(|s| {
                    tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::PlainText,
                        value: s.to_owned(),
                    })
                }),
                ..Default::default()
            }
        })
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

// --------------------------------------------------------------------------
// member completion
// --------------------------------------------------------------------------

/// Completion candidates after `receiver.` or `pkg::`.
///
/// `receiver_name` is the identifier to the left of `.` or `::`.
/// When `is_pkg_scope` is `true` the receiver names a package; otherwise it
/// names an object or type and its class members are returned.
///
/// Returns `None` when the receiver type cannot be resolved; returns
/// `Some(empty)` when the type was found but has no visible members.
pub(crate) fn member_completion(
    ast: &Arc<MimirAst>,
    file_uri: &str,
    pos: MimirPos,
    receiver_name: &str,
    is_pkg_scope: bool,
) -> Option<CompletionResponse> {
    if is_pkg_scope {
        let (pkg_decl, _) = find_decl(ast, file_uri, pos, receiver_name)?;
        if pkg_decl.kind != DeclKind::Package {
            return None;
        }
        let items = decls_to_completion_items(&pkg_decl.members);
        return Some(CompletionResponse::Array(items));
    }

    // `.` accessor: find the type of the receiver, then its class members.
    let (recv_decl, _) = find_decl(ast, file_uri, pos, receiver_name)?;
    let type_name = recv_decl.type_str.as_deref()?;
    let bare_type = type_name.split_whitespace().last().unwrap_or(type_name);

    let (class_decl, _) = find_decl_global(ast, bare_type)?;
    if !matches!(class_decl.kind, DeclKind::Class | DeclKind::Interface) {
        return None;
    }

    let mut items = decls_to_completion_items(&class_decl.members);

    // Include inherited members from the parent class.
    if let Some(ref parent_name) = class_decl.parent_class {
        if let Some((parent_decl, _)) = find_decl_global(ast, parent_name) {
            let inherited = decls_to_completion_items(&parent_decl.members);
            for item in inherited {
                if !items.iter().any(|i| i.label == item.label) {
                    items.push(item);
                }
            }
        }
    }

    debug!(receiver = receiver_name, class = bare_type, members = items.len(), "ast member completion");
    Some(CompletionResponse::Array(items))
}

fn decls_to_completion_items(decls: &[MimirDecl]) -> Vec<CompletionItem> {
    decls
        .iter()
        .map(|d| CompletionItem {
            label: d.name.clone(),
            kind: Some(decl_kind_to_completion_kind(d.kind)),
            detail: d.type_str.clone(),
            documentation: d.doc.as_deref().map(|s| {
                tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: s.to_owned(),
                })
            }),
            ..Default::default()
        })
        .collect()
}

// --------------------------------------------------------------------------
// signature help
// --------------------------------------------------------------------------

/// Signature help for the callable at `pos`.
///
/// Scans backward from the cursor to find the function/task name and the
/// current active parameter index (number of commas before the cursor
/// inside the call's argument list). Returns `None` when the cursor is
/// not inside a call or the callable cannot be resolved in the AST.
pub(crate) fn signature_help(
    ast: &Arc<MimirAst>,
    file_uri: &str,
    pos: MimirPos,
    rope: &Rope,
) -> Option<SignatureHelp> {
    let (callee_name, active_param) = callee_at(rope, pos)?;
    let (decl, _) = find_decl(ast, file_uri, pos, callee_name.as_str())?;

    if !matches!(decl.kind, DeclKind::Function | DeclKind::Task) {
        return None;
    }

    let ports: Vec<&MimirDecl> = decl
        .members
        .iter()
        .filter(|m| m.kind == DeclKind::Port)
        .collect();

    let params: Vec<ParameterInformation> = ports
        .iter()
        .map(|p| {
            let label = match &p.type_str {
                Some(t) => format!("{t} {}", p.name),
                None => p.name.clone(),
            };
            ParameterInformation {
                label: ParameterLabel::Simple(label),
                documentation: None,
            }
        })
        .collect();

    let kind_label = if decl.kind == DeclKind::Task { "task" } else { "function" };
    let return_type = decl.type_str.as_deref().map(|t| format!("{t} ")).unwrap_or_default();
    let param_list: Vec<String> = params.iter().map(|p| match &p.label {
        ParameterLabel::Simple(s) => s.clone(),
        _ => String::new(),
    }).collect();
    let label = format!(
        "{kind_label} {return_type}{}({})",
        decl.name,
        param_list.join(", ")
    );

    let active = active_param.min(ports.len().saturating_sub(1)) as u32;

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: decl.doc.as_deref().map(|s| {
                tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: s.to_owned(),
                })
            }),
            parameters: Some(params),
            active_parameter: Some(active),
        }],
        active_signature: Some(0),
        active_parameter: Some(active),
    })
}

/// Scan backward from `pos` to find the name of the function being called
/// and how many commas (= active parameter index) precede the cursor inside
/// the argument list.
///
/// Returns `None` if the cursor is not inside a `(…)` call context.
fn callee_at(rope: &Rope, pos: MimirPos) -> Option<(String, usize)> {
    if (pos.line as usize) >= rope.len_lines() {
        return None;
    }

    let line = rope.line(pos.line as usize);
    // Build a char vec up to the cursor column (UTF-16 aware).
    let mut chars_before: Vec<char> = Vec::new();
    let mut utf16: u32 = 0;
    for ch in line.chars() {
        if ch == '\n' || ch == '\r' || utf16 >= pos.character {
            break;
        }
        chars_before.push(ch);
        utf16 += ch.len_utf16() as u32;
    }

    // Walk backward counting commas and tracking paren depth.
    let mut depth = 0i32;
    let mut commas = 0usize;
    let mut paren_open_idx: Option<usize> = None;

    for (i, &ch) in chars_before.iter().enumerate().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                if depth == 0 {
                    paren_open_idx = Some(i);
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
    }

    let paren_idx = paren_open_idx?;
    // Extract the identifier immediately before the `(`.
    let before_paren: String = chars_before[..paren_idx].iter().collect();
    let name: String = before_paren
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .map(|(_, c)| c)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    if name.is_empty() {
        return None;
    }
    Some((name, commas))
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_ast::{MimirFile, MimirScope, Visibility};
    use std::sync::Arc;

    fn make_pos(line: u32, ch: u32) -> MimirPos {
        MimirPos { line, character: ch }
    }

    fn make_range(sl: u32, sc: u32, el: u32, ec: u32) -> MimirRange {
        MimirRange {
            start: make_pos(sl, sc),
            end: make_pos(el, ec),
        }
    }

    fn make_decl(name: &str, kind: DeclKind, line: u32) -> MimirDecl {
        MimirDecl {
            name: name.to_owned(),
            kind,
            range: make_range(line, 0, line, name.len() as u32),
            full_range: make_range(line, 0, line + 1, 0),
            type_str: None,
            members: vec![],
            parent_class: None,
            visibility: Visibility::Public,
            doc: None,
        }
    }

    fn simple_ast() -> MimirAst {
        MimirAst {
            files: vec![MimirFile {
                uri: "/tmp/a.sv".to_string(),
                diagnostics: vec![],
                top_scope: MimirScope {
                    range: make_range(0, 0, 100, 0),
                    declarations: vec![
                        make_decl("my_module", DeclKind::Module, 0),
                        make_decl("my_class", DeclKind::Class, 10),
                    ],
                    children: vec![],
                    imported_packages: vec![],
                },
                references: vec![],
            }],
        }
    }

    #[test]
    fn visible_decls_returns_top_level() {
        let ast = simple_ast();
        let decls = visible_decls(&ast, "/tmp/a.sv", make_pos(5, 0));
        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"my_module"));
        assert!(names.contains(&"my_class"));
    }

    #[test]
    fn visible_decls_includes_cross_file() {
        let mut ast = simple_ast();
        ast.files.push(MimirFile {
            uri: "/tmp/b.sv".to_string(),
            diagnostics: vec![],
            top_scope: MimirScope {
                range: make_range(0, 0, 50, 0),
                declarations: vec![make_decl("other_pkg", DeclKind::Package, 0)],
                children: vec![],
                imported_packages: vec![],
            },
            references: vec![],
        });
        let decls = visible_decls(&ast, "/tmp/a.sv", make_pos(5, 0));
        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"other_pkg"));
    }

    #[test]
    fn find_decl_local_before_global() {
        let mut ast = simple_ast();
        ast.files[0].top_scope.children.push(MimirScope {
            range: make_range(5, 0, 8, 0),
            declarations: vec![make_decl("inner_var", DeclKind::Variable, 6)],
            children: vec![],
            imported_packages: vec![],
        });
        let (d, file) = find_decl(&ast, "/tmp/a.sv", make_pos(6, 0), "inner_var").unwrap();
        assert_eq!(d.name, "inner_var");
        assert_eq!(file, "/tmp/a.sv");
    }

    #[test]
    fn find_decl_global_fallback() {
        let ast = simple_ast();
        let result = find_decl(&ast, "/tmp/a.sv", make_pos(0, 0), "my_class");
        assert!(result.is_some());
    }

    #[test]
    fn find_decl_unknown_returns_none() {
        let ast = simple_ast();
        assert!(find_decl(&ast, "/tmp/a.sv", make_pos(0, 0), "nonexistent").is_none());
    }

    #[test]
    fn definition_via_ref_map_returns_target() {
        // Build an AST where the cursor lands on a use_range and the ref
        // points at a target in a *different* file with a *different* name
        // than the use. The reference-map path must win over the name path.
        let mut ast = simple_ast();
        ast.files[0].references.push(MimirRef {
            use_range: make_range(7, 4, 7, 13),
            target_path: "/tmp/other.sv".to_string(),
            target_range: make_range(42, 6, 42, 15),
            target_kind: DeclKind::Function,
        });
        let ast = Arc::new(ast);

        let rope = Rope::from_str(""); // intentionally empty: ref-map path must not consult it
        let resp = definition(&ast, "/tmp/a.sv", make_pos(7, 6), &rope).unwrap();
        let GotoDefinitionResponse::Scalar(loc) = resp else {
            panic!("expected Scalar response, got {resp:?}");
        };
        assert!(loc.uri.as_str().ends_with("/tmp/other.sv"), "uri = {}", loc.uri);
        assert_eq!(loc.range.start.line, 42);
        assert_eq!(loc.range.start.character, 6);
    }

    #[test]
    fn definition_falls_through_when_no_ref_at_pos() {
        // Same fixture as above, but the cursor is *outside* the one
        // ref's use_range. The reference-map lookup misses and the
        // existing name-based path must still answer.
        let mut ast = simple_ast();
        ast.files[0].references.push(MimirRef {
            use_range: make_range(7, 4, 7, 13),
            target_path: "/tmp/other.sv".to_string(),
            target_range: make_range(42, 6, 42, 15),
            target_kind: DeclKind::Function,
        });
        let ast = Arc::new(ast);

        // Cursor on line 0 — well clear of the use_range above. The rope
        // has the `my_module` identifier so word_at_rope returns a name
        // that find_decl can resolve to the simple_ast top-level decl.
        let rope = Rope::from_str("my_module\n");
        let resp = definition(&ast, "/tmp/a.sv", make_pos(0, 0), &rope).unwrap();
        let GotoDefinitionResponse::Scalar(loc) = resp else {
            panic!("expected Scalar response, got {resp:?}");
        };
        // The name-fallback resolves to the same file (where my_module is declared).
        assert!(loc.uri.as_str().ends_with("/tmp/a.sv"), "uri = {}", loc.uri);
    }

    #[test]
    fn find_ref_at_binary_search_picks_correct_entry() {
        // Several refs in order; cursor falls inside the third one.
        let file = MimirFile {
            uri: "/tmp/a.sv".to_string(),
            diagnostics: vec![],
            top_scope: MimirScope {
                range: make_range(0, 0, 100, 0),
                declarations: vec![],
                children: vec![],
                imported_packages: vec![],
            },
            references: vec![
                MimirRef {
                    use_range: make_range(1, 0, 1, 5),
                    target_path: "/x.sv".into(),
                    target_range: make_range(0, 0, 0, 1),
                    target_kind: DeclKind::Variable,
                },
                MimirRef {
                    use_range: make_range(3, 0, 3, 5),
                    target_path: "/y.sv".into(),
                    target_range: make_range(0, 0, 0, 1),
                    target_kind: DeclKind::Variable,
                },
                MimirRef {
                    use_range: make_range(5, 4, 5, 12),
                    target_path: "/target.sv".into(),
                    target_range: make_range(20, 0, 20, 4),
                    target_kind: DeclKind::Function,
                },
                MimirRef {
                    use_range: make_range(7, 0, 7, 3),
                    target_path: "/z.sv".into(),
                    target_range: make_range(0, 0, 0, 1),
                    target_kind: DeclKind::Variable,
                },
            ],
        };
        let hit = find_ref_at(&file, make_pos(5, 7)).expect("ref at (5, 7)");
        assert_eq!(hit.target_path, "/target.sv");

        // Cursor between refs returns None.
        assert!(find_ref_at(&file, make_pos(4, 0)).is_none());
        // Cursor before any ref returns None.
        assert!(find_ref_at(&file, make_pos(0, 0)).is_none());
        // Cursor past last ref's end returns None.
        assert!(find_ref_at(&file, make_pos(9, 0)).is_none());
    }

    /// Build a two-file AST: `call.sv` has a method-call ref at the cursor
    /// pointing into `defs.sv`, where `base_cls` declares `configure(int a,
    /// string b)` as a nested member. Mirrors the cross-file inherited-method
    /// shape that breaks the tree-sitter path.
    fn ast_with_method_ref() -> MimirAst {
        // configure() with two Port members, nested inside base_cls.
        let mut configure = make_decl("configure", DeclKind::Function, 5);
        configure.members = vec![
            {
                let mut p = make_decl("a", DeclKind::Port, 5);
                p.type_str = Some("int".to_owned());
                p
            },
            {
                let mut p = make_decl("b", DeclKind::Port, 5);
                p.type_str = Some("string".to_owned());
                p
            },
        ];
        let mut base_cls = make_decl("base_cls", DeclKind::Class, 2);
        base_cls.members = vec![configure];

        MimirAst {
            files: vec![
                MimirFile {
                    uri: "/tmp/call.sv".to_string(),
                    diagnostics: vec![],
                    top_scope: MimirScope {
                        range: make_range(0, 0, 50, 0),
                        declarations: vec![],
                        children: vec![],
                        imported_packages: vec![],
                    },
                    references: vec![MimirRef {
                        use_range: make_range(30, 8, 30, 17),
                        target_path: "/tmp/defs.sv".to_string(),
                        // matches configure's name-token range (make_decl uses
                        // range = line,0 .. line,name_len).
                        target_range: make_range(5, 0, 5, "configure".len() as u32),
                        target_kind: DeclKind::Function,
                    }],
                },
                MimirFile {
                    uri: "/tmp/defs.sv".to_string(),
                    diagnostics: vec![],
                    top_scope: MimirScope {
                        range: make_range(0, 0, 50, 0),
                        declarations: vec![base_cls],
                        children: vec![],
                        imported_packages: vec![],
                    },
                    references: vec![],
                },
            ],
        }
    }

    #[test]
    fn method_params_at_resolves_cross_file_nested_method() {
        let ast = ast_with_method_ref();
        let params = method_params_at(&ast, "/tmp/call.sv", make_pos(30, 10))
            .expect("ref at the call site should resolve to configure's params");
        assert_eq!(
            params,
            vec![
                ("a".to_string(), Some("int".to_string())),
                ("b".to_string(), Some("string".to_string())),
            ]
        );
    }

    #[test]
    fn method_params_at_returns_none_for_non_callable_target() {
        let mut ast = ast_with_method_ref();
        // Flip the ref's target to a variable — params lookup must decline.
        ast.files[0].references[0].target_kind = DeclKind::Variable;
        assert!(method_params_at(&ast, "/tmp/call.sv", make_pos(30, 10)).is_none());
    }

    #[test]
    fn method_params_at_returns_none_when_no_ref_at_pos() {
        let ast = ast_with_method_ref();
        // Cursor nowhere near the single recorded use_range.
        assert!(method_params_at(&ast, "/tmp/call.sv", make_pos(0, 0)).is_none());
    }

    #[test]
    fn find_decl_at_locates_nested_member() {
        let ast = ast_with_method_ref();
        let defs = ast.find_file("/tmp/defs.sv").unwrap();
        let decl = find_decl_at(defs, make_range(5, 0, 5, "configure".len() as u32))
            .expect("configure is nested in base_cls.members");
        assert_eq!(decl.name, "configure");
        assert_eq!(decl.kind, DeclKind::Function);
    }

    #[test]
    fn identifier_completion_returns_visible_names() {
        let ast = Arc::new(simple_ast());
        let items = identifier_completion(&ast, "/tmp/a.sv", make_pos(5, 0));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"my_module"));
        assert!(labels.contains(&"my_class"));
    }

    #[test]
    fn member_completion_returns_class_members() {
        let mut ast = simple_ast();

        // Add a `data` field to the `my_class` declaration.
        let field = MimirDecl {
            name: "data".to_owned(),
            kind: DeclKind::Field,
            range: make_range(11, 4, 11, 8),
            full_range: make_range(11, 0, 12, 0),
            type_str: Some("logic [7:0]".to_owned()),
            members: vec![],
            parent_class: None,
            visibility: Visibility::Public,
            doc: None,
        };
        ast.files[0].top_scope.declarations[1].members.push(field);

        // Add a variable `obj` of type `my_class` so the receiver lookup works.
        let var = MimirDecl {
            name: "obj".to_owned(),
            kind: DeclKind::Variable,
            range: make_range(20, 0, 20, 3),
            full_range: make_range(20, 0, 21, 0),
            type_str: Some("my_class".to_owned()),
            members: vec![],
            parent_class: None,
            visibility: Visibility::Public,
            doc: None,
        };
        ast.files[0].top_scope.declarations.push(var);
        let ast = Arc::new(ast);

        // `obj.` completion should return `data` from `my_class`.
        let resp = member_completion(&ast, "/tmp/a.sv", make_pos(5, 0), "obj", false);
        assert!(resp.is_some());
        if let Some(CompletionResponse::Array(items)) = resp {
            assert!(items.iter().any(|i| i.label == "data"));
        }
    }

    #[test]
    fn callee_at_finds_name_and_param_index() {
        let src = "foo(a, b, ";
        let rope = Rope::from_str(src);
        let (name, idx) = callee_at(&rope, make_pos(0, 10)).unwrap();
        assert_eq!(name, "foo");
        assert_eq!(idx, 2);
    }

    #[test]
    fn callee_at_no_open_paren_returns_none() {
        let src = "just_ident";
        let rope = Rope::from_str(src);
        assert!(callee_at(&rope, make_pos(0, 10)).is_none());
    }
}
