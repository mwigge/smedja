//! Minimal mock language server used by `smedja-lsp` client tests.
//!
//! Speaks the LSP `Content-Length` framing over stdio. It answers just enough
//! of the protocol to exercise the real client: the `initialize` handshake,
//! `didOpen`-triggered `publishDiagnostics`, and `definition` / `hover` /
//! `documentSymbol` / `rename` requests with canned results. After a
//! `didChange`/`didSave` it re-publishes diagnostics (cleared) so the post-edit
//! refresh path can be observed.
//!
//! It is compiled as an example (built by `cargo test`) and spawned by the
//! client unit tests; it is not part of the shipped library surface.

use std::io::{Read, Write};

use serde_json::{json, Value};

fn main() {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut last_uri: Option<String> = None;

    while let Some(msg) = read_message(&mut stdin) {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                respond(
                    &mut stdout,
                    &id,
                    json!({ "capabilities": { "renameProvider": true } }),
                );
            }
            "shutdown" => respond(&mut stdout, &id, Value::Null),
            "exit" => break,
            "textDocument/didOpen" => {
                let uri = msg["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                last_uri = Some(uri.clone());
                publish_diagnostics(&mut stdout, &uri, true);
            }
            "textDocument/didSave" => {
                let uri = msg["params"]["textDocument"]["uri"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| last_uri.clone())
                    .unwrap_or_default();
                // Re-publish after a save: still one error (the planted one),
                // proving the refresh path observes a fresh publish.
                publish_diagnostics(&mut stdout, &uri, true);
            }
            "textDocument/definition" => {
                let uri = msg["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default();
                respond(
                    &mut stdout,
                    &id,
                    json!({
                        "uri": uri,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 4 }
                        }
                    }),
                );
            }
            "textDocument/hover" => {
                respond(
                    &mut stdout,
                    &id,
                    json!({ "contents": { "kind": "markdown", "value": "mock hover" } }),
                );
            }
            "textDocument/documentSymbol" => {
                respond(
                    &mut stdout,
                    &id,
                    json!([{
                        "name": "mock_symbol",
                        "kind": 12,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 2, "character": 0 }
                        },
                        "selectionRange": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 4 }
                        }
                    }]),
                );
            }
            "textDocument/rename" => {
                let uri = msg["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default();
                let new_name = msg["params"]["newName"].as_str().unwrap_or("renamed");
                respond(
                    &mut stdout,
                    &id,
                    json!({
                        "changes": {
                            uri: [{
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 3 }
                                },
                                "newText": new_name
                            }]
                        }
                    }),
                );
            }
            _ => {
                // Any other server-addressed request gets a null result.
                if id.is_some() {
                    respond(&mut stdout, &id, Value::Null);
                }
            }
        }
    }
}

fn respond(out: &mut impl Write, id: &Option<Value>, result: Value) {
    let Some(id) = id else { return };
    send(
        out,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn publish_diagnostics(out: &mut impl Write, uri: &str, error: bool) {
    let diagnostics = if error {
        json!([{
            "range": {
                "start": { "line": 1, "character": 4 },
                "end": { "line": 1, "character": 9 }
            },
            "severity": 1,
            "code": "E0001",
            "message": "planted error"
        }])
    } else {
        json!([])
    };
    send(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": diagnostics }
        }),
    );
}

fn send(out: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn read_message(input: &mut impl Read) -> Option<Value> {
    // Read headers byte-by-byte until the blank line.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if input.read(&mut byte).ok()? == 0 {
            return None;
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_str = String::from_utf8_lossy(&header);
    let len: usize = header_str
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())?;
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}
