//! LSP-backed agent tools dispatched from `execute_tool`.
//!
//! These turn the daemon's shared [`smedja_lsp::LspManager`] into first-class
//! tools the model can call: `lsp_definition`, `lsp_references`, `lsp_hover`,
//! `lsp_document_symbols`, `lsp_workspace_symbols`, and `lsp_rename_symbol`.
//!
//! Read tools return compact workspace-relative JSON. `lsp_rename_symbol`
//! applies the server's `WorkspaceEdit` to disk — every edited path is bounded
//! to the workspace, and the whole call is gated as a write by the cowork gate
//! at the orchestrator layer before it ever reaches here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_lsp::LspManager;

use crate::executor::fs_tools::assert_within_workspace;

/// Routes an `lsp_*` tool call to its handler. `mgr` is the daemon's shared
/// language-server manager; `workspace` roots relative paths and bounds writes.
pub(crate) async fn dispatch(
    tool_name: &str,
    input: &Value,
    mgr: &Arc<LspManager>,
    workspace: &Path,
) -> String {
    match tool_name {
        "lsp_definition" => definition(input, mgr, workspace).await,
        "lsp_references" => references(input, mgr, workspace).await,
        "lsp_hover" => hover(input, mgr).await,
        "lsp_document_symbols" => document_symbols(input, mgr, workspace).await,
        "lsp_workspace_symbols" => workspace_symbols(input, mgr, workspace).await,
        "lsp_rename_symbol" => rename_symbol(input, mgr, workspace).await,
        other => format!("error: unknown lsp tool '{other}'"),
    }
}

/// Parses the `{file, line, col}` positional triple common to most lsp tools.
fn position_args(input: &Value) -> Result<(PathBuf, u32, u32), String> {
    let file = input
        .get("file")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "error: 'file' is required".to_owned())?;
    let line = input
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| "error: 'line' (1-based) is required".to_owned())?;
    let col = input.get("col").and_then(Value::as_u64).unwrap_or(1);
    Ok((
        PathBuf::from(file),
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(col).unwrap_or(u32::MAX),
    ))
}

async fn definition(input: &Value, mgr: &Arc<LspManager>, workspace: &Path) -> String {
    let (file, line, col) = match position_args(input) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match mgr.definition(&file, line, col).await {
        Ok(res) => json!({ "locations": locations_to_json(workspace, &res) }).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

async fn references(input: &Value, mgr: &Arc<LspManager>, workspace: &Path) -> String {
    let (file, line, col) = match position_args(input) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match mgr.references(&file, line, col).await {
        Ok(res) => json!({ "references": locations_to_json(workspace, &res) }).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

async fn hover(input: &Value, mgr: &Arc<LspManager>) -> String {
    let (file, line, col) = match position_args(input) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match mgr.hover(&file, line, col).await {
        Ok(res) => json!({ "hover": hover_to_text(&res) }).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

async fn document_symbols(input: &Value, mgr: &Arc<LspManager>, workspace: &Path) -> String {
    let Some(file) = input.get("file").and_then(Value::as_str) else {
        return "error: 'file' is required".to_owned();
    };
    let path = PathBuf::from(file);
    match mgr.document_symbol(&path).await {
        Ok(res) => json!({ "symbols": symbols_to_json(workspace, &res, Some(&path)) }).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

async fn workspace_symbols(input: &Value, mgr: &Arc<LspManager>, workspace: &Path) -> String {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("");
    match mgr.workspace_symbol(query).await {
        Ok(res) => json!({ "symbols": symbols_to_json(workspace, &res, None) }).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

async fn rename_symbol(input: &Value, mgr: &Arc<LspManager>, workspace: &Path) -> String {
    let (file, line, col) = match position_args(input) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(new_name) = input
        .get("new_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return "error: 'new_name' is required".to_owned();
    };
    let edit = match mgr.rename(&file, line, col, new_name).await {
        Ok(e) => e,
        Err(e) => return format!("error: {e}"),
    };
    match apply_workspace_edit(workspace, &edit).await {
        Ok(files) if files.is_empty() => {
            "error: language server returned no edits for this rename".to_owned()
        }
        Ok(files) => json!({ "renamed": true, "changed_files": files }).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

// ── Result formatting ───────────────────────────────────────────────────────

/// Normalises a `Location` / `Location[]` / `LocationLink[]` result into a list
/// of `{ file, line, col }`, workspace-relative and 1-based.
fn locations_to_json(workspace: &Path, value: &Value) -> Vec<Value> {
    let items: Vec<&Value> = match value {
        Value::Array(a) => a.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    items
        .into_iter()
        .filter_map(|loc| {
            // Location has `uri`+`range`; LocationLink has `targetUri`+`targetRange`.
            let uri = loc
                .get("uri")
                .or_else(|| loc.get("targetUri"))
                .and_then(Value::as_str)?;
            let range = loc.get("range").or_else(|| loc.get("targetRange"))?;
            let (line, col) = range_start_1based(range);
            Some(json!({
                "file": rel_from_uri(workspace, uri),
                "line": line,
                "col": col,
            }))
        })
        .collect()
}

/// Extracts hover text from `MarkupContent` / `MarkedString` / arrays thereof.
fn hover_to_text(value: &Value) -> String {
    let contents = value.get("contents").unwrap_or(&Value::Null);
    marked_to_text(contents)
}

fn marked_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Value::Array(a) => a
            .iter()
            .map(marked_to_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Normalises `DocumentSymbol[]` (hierarchical) or `SymbolInformation[]` (flat,
/// with a `location`) into a flat list of `{ name, kind, file, line }`.
fn symbols_to_json(workspace: &Path, value: &Value, doc: Option<&Path>) -> Vec<Value> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sym in arr {
        collect_symbol(workspace, sym, doc, &mut out);
    }
    out
}

fn collect_symbol(workspace: &Path, sym: &Value, doc: Option<&Path>, out: &mut Vec<Value>) {
    let name = sym.get("name").and_then(Value::as_str).unwrap_or_default();
    let kind = symbol_kind(sym.get("kind").and_then(Value::as_u64).unwrap_or(0));
    // SymbolInformation carries `location`; DocumentSymbol carries `range`.
    let (file, line) = if let Some(loc) = sym.get("location") {
        let uri = loc.get("uri").and_then(Value::as_str).unwrap_or_default();
        let (line, _) = loc.get("range").map_or((1, 1), range_start_1based);
        (rel_from_uri(workspace, uri), line)
    } else {
        let (line, _) = sym.get("range").map_or((1, 1), range_start_1based);
        let file = doc.map_or_else(String::new, |p| rel_path(workspace, p));
        (file, line)
    };
    out.push(json!({ "name": name, "kind": kind, "file": file, "line": line }));
    // Recurse into hierarchical children (DocumentSymbol).
    if let Some(children) = sym.get("children").and_then(Value::as_array) {
        for child in children {
            collect_symbol(workspace, child, doc, out);
        }
    }
}

/// Applies a `WorkspaceEdit` to disk. Returns the workspace-relative paths that
/// were changed. Every target is bounded to the workspace.
async fn apply_workspace_edit(workspace: &Path, edit: &Value) -> Result<Vec<String>, String> {
    // Two encodings: `changes: { uri: TextEdit[] }` or
    // `documentChanges: [ { textDocument: { uri }, edits: TextEdit[] } ]`.
    let mut per_file: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            per_file.push((uri.clone(), edits.as_array().cloned().unwrap_or_default()));
        }
    }
    if let Some(doc_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for dc in doc_changes {
            let Some(uri) = dc
                .get("textDocument")
                .and_then(|t| t.get("uri"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let edits = dc.get("edits").and_then(Value::as_array).cloned();
            per_file.push((uri.to_owned(), edits.unwrap_or_default()));
        }
    }

    let mut changed = Vec::new();
    for (uri, edits) in per_file {
        if edits.is_empty() {
            continue;
        }
        let rel = rel_from_uri(workspace, &uri);
        let full = assert_within_workspace(workspace, &rel)?;
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| format!("error: cannot read {rel}: {e}"))?;
        let updated = apply_text_edits(&text, &edits)?;
        tokio::fs::write(&full, updated)
            .await
            .map_err(|e| format!("error: cannot write {rel}: {e}"))?;
        changed.push(rel);
    }
    changed.sort();
    changed.dedup();
    Ok(changed)
}

/// Applies LSP `TextEdit`s to `text`. Edits are applied end-to-start so earlier
/// byte offsets stay valid as later ones are spliced.
fn apply_text_edits(text: &str, edits: &[Value]) -> Result<String, String> {
    // Materialise (start_offset, end_offset, new_text), then apply descending.
    let mut spans: Vec<(usize, usize, String)> = Vec::with_capacity(edits.len());
    for e in edits {
        let range = e
            .get("range")
            .ok_or_else(|| "error: text edit missing range".to_owned())?;
        let (sl, sc) = range_start_0based(range);
        let (el, ec) = range_end_0based(range);
        let start = position_to_offset(text, sl, sc);
        let end = position_to_offset(text, el, ec);
        let new_text = e
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        spans.push((start, end.max(start), new_text));
    }
    spans.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = text.to_owned();
    for (start, end, new_text) in spans {
        if start <= out.len() && end <= out.len() {
            out.replace_range(start..end, &new_text);
        }
    }
    Ok(out)
}

/// Converts an LSP `Position` (0-based line, UTF-16 character offset) to a byte
/// offset into `text`. UTF-16 units are counted so non-ASCII lines map
/// correctly; ASCII collapses to the identity case.
fn position_to_offset(text: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    for (i, l) in text.split_inclusive('\n').enumerate() {
        if i as u32 == line {
            let mut u16_count = 0u32;
            for ch in l.chars() {
                if u16_count >= character {
                    break;
                }
                u16_count += ch.len_utf16() as u32;
                offset += ch.len_utf8();
            }
            return offset;
        }
        offset += l.len();
    }
    offset.min(text.len())
}

fn range_start_1based(range: &Value) -> (u64, u64) {
    let (l, c) = range_start_0based(range);
    (u64::from(l) + 1, u64::from(c) + 1)
}

fn range_start_0based(range: &Value) -> (u32, u32) {
    point(range.get("start"))
}

fn range_end_0based(range: &Value) -> (u32, u32) {
    point(range.get("end"))
}

fn point(p: Option<&Value>) -> (u32, u32) {
    let line = p
        .and_then(|p| p.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let ch = p
        .and_then(|p| p.get("character"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(ch).unwrap_or(u32::MAX),
    )
}

/// Maps a `file://` URI to a workspace-relative display path.
fn rel_from_uri(workspace: &Path, uri: &str) -> String {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    rel_path(workspace, Path::new(path))
}

fn rel_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Human-readable name for an LSP `SymbolKind` number.
fn symbol_kind(n: u64) -> &'static str {
    match n {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-parameter",
        _ => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_normalises_single_and_array() {
        let ws = Path::new("/proj");
        let single = json!({
            "uri": "file:///proj/src/a.rs",
            "range": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 8 } }
        });
        let out = locations_to_json(ws, &single);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["file"], "src/a.rs");
        assert_eq!(out[0]["line"], 5);
        assert_eq!(out[0]["col"], 3);

        let link_array = json!([{
            "targetUri": "file:///proj/src/b.rs",
            "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
        }]);
        let out = locations_to_json(ws, &link_array);
        assert_eq!(out[0]["file"], "src/b.rs");
        assert_eq!(out[0]["line"], 1);
    }

    #[test]
    fn hover_extracts_markup_and_marked_strings() {
        let markup = json!({ "contents": { "kind": "markdown", "value": "fn foo()" } });
        assert_eq!(hover_to_text(&markup), "fn foo()");
        let marked = json!({ "contents": "plain hover" });
        assert_eq!(hover_to_text(&marked), "plain hover");
        let arr = json!({ "contents": [ "a", { "language": "rust", "value": "b" } ] });
        assert_eq!(hover_to_text(&arr), "a\nb");
    }

    #[test]
    fn symbols_flatten_document_and_information_shapes() {
        let ws = Path::new("/proj");
        let doc = Path::new("src/a.rs");
        let hierarchical = json!([{
            "name": "Outer", "kind": 23,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } },
            "children": [{
                "name": "inner", "kind": 12,
                "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 3, "character": 0 } }
            }]
        }]);
        let out = symbols_to_json(ws, &hierarchical, Some(doc));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["name"], "Outer");
        assert_eq!(out[0]["kind"], "struct");
        assert_eq!(out[0]["file"], "src/a.rs");
        assert_eq!(out[1]["name"], "inner");
        assert_eq!(out[1]["kind"], "function");
        assert_eq!(out[1]["line"], 3);

        let info = json!([{
            "name": "g", "kind": 12,
            "location": {
                "uri": "file:///proj/src/z.rs",
                "range": { "start": { "line": 7, "character": 0 }, "end": { "line": 7, "character": 1 } }
            }
        }]);
        let out = symbols_to_json(ws, &info, None);
        assert_eq!(out[0]["file"], "src/z.rs");
        assert_eq!(out[0]["line"], 8);
    }

    #[test]
    fn position_to_offset_handles_ascii_and_unicode() {
        let text = "abc\nxéz\n";
        // line 0, char 2 → byte 2
        assert_eq!(position_to_offset(text, 0, 2), 2);
        // line 1: 'x'(1 u16) 'é'(1 u16) → char 2 lands after 'é' (x=1 byte, é=2 bytes)
        assert_eq!(position_to_offset(text, 1, 2), 4 + 3);
    }

    #[test]
    fn apply_text_edits_replaces_span() {
        let text = "let foo = 1;\n";
        let edits = vec![json!({
            "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 7 } },
            "newText": "bar"
        })];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "let bar = 1;\n");
    }

    #[test]
    fn apply_text_edits_multiple_end_to_start() {
        let text = "aXbXc";
        let edits = vec![
            json!({ "range": { "start": {"line":0,"character":1}, "end": {"line":0,"character":2} }, "newText": "1" }),
            json!({ "range": { "start": {"line":0,"character":3}, "end": {"line":0,"character":4} }, "newText": "2" }),
        ];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "a1b2c");
    }

    #[tokio::test]
    async fn apply_workspace_edit_writes_within_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let file = ws.path().join("m.rs");
        std::fs::write(&file, "let old = 1;\n").unwrap();
        let edit = json!({
            "changes": {
                format!("file://{}", file.display()): [{
                    "range": { "start": {"line":0,"character":4}, "end": {"line":0,"character":7} },
                    "newText": "new"
                }]
            }
        });
        let changed = apply_workspace_edit(ws.path(), &edit).await.unwrap();
        assert_eq!(changed, vec!["m.rs".to_owned()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "let new = 1;\n");
    }

    #[tokio::test]
    async fn apply_workspace_edit_rejects_escape() {
        let ws = tempfile::tempdir().unwrap();
        let edit = json!({
            "changes": {
                "file:///etc/passwd": [{
                    "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":1} },
                    "newText": "x"
                }]
            }
        });
        let err = apply_workspace_edit(ws.path(), &edit).await.unwrap_err();
        assert!(
            err.contains("workspace") || err.to_lowercase().contains("outside"),
            "got: {err}"
        );
    }
}
