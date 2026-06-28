//! Minimal LSP stdio client: framing, handshake, diagnostic parsing.
//!
//! Speaks the `Content-Length` framing defined in the Language Server Protocol
//! specification. Handles `initialize` / `initialized` and then listens for
//! `textDocument/publishDiagnostics` notifications. All other server messages
//! (progress, log, window requests) are silently ignored.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::types::{Diagnostic, Severity};

pub(crate) struct LspClient {
    _child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    req_id: u64,
    workspace: PathBuf,
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
            reader: BufReader::new(stdout),
            req_id: 0,
            workspace: workspace.to_owned(),
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

    /// Reads one LSP message. Blocks until a complete message arrives.
    async fn recv(&mut self) -> Result<Value> {
        let mut content_length: Option<usize> = None;

        // Read headers until empty line.
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
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
        self.reader.read_exact(&mut buf).await?;
        Ok(serde_json::from_slice(&buf)?)
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
                        "publishDiagnostics": {
                            "relatedInformation": false,
                            "versionSupport": false,
                            "codeDescriptionSupport": false,
                            "dataSupport": false
                        }
                    },
                    "workspace": {
                        "workspaceFolders": true
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
        loop {
            let msg = self.recv().await?;
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

    /// Runs the notification loop until the server disconnects.
    ///
    /// Each `textDocument/publishDiagnostics` notification is parsed and sent
    /// through `diag_tx`. Server-side requests (e.g. `window/workDoneProgress/create`)
    /// receive a generic null response so the server does not stall.
    pub(crate) async fn run(&mut self, diag_tx: mpsc::Sender<Vec<Diagnostic>>) -> Result<()> {
        let workspace = self.workspace.clone();

        loop {
            let msg = self.recv().await?;

            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();

            match method {
                "textDocument/publishDiagnostics" => {
                    let diags = parse_diagnostics(&workspace, &msg["params"]);
                    let _ = diag_tx.send(diags).await;
                }
                // Respond to server-initiated requests so they don't stall.
                m if !m.is_empty() && msg.get("id").is_some() => {
                    if let Some(id) = msg.get("id") {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": serde_json::Value::Null
                        });
                        let _ = self.send(resp).await;
                    }
                }
                _ => {}
            }
        }
    }
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

fn path_to_uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn uri_to_rel_path(workspace: &Path, uri: &str) -> PathBuf {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);
    let abs = PathBuf::from(path_str);
    abs.strip_prefix(workspace)
        .map(PathBuf::from)
        .unwrap_or(abs)
}
