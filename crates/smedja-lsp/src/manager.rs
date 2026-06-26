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
/// Only servers whose binary is on `$PATH` are started.
struct ServerSpec {
    name: &'static str,
    binary: &'static str,
    args: &'static [&'static str],
}

const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "rust-analyzer",
        binary: "rust-analyzer",
        args: &[],
    },
    ServerSpec {
        name: "pyright",
        binary: "pyright-langserver",
        args: &["--stdio"],
    },
    ServerSpec {
        name: "gopls",
        binary: "gopls",
        args: &[],
    },
    ServerSpec {
        name: "typescript-language-server",
        binary: "typescript-language-server",
        args: &["--stdio"],
    },
    ServerSpec {
        name: "clangd",
        binary: "clangd",
        args: &[],
    },
];

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
    let available: Vec<&ServerSpec> = SERVERS.iter().filter(|s| which(s.binary).is_ok()).collect();

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

/// Runs a server task and restarts it up to `MAX_RESTART_ATTEMPTS` times with
/// exponential backoff when it exits unexpectedly.
async fn run_server_with_restart(
    name: String,
    binary: &str,
    args: &[String],
    workspace: &Path,
    event_tx: mpsc::Sender<ServerEvent>,
) {
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
            break;
        }
    }
    warn!(server = %name, "LSP server gave up after all restart attempts; staying degraded");
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
