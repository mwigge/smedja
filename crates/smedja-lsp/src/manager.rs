//! `LspManager` — detects available language servers, starts them, aggregates
//! diagnostics from all active servers, and exposes a `watch` channel so
//! consumers get push updates with zero polling overhead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

/// Maximum restart attempts per language server after an unexpected exit.
const MAX_RESTART_ATTEMPTS: u32 = 3;
/// Delay (seconds) before each restart attempt: 1 s, 5 s, 30 s.
const RESTART_DELAYS_SECS: &[u64] = &[1, 5, 30];
/// Bounded channel capacity for server events and diagnostic relay.
const EVENT_CHANNEL_CAP: usize = 512;
use which::which;

use crate::client::{path_to_uri, LspClient, LspCommand};
use crate::types::{Diagnostic, LspSnapshot, ServerState, ServerStatus};

/// Shared registry of live per-server command channels, keyed by server name.
/// Populated when a server becomes ready and cleared when it disconnects, so the
/// request methods only ever address servers that can actually answer.
type ConnMap = Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<LspCommand>>>>;

/// Bounded per-server command-channel capacity.
const CMD_CHANNEL_CAP: usize = 64;

/// Hard ceiling on a single LSP request round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Supported language servers, in priority order.
/// A server is started only when its binary is on `$PATH` **and** the workspace
/// contains one of its `markers` — otherwise clangd (etc.) would start on a Rust
/// project just because the binary happens to be installed.
struct ServerSpec {
    name: &'static str,
    binary: &'static str,
    args: &'static [&'static str],
    /// Project-root marker files that indicate this language is in use.
    markers: &'static [&'static str],
    /// File extensions (without the dot) this server answers requests for.
    extensions: &'static [&'static str],
}

const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "rust-analyzer",
        binary: "rust-analyzer",
        args: &[],
        markers: &["Cargo.toml"],
        extensions: &["rs"],
    },
    ServerSpec {
        name: "pyright",
        binary: "pyright-langserver",
        args: &["--stdio"],
        markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
        extensions: &["py", "pyi"],
    },
    ServerSpec {
        name: "gopls",
        binary: "gopls",
        args: &[],
        markers: &["go.mod", "go.work"],
        extensions: &["go"],
    },
    ServerSpec {
        name: "typescript-language-server",
        binary: "typescript-language-server",
        args: &["--stdio"],
        markers: &["package.json", "tsconfig.json"],
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
    },
    ServerSpec {
        name: "clangd",
        binary: "clangd",
        args: &[],
        markers: &[
            "compile_commands.json",
            "CMakeLists.txt",
            "Makefile",
            ".clangd",
        ],
        extensions: &["c", "h", "cpp", "cc", "cxx", "hpp", "hh", "hxx"],
    },
];

/// Returns the name of the server that answers requests for `ext`.
fn server_name_for_ext(ext: &str) -> Option<&'static str> {
    SERVERS
        .iter()
        .find(|s| s.extensions.contains(&ext))
        .map(|s| s.name)
}

/// True when `workspace` contains at least one of `spec`'s project markers.
fn workspace_has_marker(workspace: &std::path::Path, spec: &ServerSpec) -> bool {
    spec.markers.iter().any(|m| workspace.join(m).exists())
}

/// Resolves `name` to an executable path, checking `$PATH` first, then
/// `~/.cargo/bin` — which Rustup populates but which may be absent from the
/// daemon's `$PATH` when smdjad is launched as a service or outside a login shell.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    if let Ok(p) = which(name) {
        return Some(p);
    }
    let cargo_bin = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cargo").join("bin").join(name))?;
    cargo_bin.is_file().then_some(cargo_bin)
}

/// Internal event sent from a per-server task to the aggregator.
enum ServerEvent {
    Starting {
        name: String,
    },
    Ready {
        name: String,
    },
    Degraded {
        name: String,
        reason: String,
    },
    Diagnostics {
        name: String,
        diags: Vec<Diagnostic>,
    },
}

/// Manages one or more language server child processes and exposes their
/// combined diagnostic output through a `tokio::sync::watch` channel.
pub struct LspManager {
    tx: watch::Sender<LspSnapshot>,
    rx: watch::Receiver<LspSnapshot>,
    /// Handle to the `run_all` background task; aborted on `shutdown()`.
    runner: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The workspace the servers are currently rooted at, so re-rooting can be a
    /// no-op when unchanged.
    current_ws: std::sync::Mutex<Option<PathBuf>>,
    /// Live per-server command channels, used to issue requests and doc syncs.
    conns: ConnMap,
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LspManager {
    /// Creates a new manager. Call [`start`](Self::start) to detect and launch
    /// servers; the watch channel starts with an empty snapshot.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(LspSnapshot::default());
        Self {
            tx,
            rx,
            runner: std::sync::Mutex::new(None),
            current_ws: std::sync::Mutex::new(None),
            conns: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Returns a clone of the watch receiver (cheap — shared ref-count).
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<LspSnapshot> {
        self.rx.clone()
    }

    /// Returns the current snapshot without subscribing.
    #[must_use]
    pub fn snapshot(&self) -> LspSnapshot {
        self.rx.borrow().clone()
    }

    /// Detects servers available on `$PATH` and starts them for `workspace`.
    /// Returns immediately; all I/O runs in background tokio tasks.
    pub fn start(&self, workspace: PathBuf) {
        *self
            .current_ws
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(workspace.clone());
        let tx = self.tx.clone();
        let conns = Arc::clone(&self.conns);
        let handle = tokio::spawn(async move {
            run_all(workspace, tx, conns).await;
        });
        *self
            .runner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Re-roots the language servers at `workspace` when it differs from the
    /// current root (a no-op otherwise). Used when the daemon learns a session's
    /// real project directory — it boots rooted at the daemon cwd (often `$HOME`,
    /// where no project markers exist, so nothing starts), and this points it at
    /// the actual repo so e.g. rust-analyzer starts for a `Cargo.toml` project.
    pub fn ensure_workspace(&self, workspace: PathBuf) {
        {
            let cur = self
                .current_ws
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cur.as_deref() == Some(workspace.as_path()) {
                return;
            }
        }
        self.shutdown();
        self.start(workspace);
    }

    /// Aborts the background server manager and all child processes.
    /// Child processes are killed because `LspClient` uses `kill_on_drop(true)`.
    pub fn shutdown(&self) {
        if let Some(h) = self
            .runner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            h.abort();
        }
        self.conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    // ── LSP requests (agent tools) ──────────────────────────────────────────

    /// `textDocument/definition` for the symbol at `file`:`line`:`col`
    /// (1-based). Returns the server's raw `result` (a `Location`,
    /// `Location[]`, or `LocationLink[]`).
    ///
    /// # Errors
    /// When no server serves the file's language, the request times out, or the
    /// server returns an error.
    pub async fn definition(&self, file: &Path, line: u32, col: u32) -> Result<Value> {
        self.position_request(file, "textDocument/definition", line, col, None)
            .await
    }

    /// `textDocument/references` for the symbol at `file`:`line`:`col`.
    ///
    /// # Errors
    /// See [`LspManager::definition`].
    pub async fn references(&self, file: &Path, line: u32, col: u32) -> Result<Value> {
        self.position_request(
            file,
            "textDocument/references",
            line,
            col,
            Some(("context", json!({ "includeDeclaration": true }))),
        )
        .await
    }

    /// `textDocument/hover` for the symbol at `file`:`line`:`col`.
    ///
    /// # Errors
    /// See [`LspManager::definition`].
    pub async fn hover(&self, file: &Path, line: u32, col: u32) -> Result<Value> {
        self.position_request(file, "textDocument/hover", line, col, None)
            .await
    }

    /// `textDocument/rename` for the symbol at `file`:`line`:`col`, returning
    /// the server's `WorkspaceEdit`.
    ///
    /// # Errors
    /// See [`LspManager::definition`].
    pub async fn rename(&self, file: &Path, line: u32, col: u32, new_name: &str) -> Result<Value> {
        self.position_request(
            file,
            "textDocument/rename",
            line,
            col,
            Some(("newName", json!(new_name))),
        )
        .await
    }

    /// `textDocument/documentSymbol` for `file`.
    ///
    /// # Errors
    /// See [`LspManager::definition`].
    pub async fn document_symbol(&self, file: &Path) -> Result<Value> {
        let (abs, tx) = self
            .conn_for_file(file)
            .ok_or_else(|| anyhow!("no language server available for {}", file.display()))?;
        let _ = tx.send(LspCommand::DidOpen { path: abs.clone() }).await;
        let params = json!({ "textDocument": { "uri": path_to_uri(&abs) } });
        request(&tx, "textDocument/documentSymbol", params).await
    }

    /// `workspace/symbol` matching `query`. Uses the first ready server.
    ///
    /// # Errors
    /// When no server is ready or the request times out / errors.
    pub async fn workspace_symbol(&self, query: &str) -> Result<Value> {
        let tx = {
            self.conns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .next()
                .cloned()
        };
        let tx = tx.ok_or_else(|| anyhow!("no language server is ready"))?;
        request(&tx, "workspace/symbol", json!({ "query": query })).await
    }

    /// Returns the current diagnostics for `file` from the aggregated snapshot.
    #[must_use]
    pub fn diagnostics_for(&self, file: &Path) -> Vec<Diagnostic> {
        let ws = self
            .current_ws
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let target_abs = abs_path(ws.as_deref(), file);
        self.snapshot()
            .diagnostics
            .into_iter()
            .filter(|d| abs_path(ws.as_deref(), &d.file) == target_abs)
            .collect()
    }

    /// Notifies the language server that `file` changed on disk (didChange +
    /// didSave) and waits, bounded by `timeout`, for the next diagnostics
    /// snapshot before returning `file`'s fresh diagnostics.
    ///
    /// On timeout (server lag) returns an empty vector so callers can silently
    /// no-op. A no-op (empty) result also occurs when no server serves the file.
    pub async fn refresh_and_wait(&self, file: &Path, timeout: Duration) -> Vec<Diagnostic> {
        let Some((abs, tx)) = self.conn_for_file(file) else {
            return Vec::new();
        };
        let mut rx = self.subscribe();
        // Mark the current snapshot as seen so we only react to a fresh publish.
        rx.borrow_and_update();
        if tx.send(LspCommand::DidChange { path: abs }).await.is_err() {
            return Vec::new();
        }
        // Wait for the first fresh diagnostics snapshot within the budget.
        let fresh = matches!(
            tokio::time::timeout(timeout, rx.changed()).await,
            Ok(Ok(()))
        );
        if fresh {
            self.diagnostics_for(file)
        } else {
            Vec::new()
        }
    }

    /// Issues a position-based request (`definition` / `references` / `hover` /
    /// `rename`) after ensuring the document is open.
    async fn position_request(
        &self,
        file: &Path,
        method: &str,
        line: u32,
        col: u32,
        extra: Option<(&str, Value)>,
    ) -> Result<Value> {
        let (abs, tx) = self
            .conn_for_file(file)
            .ok_or_else(|| anyhow!("no language server available for {}", file.display()))?;
        let _ = tx.send(LspCommand::DidOpen { path: abs.clone() }).await;
        let mut params = json!({
            "textDocument": { "uri": path_to_uri(&abs) },
            "position": {
                "line": line.saturating_sub(1),
                "character": col.saturating_sub(1)
            }
        });
        if let (Some(obj), Some((k, v))) = (params.as_object_mut(), extra) {
            obj.insert(k.to_owned(), v);
        }
        request(&tx, method, params).await
    }

    /// Resolves `file` to `(absolute_path, command_sender)` for the server that
    /// serves its language, or `None` when no such server is ready.
    fn conn_for_file(&self, file: &Path) -> Option<(PathBuf, mpsc::Sender<LspCommand>)> {
        let ws = self
            .current_ws
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let abs = abs_path(ws.as_deref(), file);
        let ext = abs.extension().and_then(|e| e.to_str())?;
        let name = server_name_for_ext(ext)?;
        let tx = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()?;
        Some((abs, tx))
    }
}

/// Sends a request through `tx` and awaits the correlated response, bounded by
/// [`REQUEST_TIMEOUT`].
async fn request(tx: &mpsc::Sender<LspCommand>, method: &str, params: Value) -> Result<Value> {
    let (reply, reply_rx) = oneshot::channel();
    tx.send(LspCommand::Request {
        method: method.to_owned(),
        params,
        reply,
    })
    .await
    .map_err(|_| anyhow!("language server channel closed"))?;
    match tokio::time::timeout(REQUEST_TIMEOUT, reply_rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => bail!("language server dropped the request"),
        Err(_) => bail!("language server request timed out"),
    }
}

/// Resolves `p` against `ws` when relative; returns it unchanged when absolute
/// or when no workspace is known.
fn abs_path(ws: Option<&Path>, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_owned()
    } else if let Some(ws) = ws {
        ws.join(p)
    } else {
        p.to_owned()
    }
}

async fn run_all(workspace: PathBuf, watch_tx: watch::Sender<LspSnapshot>, conns: ConnMap) {
    // Start a server only when its binary is locatable AND the workspace actually
    // uses that language (a project marker is present). resolve_binary checks
    // $PATH first, then ~/.cargo/bin, so rust-analyzer works even when the daemon
    // runs outside a login shell where ~/.cargo/bin is not on $PATH.
    let available: Vec<(&ServerSpec, PathBuf)> = SERVERS
        .iter()
        .filter(|s| workspace_has_marker(&workspace, s))
        .filter_map(|s| resolve_binary(s.binary).map(|p| (s, p)))
        .collect();

    if available.is_empty() {
        tracing::debug!("no LSP servers found on PATH or in ~/.cargo/bin");
        return;
    }

    // Seed watch with "Starting" state for each server.
    let initial_servers = available
        .iter()
        .map(|(s, _)| ServerStatus {
            name: s.name.to_owned(),
            state: ServerState::Starting,
        })
        .collect();
    let _ = watch_tx.send(LspSnapshot {
        servers: initial_servers,
        diagnostics: Vec::new(),
    });

    let (event_tx, event_rx) = mpsc::channel::<ServerEvent>(EVENT_CHANNEL_CAP);

    // Spawn one task per server; each task restarts up to MAX_RESTART_ATTEMPTS times.
    for (spec, binary_path) in &available {
        let name = spec.name.to_owned();
        let binary = binary_path.to_string_lossy().into_owned();
        let args: Vec<String> = spec.args.iter().map(ToString::to_string).collect();
        let ws = workspace.clone();
        let etx = event_tx.clone();
        let conns = Arc::clone(&conns);

        tokio::spawn(async move {
            run_server_with_restart(name, &binary, &args, &ws, etx, conns).await;
        });
    }

    // Drop our copy so the channel closes when all server tasks exit.
    drop(event_tx);

    // Run the aggregator on this task.
    aggregate(event_rx, watch_tx).await;
}

/// Delay (seconds) between recovery-probe attempts after the restart cap is hit.
const RECOVERY_PROBE_SECS: u64 = 300; // 5 minutes

/// Runs a server task and restarts it up to `MAX_RESTART_ATTEMPTS` times with
/// exponential backoff when it exits unexpectedly.
///
/// After exhausting all restart attempts, the task enters a recovery-probe loop:
/// it waits 5 minutes, resets the attempt counter, and tries to restart the
/// server again. This handles the case where a transient system condition (OOM,
/// missing binary on a newly-mounted volume) clears itself over time.
async fn run_server_with_restart(
    name: String,
    binary: &str,
    args: &[String],
    workspace: &Path,
    event_tx: mpsc::Sender<ServerEvent>,
    conns: ConnMap,
) {
    loop {
        // Inner restart loop: attempt up to MAX_RESTART_ATTEMPTS restarts.
        for attempt in 0..=MAX_RESTART_ATTEMPTS {
            if attempt > 0 {
                let delay = RESTART_DELAYS_SECS[(attempt - 1) as usize];
                warn!(server = %name, attempt, delay_secs = delay, "LSP server restarting");
                let _ = event_tx
                    .send(ServerEvent::Starting { name: name.clone() })
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
            run_server(&name, binary, args, workspace, event_tx.clone(), &conns).await;
            if event_tx.is_closed() {
                return;
            }
        }

        // Restart cap exhausted — schedule a recovery probe.
        tracing::error!(
            server = %name,
            "LSP server restart cap reached; scheduling {RECOVERY_PROBE_SECS}s recovery probe"
        );
        let _ = event_tx
            .send(ServerEvent::Degraded {
                name: name.clone(),
                reason: "restart cap reached; will retry in 5 minutes".to_owned(),
            })
            .await;

        tokio::time::sleep(std::time::Duration::from_secs(RECOVERY_PROBE_SECS)).await;

        if event_tx.is_closed() {
            return;
        }

        tracing::info!(server = %name, "LSP server recovery probe: attempting restart");
        // Loop back to the inner restart loop with a fresh attempt counter.
    }
}

/// Runs a single server lifecycle: spawn → handshake → notification loop.
async fn run_server(
    name: &str,
    binary: &str,
    args: &[String],
    workspace: &Path,
    event_tx: mpsc::Sender<ServerEvent>,
    conns: &ConnMap,
) {
    let arg_slices: Vec<&str> = args.iter().map(String::as_str).collect();
    let client = match LspClient::spawn_and_init(binary, &arg_slices, workspace).await {
        Ok(c) => c,
        Err(e) => {
            warn!(server = %name, error = %e, "LSP spawn/handshake failed");
            let _ = event_tx
                .send(ServerEvent::Degraded {
                    name: name.to_owned(),
                    reason: e.to_string(),
                })
                .await;
            return;
        }
    };

    info!(server = %name, "LSP ready");
    let _ = event_tx
        .send(ServerEvent::Ready {
            name: name.to_owned(),
        })
        .await;

    let (diag_tx, mut diag_rx) = mpsc::channel::<Vec<Diagnostic>>(EVENT_CHANNEL_CAP);

    // Forward diagnostics from the inner channel to the event bus.
    let etx2 = event_tx.clone();
    let name2 = name.to_owned();
    tokio::spawn(async move {
        while let Some(diags) = diag_rx.recv().await {
            let _ = etx2
                .send(ServerEvent::Diagnostics {
                    name: name2.clone(),
                    diags,
                })
                .await;
        }
    });

    // Register this server's command channel so requests can address it, then
    // run the I/O loop (blocks until the server exits or errors).
    let (cmd_tx, cmd_rx) = mpsc::channel::<LspCommand>(CMD_CHANNEL_CAP);
    conns
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_owned(), cmd_tx);

    let run_result = client.run(diag_tx, cmd_rx).await;

    // Deregister on any exit path so stale channels are never addressed.
    conns
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(name);

    if let Err(e) = run_result {
        warn!(server = %name, error = %e, "LSP server disconnected");
        let _ = event_tx
            .send(ServerEvent::Degraded {
                name: name.to_owned(),
                reason: e.to_string(),
            })
            .await;
    }
}

/// Receives `ServerEvent`s and pushes updated `LspSnapshot` through `watch_tx`.
async fn aggregate(mut rx: mpsc::Receiver<ServerEvent>, watch_tx: watch::Sender<LspSnapshot>) {
    let mut states: HashMap<String, ServerState> = HashMap::new();
    let mut diag_map: HashMap<String, Vec<Diagnostic>> = HashMap::new();

    while let Some(event) = rx.recv().await {
        match event {
            ServerEvent::Starting { name } => {
                states.insert(name, ServerState::Starting);
            }
            ServerEvent::Ready { name } => {
                states.insert(name, ServerState::Ready);
            }
            ServerEvent::Degraded { name, reason } => {
                states.insert(name, ServerState::Degraded(reason));
            }
            ServerEvent::Diagnostics { name, diags } => {
                diag_map.insert(name, diags);
            }
        }

        let _ = watch_tx.send(build_snapshot(&states, &diag_map));
    }
}

fn build_snapshot(
    states: &HashMap<String, ServerState>,
    diag_map: &HashMap<String, Vec<Diagnostic>>,
) -> LspSnapshot {
    // Stable server order: follow SERVERS declaration order.
    let servers = SERVERS
        .iter()
        .filter_map(|spec| {
            states.get(spec.name).map(|state| ServerStatus {
                name: spec.name.to_owned(),
                state: state.clone(),
            })
        })
        .collect();

    let mut diagnostics: Vec<Diagnostic> = diag_map.values().flatten().cloned().collect();
    diagnostics.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
    });

    LspSnapshot {
        servers,
        diagnostics,
    }
}

#[cfg(test)]
impl LspManager {
    /// Points the manager at `ws` without starting any servers.
    fn set_workspace_for_test(&self, ws: PathBuf) {
        *self
            .current_ws
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ws);
    }

    /// Publishes `snap` through the watch channel.
    fn push_snapshot_for_test(&self, snap: LspSnapshot) {
        let _ = self.tx.send(snap);
    }

    /// Spawns `binary` as a managed server under `name`, wiring its diagnostics
    /// into the snapshot and registering its command channel — the same wiring
    /// [`run_server`] performs, minus restart supervision.
    async fn connect_mock_for_test(&self, name: &str, binary: &Path, workspace: &Path) {
        let client = LspClient::spawn_and_init(binary.to_str().unwrap(), &[], workspace)
            .await
            .expect("spawn mock server");
        let (diag_tx, mut diag_rx) = mpsc::channel::<Vec<Diagnostic>>(64);
        let watch_tx = self.tx.clone();
        let nm = name.to_owned();
        tokio::spawn(async move {
            while let Some(diags) = diag_rx.recv().await {
                let _ = watch_tx.send(LspSnapshot {
                    servers: vec![ServerStatus {
                        name: nm.clone(),
                        state: ServerState::Ready,
                    }],
                    diagnostics: diags,
                });
            }
        });
        let (cmd_tx, cmd_rx) = mpsc::channel::<LspCommand>(64);
        self.conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), cmd_tx);
        tokio::spawn(async move {
            let _ = client.run(diag_tx, cmd_rx).await;
        });
    }
}

#[cfg(test)]
mod request_tests {
    use super::{server_name_for_ext, LspManager};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use crate::types::{Diagnostic, LspSnapshot, Severity};

    fn mock_server_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let profile_dir = exe.parent()?.parent()?;
        let candidate = profile_dir.join("examples").join("mock_lsp");
        candidate.exists().then_some(candidate)
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "smedja-lsp-mgr-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ext_maps_to_expected_server() {
        assert_eq!(server_name_for_ext("rs"), Some("rust-analyzer"));
        assert_eq!(server_name_for_ext("py"), Some("pyright"));
        assert_eq!(
            server_name_for_ext("tsx"),
            Some("typescript-language-server")
        );
        assert_eq!(server_name_for_ext("cpp"), Some("clangd"));
        assert_eq!(server_name_for_ext("txt"), None);
    }

    #[test]
    fn diagnostics_for_filters_by_file() {
        let mgr = LspManager::new();
        mgr.set_workspace_for_test(PathBuf::from("/proj"));
        mgr.push_snapshot_for_test(LspSnapshot {
            servers: Vec::new(),
            diagnostics: vec![
                Diagnostic {
                    file: PathBuf::from("src/a.rs"),
                    line: 1,
                    col: 1,
                    severity: Severity::Error,
                    code: None,
                    message: "a".to_owned(),
                },
                Diagnostic {
                    file: PathBuf::from("src/b.rs"),
                    line: 2,
                    col: 1,
                    severity: Severity::Warning,
                    code: None,
                    message: "b".to_owned(),
                },
            ],
        });
        // Relative and absolute forms both resolve to the same file.
        let by_rel = mgr.diagnostics_for(Path::new("src/a.rs"));
        assert_eq!(by_rel.len(), 1);
        assert_eq!(by_rel[0].message, "a");
        let by_abs = mgr.diagnostics_for(Path::new("/proj/src/b.rs"));
        assert_eq!(by_abs.len(), 1);
        assert_eq!(by_abs[0].message, "b");
    }

    #[tokio::test]
    async fn definition_through_manager_api() {
        let Some(server) = mock_server_path() else {
            eprintln!("mock_lsp example not built; skipping");
            return;
        };
        let ws = scratch_dir("def");
        std::fs::write(ws.join("m.rs"), "fn main() {}\n").unwrap();
        let mgr = LspManager::new();
        mgr.set_workspace_for_test(ws.clone());
        mgr.connect_mock_for_test("rust-analyzer", &server, &ws)
            .await;

        let res = mgr
            .definition(Path::new("m.rs"), 1, 1)
            .await
            .expect("definition ok");
        assert!(res.get("uri").is_some());

        mgr.shutdown();
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn refresh_and_wait_returns_planted_diagnostic() {
        let Some(server) = mock_server_path() else {
            eprintln!("mock_lsp example not built; skipping");
            return;
        };
        let ws = scratch_dir("refresh");
        let file = ws.join("m.rs");
        std::fs::write(&file, "fn main() { let x = 1; }\n").unwrap();
        let mgr = LspManager::new();
        mgr.set_workspace_for_test(ws.clone());
        mgr.connect_mock_for_test("rust-analyzer", &server, &ws)
            .await;

        let diags = mgr
            .refresh_and_wait(Path::new("m.rs"), Duration::from_secs(5))
            .await;
        assert_eq!(diags.len(), 1, "expected the planted diagnostic");
        assert_eq!(diags[0].message, "planted error");
        assert_eq!(diags[0].severity, Severity::Error);

        mgr.shutdown();
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn request_times_out_when_no_server() {
        let mgr = LspManager::new();
        mgr.set_workspace_for_test(PathBuf::from("/proj"));
        // No conn registered for .rs → immediate error, not a hang.
        let err = mgr.definition(Path::new("a.rs"), 1, 1).await.unwrap_err();
        assert!(err.to_string().contains("no language server"));
    }
}

#[cfg(test)]
mod marker_tests {
    use super::{resolve_binary, workspace_has_marker, SERVERS};

    #[test]
    fn resolve_binary_finds_shell_on_path() {
        // `sh` is always on $PATH — resolve_binary must find it.
        assert!(resolve_binary("sh").is_some());
    }

    #[test]
    fn resolve_binary_returns_none_for_nonexistent() {
        assert!(resolve_binary("__smedja_nonexistent_binary_xyz__").is_none());
    }

    #[test]
    fn resolve_binary_path_is_file() {
        let p = resolve_binary("sh").expect("sh must resolve");
        assert!(
            p.is_file(),
            "resolved path must be a real file: {}",
            p.display()
        );
    }

    #[test]
    fn marker_gating_starts_only_relevant_servers() {
        let dir = std::env::temp_dir().join(format!(
            "smedja-lsp-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), b"[package]\n").unwrap();

        let spec = |name: &str| SERVERS.iter().find(|s| s.name == name).unwrap();
        // A Rust project starts rust-analyzer but NOT clangd / gopls / tsserver.
        assert!(workspace_has_marker(&dir, spec("rust-analyzer")));
        assert!(!workspace_has_marker(&dir, spec("clangd")));
        assert!(!workspace_has_marker(&dir, spec("gopls")));
        assert!(!workspace_has_marker(
            &dir,
            spec("typescript-language-server")
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
