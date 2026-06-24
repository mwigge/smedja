//! MCP stdio transport — newline-framed JSON-RPC 2.0 over a child process.
//!
//! Spawns the configured MCP server as a child process (its command is stored
//! in the registered server's `url` field) using [`tokio::process`], writing one
//! line of JSON per request to the child's stdin and reading one line of JSON
//! per response from its stdout. The child is spawned lazily on first call and
//! reused across calls; [`Drop`] kills it so no orphan process remains.
//!
//! All I/O uses async APIs — there is no blocking `std::io` on the async path.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use smedja_ingot::McpServer;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::mcp_http::McpHttpClient;

/// Per-call read timeout for a stdio child response.
const STDIO_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A selectable MCP transport: HTTP, or a stdio child process.
pub(crate) enum McpTransport {
    /// HTTP JSON-RPC transport (existing default).
    Http(McpHttpClient),
    /// stdio child-process JSON-RPC transport.
    ///
    /// Boxed because the stdio client (which owns child-process handles) is much
    /// larger than the HTTP client, keeping the enum compact.
    Stdio(Box<McpStdioClient>),
}

impl McpTransport {
    /// Builds the transport for `server`, selecting by its `transport` field.
    ///
    /// `"stdio"` builds a stdio child-process client; `"http"`, an empty value,
    /// or any unrecognised value defaults to the HTTP client for
    /// back-compatibility. `token` is the resolved outbound Bearer credential
    /// for the HTTP transport (ignored by stdio).
    ///
    /// # Errors
    ///
    /// Returns the underlying [`reqwest::Error`] if the HTTP client cannot be
    /// built.
    pub(crate) fn for_server(server: &McpServer, token: &str) -> Result<Self, reqwest::Error> {
        match server.transport.as_str() {
            "stdio" => Ok(Self::Stdio(Box::new(McpStdioClient::new(&server.url)))),
            _ => Ok(Self::Http(McpHttpClient::new(&server.url, token)?)),
        }
    }

    /// Calls a tool over the selected transport.
    ///
    /// # Errors
    ///
    /// Returns an error string on transport failure or a tool-level error.
    pub(crate) async fn call_tool(&self, name: &str, input: &Value) -> Result<String, String> {
        match self {
            Self::Http(client) => client.call_tool(name, input).await,
            Self::Stdio(client) => client.call_tool(name, input).await,
        }
    }

    /// Lists tools over the selected transport.
    ///
    /// # Errors
    ///
    /// Returns an error string on transport failure or a parse failure.
    pub(crate) async fn list_tools(&self) -> Result<Vec<crate::mcp_http::McpTool>, String> {
        match self {
            Self::Http(client) => client.list_tools().await,
            Self::Stdio(client) => client.list_tools().await,
        }
    }
}

/// A stdio MCP client that owns a lazily-spawned child process.
pub(crate) struct McpStdioClient {
    command: String,
    child: Mutex<Option<StdioChild>>,
}

/// The spawned child and its framed stdin/stdout handles.
struct StdioChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpStdioClient {
    /// Creates a stdio client for the given `command` (spawned lazily on first
    /// call). The command is split on whitespace into program and arguments.
    #[must_use]
    pub(crate) fn new(command: &str) -> Self {
        Self {
            command: command.to_owned(),
            child: Mutex::new(None),
        }
    }

    /// Spawns the child process with piped stdin/stdout.
    fn spawn(&self) -> Result<StdioChild, String> {
        let mut parts = self.command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| "empty stdio command".to_owned())?;
        let mut cmd = Command::new(program);
        cmd.args(parts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn stdio MCP server: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child stdin unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "child stdout unavailable".to_owned())?;
        Ok(StdioChild {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Sends a JSON-RPC request line and reads one response line, applying the
    /// per-call read timeout. Spawns (or re-spawns) the child as needed.
    async fn round_trip(&self, request: &Value) -> Result<Value, String> {
        self.round_trip_with_timeout(request, STDIO_READ_TIMEOUT)
            .await
    }

    /// Like [`round_trip`](Self::round_trip) but with an explicit read timeout,
    /// so the timeout-to-error mapping is exercisable in tests.
    async fn round_trip_with_timeout(
        &self,
        request: &Value,
        read_timeout: Duration,
    ) -> Result<Value, String> {
        let mut guard = self.child.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn()?);
        }
        let child = guard
            .as_mut()
            .ok_or_else(|| "stdio child not spawned".to_owned())?;

        let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
        line.push('\n');
        child
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("stdio write failed: {e}"))?;
        child
            .stdin
            .flush()
            .await
            .map_err(|e| format!("stdio flush failed: {e}"))?;

        let mut response_line = String::new();
        let read =
            tokio::time::timeout(read_timeout, child.stdout.read_line(&mut response_line)).await;
        match read {
            Ok(Ok(0)) => Err("stdio MCP server closed the connection".to_owned()),
            Ok(Ok(_)) => serde_json::from_str(response_line.trim_end())
                .map_err(|e| format!("stdio response parse failed: {e}")),
            Ok(Err(e)) => Err(format!("stdio read failed: {e}")),
            Err(_) => Err("stdio MCP server timed out".to_owned()),
        }
    }

    /// Calls `tools/call` over stdio and returns the result JSON as a string.
    ///
    /// # Errors
    ///
    /// Returns an error string on spawn failure, a read timeout, transport
    /// closure, or a JSON-RPC error from the child.
    pub(crate) async fn call_tool(&self, name: &str, input: &Value) -> Result<String, String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": input }
        });
        let resp = self.round_trip(&request).await?;
        if let Some(err) = resp.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("MCP server error: {msg}"));
        }
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        Ok(serde_json::to_string(&result).unwrap_or_default())
    }

    /// Calls `tools/list` over stdio and returns the parsed tool list.
    ///
    /// # Errors
    ///
    /// Returns an error string on spawn failure, a read timeout, or a parse
    /// failure.
    pub(crate) async fn list_tools(&self) -> Result<Vec<crate::mcp_http::McpTool>, String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        });
        let resp = self.round_trip(&request).await?;
        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Ok(tools)
    }
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        // Best-effort synchronous teardown: kill the child if one is held.
        // `kill_on_drop(true)` is the primary guard; this start_kill is a belt.
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut held) = guard.take() {
                let _ = held.child.start_kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use serde_json::json;
    use smedja_ingot::McpServer;

    use super::{McpStdioClient, McpTransport};

    /// Writes a small POSIX-shell MCP stub to a temp file and returns its path.
    ///
    /// The stub reads newline-framed JSON-RPC requests on stdin and replies with
    /// a canned newline-framed result per line — the lowest-common-denominator
    /// MCP stdio framing.
    fn write_echo_server(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let script = dir.join("mcp_stub.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        write!(
            f,
            "#!/bin/sh\nwhile IFS= read -r line; do\n  printf '%s\\n' '{body}'\ndone\n"
        )
        .unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    #[tokio::test]
    async fn stdio_list_tools_round_trips_newline_framed_json() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"ping","description":"p","input_schema":{}}]}}"#;
        let script = write_echo_server(dir.path(), body);
        let client = McpStdioClient::new(&format!("sh {}", script.display()));

        let tools = client.list_tools().await.expect("list_tools must succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");
    }

    #[tokio::test]
    async fn stdio_call_tool_returns_result_and_reuses_child() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"stdio-ok"}],"isError":false}}"#;
        let script = write_echo_server(dir.path(), body);
        let client = McpStdioClient::new(&format!("sh {}", script.display()));

        let first = client.call_tool("greet", &json!({})).await.unwrap();
        assert!(first.contains("stdio-ok"), "got: {first}");
        // A second call reuses the spawned child (no panic, same canned reply).
        let second = client.call_tool("greet", &json!({})).await.unwrap();
        assert!(second.contains("stdio-ok"), "got: {second}");
    }

    #[tokio::test]
    async fn stdio_stalled_child_times_out_to_error() {
        // A child that never replies must surface a timeout error string rather
        // than hanging. We inject a short read timeout to exercise the mapping.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("silent.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let client = McpStdioClient::new(&format!("sh {}", script.display()));

        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
        let result = client
            .round_trip_with_timeout(&request, std::time::Duration::from_millis(120))
            .await;
        let err = result.expect_err("a silent child must time out to an error");
        assert!(
            err.contains("timed out"),
            "timeout must map to a tool-error string; got: {err}"
        );
        // Dropping the client tears down the child via kill_on_drop.
    }

    #[test]
    fn transport_defaults_to_http_for_unknown_value() {
        let server = McpServer {
            id: "1".into(),
            name: "n".into(),
            url: "https://example.com/mcp".into(),
            transport: "carrier-pigeon".into(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        };
        let t = McpTransport::for_server(&server, "").unwrap();
        assert!(matches!(t, McpTransport::Http(_)));
    }

    #[test]
    fn transport_selects_http_for_empty_value() {
        let server = McpServer {
            id: "1".into(),
            name: "n".into(),
            url: "https://example.com/mcp".into(),
            transport: String::new(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        };
        let t = McpTransport::for_server(&server, "").unwrap();
        assert!(matches!(t, McpTransport::Http(_)));
    }

    #[test]
    fn transport_selects_stdio_for_stdio_value() {
        let server = McpServer {
            id: "1".into(),
            name: "n".into(),
            url: "echo".into(),
            transport: "stdio".into(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        };
        let t = McpTransport::for_server(&server, "").unwrap();
        assert!(matches!(t, McpTransport::Stdio(_)));
    }

    #[tokio::test]
    async fn stdio_server_dispatches_via_stdio_client() {
        // An end-to-end-ish check: a stdio-transport server routes through the
        // stdio client and returns the child's canned result.
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"via-stdio"}],"isError":false}}"#;
        let script = write_echo_server(dir.path(), body);
        let server = McpServer {
            id: "1".into(),
            name: "stdio-srv".into(),
            url: format!("sh {}", script.display()),
            transport: "stdio".into(),
            tools_json: "[]".into(),
            last_refresh: 0.0,
        };
        let transport = McpTransport::for_server(&server, "").unwrap();
        let result = transport.call_tool("greet", &json!({})).await.unwrap();
        assert!(result.contains("via-stdio"), "got: {result}");
    }
}
