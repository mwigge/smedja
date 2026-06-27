//! `LspManager` — detects available language servers, starts them, aggregates
//! diagnostics from all active servers, and exposes a `watch` channel so
//! consumers get push updates with zero polling overhead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

/// Maximum restart attempts per language server after an unexpected exit.
const MAX_RESTART_ATTEMPTS: u32 = 3;
/// Delay (seconds) before each restart attempt: 1 s, 5 s, 30 s.
const RESTART_DELAYS_SECS: &[u64] = &[1, 5, 30];
/// Bounded channel capacity for server events and diagnostic relay.
const EVENT_CHANNEL_CAP: usize = 512;
use which::which;

use crate::client::LspClient;
use crate::types::{Diagnostic, LspSnapshot, ServerState, ServerStatus};

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
}

const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "rust-analyzer",
        binary: "rust-analyzer",
        args: &[],
        markers: &["Cargo.toml"],
    },
    ServerSpec {
        name: "pyright",
        binary: "pyright-langserver",
        args: &["--stdio"],
        markers: &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"],
    },
    ServerSpec {
        name: "gopls",
        binary: "gopls",
        args: &[],
        markers: &["go.mod", "go.work"],
    },
    ServerSpec {
        name: "typescript-language-server",
        binary: "typescript-language-server",
        args: &["--stdio"],
        markers: &["package.json", "tsconfig.json"],
    },
    ServerSpec {
        name: "clangd",
        binary: "clangd",
        args: &[],
        markers: &["compile_commands.json", "CMakeLists.txt", "Makefile", ".clangd"],
    },
];

/// True when `workspace` contains at least one of `spec`'s project markers.
fn workspace_has_marker(workspace: &std::path::Path, spec: &ServerSpec) -> bool {
    spec.markers.iter().any(|m| workspace.join(m).exists())
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
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            run_all(workspace, tx).await;
        });
        *self.runner.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Aborts the background server manager and all child processes.
    /// Child processes are killed because `LspClient` uses `kill_on_drop(true)`.
    pub fn shutdown(&self) {
        if let Some(h) = self.runner.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
    }
}

async fn run_all(workspace: PathBuf, watch_tx: watch::Sender<LspSnapshot>) {
    // Start a server only when its binary exists AND the workspace actually uses
    // that language (a project marker is present) — so e.g. clangd does not run
    // on a Rust repo just because clangd is installed.
    let available: Vec<&ServerSpec> = SERVERS
        .iter()
        .filter(|s| which(s.binary).is_ok() && workspace_has_marker(&workspace, s))
        .collect();

    if available.is_empty() {
        tracing::debug!("no LSP servers found on PATH");
        return;
    }

    // Seed watch with "Starting" state for each server.
    let initial_servers = available
        .iter()
        .map(|s| ServerStatus {
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
    for spec in &available {
        let name = spec.name.to_owned();
        let binary = spec.binary.to_owned();
        let args: Vec<String> = spec.args.iter().map(|s| s.to_string()).collect();
        let ws = workspace.clone();
        let etx = event_tx.clone();

        tokio::spawn(async move {
            run_server_with_restart(name, &binary, &args, &ws, etx).await;
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
            run_server(&name, binary, args, workspace, event_tx.clone()).await;
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
) {
    let arg_slices: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut client = match LspClient::spawn_and_init(binary, &arg_slices, workspace).await {
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

    // Run the LSP notification loop (blocks until server exits or errors).
    if let Err(e) = client.run(diag_tx).await {
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
mod marker_tests {
    use super::{workspace_has_marker, SERVERS};

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
        assert!(!workspace_has_marker(&dir, spec("typescript-language-server")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
