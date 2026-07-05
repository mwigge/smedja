//! Minimal LSP stdio client: framing, handshake, document sync, requests.
//!
//! Speaks the `Content-Length` framing defined in the Language Server Protocol
//! specification. Handles `initialize` / `initialized`, opens documents
//! (`textDocument/didOpen` on first touch, `didChange` / `didSave` after edits),
//! correlates request/response pairs by `id`, and relays
//! `textDocument/publishDiagnostics` notifications. Server-initiated requests
//! (progress, log, window requests) receive a generic null response so the
//! server does not stall.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::types::{Diagnostic, Severity};

/// Bounded capacity for the internal inbound-message relay channel.
const INBOUND_CHANNEL_CAP: usize = 256;

/// A command issued from the manager into a running server's I/O loop.
///
/// Notifications (`DidOpen` / `DidChange`) are fire-and-forget; `Request`
/// carries a oneshot the loop resolves with the server's `result` value (or an
/// error) once the matching response arrives.
pub(crate) enum LspCommand {
    /// Ensure a document is open — sends `didOpen` on first touch, reading the
    /// file contents from disk. A no-op when the document is already open.
    DidOpen { path: PathBuf },
    /// Notify the server that a document changed on disk: re-reads the file,
    /// bumps its version, and sends `didChange` (full sync) followed by
    /// `didSave`. Opens the document first when it was not already open.
    DidChange { path: PathBuf },
    /// Issue an LSP request and resolve `reply` with the server's `result`.
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value>>,
    },
}

pub(crate) struct LspClient {
    /// Kept alive so `kill_on_drop` reaps the child when the client is dropped.
    _child: Child,
    stdin: ChildStdin,
    /// `Some` until [`LspClient::run`] moves the reader into its reader task.
    reader: Option<BufReader<ChildStdout>>,
    req_id: u64,
    workspace: PathBuf,
    /// Open documents: URI → last-sent version number.
    open_docs: HashMap<String, i32>,
}

impl LspClient {
    /// Spawns `binary args` in `workspace`, performs the `initialize` /
    /// `initialized` handshake, and returns a ready client.
    pub(crate) async fn spawn_and_init(
        binary: &str,
        args: &[&str],
        workspace: &Path,
    ) -> Result<Self> {
        let mut child = Command::new(binary)
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child has no stdout"))?;

        let mut client = Self {
            _child: child,
            stdin,
            reader: Some(BufReader::new(stdout)),
            req_id: 0,
            workspace: workspace.to_owned(),
            open_docs: HashMap::new(),
        };

        client.handshake().await?;
        Ok(client)
    }

    /// Sends one LSP message (Content-Length framing).
    async fn send(&mut self, msg: Value) -> Result<()> {
        let body = msg.to_string();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(body.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Sends `initialize`, waits for the response, then sends `initialized`.
    async fn handshake(&mut self) -> Result<()> {
        self.req_id += 1;
        let id = self.req_id;
        let uri = path_to_uri(&self.workspace);

        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": uri,
                "workspaceFolders": [{"uri": uri, "name": "workspace"}],
                "capabilities": {
                    "textDocument": {
                        "synchronization": {
                            "dynamicRegistration": false,
                            "didSave": true,
                            "willSave": false,
                            "willSaveWaitUntil": false
                        },
                        "publishDiagnostics": {
                            "relatedInformation": false,
                            "versionSupport": false,
                            "codeDescriptionSupport": false,
                            "dataSupport": false
                        },
                        "definition": { "dynamicRegistration": false, "linkSupport": true },
                        "references": { "dynamicRegistration": false },
                        "hover": {
                            "dynamicRegistration": false,
                            "contentFormat": ["markdown", "plaintext"]
                        },
                        "documentSymbol": {
                            "dynamicRegistration": false,
                            "hierarchicalDocumentSymbolSupport": true
                        },
                        "rename": { "dynamicRegistration": false, "prepareSupport": false }
                    },
                    "workspace": {
                        "workspaceFolders": true,
                        "symbol": { "dynamicRegistration": false },
                        "applyEdit": true
                    },
                    "window": {
                        "workDoneProgress": false
                    }
                },
                "clientInfo": {
                    "name": "smedja",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }))
        .await?;

        // Wait for the initialize response (id matches); skip notifications.
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| anyhow!("reader already taken"))?;
        loop {
            let msg = read_message(reader).await?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                break;
            }
            // Otherwise it's a notification or a different response — ignore.
        }

        // Send initialized notification.
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .await?;

        Ok(())
    }

    /// Sends `textDocument/didOpen` for `path` when it is not already open.
    async fn did_open(&mut self, path: &Path) -> Result<()> {
        let abs = self.absolute(path);
        let uri = path_to_uri(&abs);
        if self.open_docs.contains_key(&uri) {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(&abs).await.unwrap_or_default();
        let version = 1;
        self.open_docs.insert(uri.clone(), version);
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id(&abs),
                    "version": version,
                    "text": text
                }
            }
        }))
        .await
    }

    /// Sends `didChange` (full sync) + `didSave` for `path`, opening it first
    /// when needed. Re-reads the current on-disk contents and bumps the version.
    async fn did_change(&mut self, path: &Path) -> Result<()> {
        self.did_open(path).await?;
        let abs = self.absolute(path);
        let uri = path_to_uri(&abs);
        let text = tokio::fs::read_to_string(&abs).await.unwrap_or_default();
        let version = self
            .open_docs
            .get(&uri)
            .copied()
            .unwrap_or(1)
            .saturating_add(1);
        self.open_docs.insert(uri.clone(), version);
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }
        }))
        .await?;
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": {
                "textDocument": { "uri": uri },
                "text": text
            }
        }))
        .await
    }

    /// Resolves `path` against the workspace root when it is relative.
    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_owned()
        } else {
            self.workspace.join(path)
        }
    }

    /// Handles one manager command inside the I/O loop.
    ///
    /// `pending` is the id → reply-sender map used to correlate request
    /// responses read from the server.
    async fn handle_command(
        &mut self,
        cmd: LspCommand,
        pending: &mut HashMap<u64, oneshot::Sender<Result<Value>>>,
    ) -> Result<()> {
        match cmd {
            LspCommand::DidOpen { path } => {
                self.did_open(&path).await?;
            }
            LspCommand::DidChange { path } => {
                self.did_change(&path).await?;
            }
            LspCommand::Request {
                method,
                params,
                reply,
            } => {
                self.req_id += 1;
                let id = self.req_id;
                pending.insert(id, reply);
                if let Err(e) = self
                    .send(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": method,
                        "params": params
                    }))
                    .await
                {
                    if let Some(tx) = pending.remove(&id) {
                        let _ = tx.send(Err(anyhow!("failed to send request: {e}")));
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs the client I/O loop until the server disconnects.
    ///
    /// A dedicated reader task relays every server message through an internal
    /// channel; the loop multiplexes those messages with manager commands from
    /// `cmd_rx`. Each `textDocument/publishDiagnostics` notification is parsed
    /// and forwarded through `diag_tx`; responses resolve the matching pending
    /// request; server-initiated requests receive a null result.
    pub(crate) async fn run(
        mut self,
        diag_tx: mpsc::Sender<Vec<Diagnostic>>,
        mut cmd_rx: mpsc::Receiver<LspCommand>,
    ) -> Result<()> {
        let workspace = self.workspace.clone();

        // Move the reader into a task so a `select!` on inbound messages never
        // cancels a partially-read frame (read_message is not cancel-safe).
        let Some(mut reader) = self.reader.take() else {
            bail!("LSP client run called without a reader");
        };
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<Value>(INBOUND_CHANNEL_CAP);
        let reader_task = tokio::spawn(async move {
            while let Ok(msg) = read_message(&mut reader).await {
                if inbound_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let mut pending: HashMap<u64, oneshot::Sender<Result<Value>>> = HashMap::new();
        let mut cmds_open = true;

        let result = loop {
            tokio::select! {
                biased;
                msg = inbound_rx.recv() => {
                    let Some(msg) = msg else {
                        break Err(anyhow!("LSP server closed stdout"));
                    };
                    self.handle_message(msg, &workspace, &diag_tx, &mut pending).await;
                }
                cmd = cmd_rx.recv(), if cmds_open => {
                    match cmd {
                        Some(cmd) => {
                            if let Err(e) = self.handle_command(cmd, &mut pending).await {
                                break Err(e);
                            }
                        }
                        None => cmds_open = false,
                    }
                }
            }
        };

        reader_task.abort();
        // Fail any still-pending requests so callers unblock promptly.
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(anyhow!("LSP server disconnected")));
        }
        result
    }

    /// Dispatches one inbound server message.
    async fn handle_message(
        &mut self,
        msg: Value,
        workspace: &Path,
        diag_tx: &mpsc::Sender<Vec<Diagnostic>>,
        pending: &mut HashMap<u64, oneshot::Sender<Result<Value>>>,
    ) {
        let method = msg.get("method").and_then(Value::as_str);

        match method {
            Some("textDocument/publishDiagnostics") => {
                let diags = parse_diagnostics(workspace, &msg["params"]);
                let _ = diag_tx.send(diags).await;
            }
            // A server-initiated request (has both method and id): reply null so
            // the server does not stall.
            Some(_) => {
                if let Some(id) = msg.get("id") {
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null });
                    let _ = self.send(resp).await;
                }
            }
            // No method: a response to one of our requests.
            None => {
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.remove(&id) {
                        if let Some(err) = msg.get("error") {
                            let m = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("LSP error");
                            let _ = tx.send(Err(anyhow!("{m}")));
                        } else {
                            let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                }
            }
        }
    }
}

/// Reads one LSP message (Content-Length framing). Blocks until complete.
async fn read_message(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length: Option<usize> = None;

    // Read headers until empty line.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("LSP server closed stdout");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
            content_length = rest.trim().parse().ok();
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("no Content-Length header"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

/// Parses `textDocument/publishDiagnostics` params into our `Diagnostic` type.
fn parse_diagnostics(workspace: &Path, params: &Value) -> Vec<Diagnostic> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let file = uri_to_rel_path(workspace, uri);

    let Some(items) = params.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut out: Vec<Diagnostic> = items
        .iter()
        .filter_map(|d| {
            let line = u32::try_from(
                d["range"]["start"]["line"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(1),
            )
            .unwrap_or(u32::MAX);
            let col = u32::try_from(
                d["range"]["start"]["character"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(1),
            )
            .unwrap_or(u32::MAX);
            let severity_n = d.get("severity").and_then(Value::as_u64).unwrap_or(1);
            let code = d
                .get("code")
                .and_then(|c| {
                    c.as_str()
                        .map(str::to_owned)
                        .or_else(|| c.as_u64().map(|n| n.to_string()))
                })
                .filter(|s| !s.is_empty());
            let message = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();

            if message.is_empty() {
                return None;
            }

            Some(Diagnostic {
                file: file.clone(),
                line,
                col,
                severity: Severity::from_lsp(severity_n),
                code,
                message,
            })
        })
        .collect();

    out.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.line.cmp(&b.line)));
    out
}

/// Maps a file path to the LSP `languageId` for its extension.
fn language_id(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
    {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "go" => "go",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" => "c",
        "h" | "hpp" | "hh" | "hxx" | "cpp" | "cc" | "cxx" | "c++" => "cpp",
        _ => "plaintext",
    }
}

pub(crate) fn path_to_uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn uri_to_rel_path(workspace: &Path, uri: &str) -> PathBuf {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);
    let abs = PathBuf::from(path_str);
    abs.strip_prefix(workspace)
        .map(PathBuf::from)
        .unwrap_or(abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_id_maps_known_extensions() {
        assert_eq!(language_id(Path::new("a/b.rs")), "rust");
        assert_eq!(language_id(Path::new("x.py")), "python");
        assert_eq!(language_id(Path::new("x.tsx")), "typescriptreact");
        assert_eq!(language_id(Path::new("x.hpp")), "cpp");
        assert_eq!(language_id(Path::new("x.unknown")), "plaintext");
        assert_eq!(language_id(Path::new("Makefile")), "plaintext");
    }

    #[test]
    fn uri_round_trips_relative_to_workspace() {
        let ws = Path::new("/home/u/proj");
        let uri = path_to_uri(&ws.join("src/main.rs"));
        assert_eq!(uri, "file:///home/u/proj/src/main.rs");
        assert_eq!(uri_to_rel_path(ws, &uri), PathBuf::from("src/main.rs"));
    }

    /// Locates the `mock_lsp` example binary built alongside the tests, or
    /// `None` when it has not been compiled (the caller then skips).
    fn mock_server_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        // .../target/<profile>/deps/<test-bin>
        let profile_dir = exe.parent()?.parent()?;
        let candidate = profile_dir.join("examples").join("mock_lsp");
        candidate.exists().then_some(candidate)
    }

    /// Creates a unique scratch workspace directory for a test.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "smedja-lsp-client-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn did_open_triggers_diagnostics_and_requests_correlate() {
        let Some(server) = mock_server_path() else {
            eprintln!("mock_lsp example not built; skipping live-client test");
            return;
        };
        let ws = scratch_dir("open");
        let file = ws.join("a.rs");
        std::fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();

        let client = LspClient::spawn_and_init(server.to_str().unwrap(), &[], &ws)
            .await
            .expect("spawn mock server");
        let (diag_tx, mut diag_rx) = mpsc::channel::<Vec<Diagnostic>>(16);
        let (cmd_tx, cmd_rx) = mpsc::channel::<LspCommand>(16);
        tokio::spawn(async move {
            let _ = client.run(diag_tx, cmd_rx).await;
        });

        // First touch of the file must send didOpen, which the mock answers with
        // a publishDiagnostics carrying the planted error.
        cmd_tx
            .send(LspCommand::DidOpen { path: file.clone() })
            .await
            .unwrap();
        let diags = tokio::time::timeout(std::time::Duration::from_secs(5), diag_rx.recv())
            .await
            .expect("diagnostics within timeout")
            .expect("diagnostics channel open");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].message, "planted error");

        // A definition request must correlate to its response by id.
        let (rtx, rrx) = oneshot::channel();
        cmd_tx
            .send(LspCommand::Request {
                method: "textDocument/definition".to_owned(),
                params: json!({
                    "textDocument": { "uri": path_to_uri(&file) },
                    "position": { "line": 0, "character": 0 }
                }),
                reply: rtx,
            })
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), rrx)
            .await
            .expect("response within timeout")
            .expect("reply not dropped")
            .expect("server ok");
        assert!(result.get("uri").and_then(Value::as_str).is_some());

        // A hover request issued right after must also correlate correctly,
        // proving id-based multiplexing (not FIFO-by-luck).
        let (htx, hrx) = oneshot::channel();
        cmd_tx
            .send(LspCommand::Request {
                method: "textDocument/hover".to_owned(),
                params: json!({
                    "textDocument": { "uri": path_to_uri(&file) },
                    "position": { "line": 0, "character": 0 }
                }),
                reply: htx,
            })
            .await
            .unwrap();
        let hover = tokio::time::timeout(std::time::Duration::from_secs(5), hrx)
            .await
            .expect("hover within timeout")
            .expect("reply not dropped")
            .expect("server ok");
        assert_eq!(hover["contents"]["value"], "mock hover");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn parse_diagnostics_converts_to_one_based() {
        let params = json!({
            "uri": "file:///home/u/proj/src/main.rs",
            "diagnostics": [{
                "range": { "start": { "line": 4, "character": 8 }, "end": { "line": 4, "character": 9 } },
                "severity": 1,
                "code": "E0308",
                "message": "mismatched types"
            }]
        });
        let diags = parse_diagnostics(Path::new("/home/u/proj"), &params);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 5);
        assert_eq!(diags[0].col, 9);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("E0308"));
        assert_eq!(diags[0].file, PathBuf::from("src/main.rs"));
    }
}
