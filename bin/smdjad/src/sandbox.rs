//! Cross-platform sandbox executor for tool isolation.
//!
//! Shell/tool execution (`bash`, `run_command`) is confined behind a
//! [`SandboxBackend`] selected by capability detection:
//!
//! - [`DockerBackend`] — opt-in (`SMEDJA_TOOL_SANDBOX=docker` or
//!   `SMEDJA_SANDBOX_MODE`/Docker reachable); strongest isolation.
//! - [`SeatbeltBackend`] — macOS `sandbox-exec`; zero-config default on macOS.
//! - [`LandlockBackend`] — Linux Landlock LSM; zero-config default on Linux.
//!
//! Selection precedence: Docker (when opted in and reachable) → the current
//! platform's OS-native backend → none. Read-only tools (`read_file`,
//! `list_files`, `graph_query`) bypass the sandbox entirely.
//!
//! The writable filesystem root is the *confined root* — the active worktree
//! when a task owns one, otherwise the session workspace — with `.git`
//! read-only and an ephemeral `/tmp`. A declarative [`NetworkPolicy`] governs
//! egress and shares the daemon's `is_blocked_ip` SSRF floor. A
//! [`SandboxMode`] governs the fallback when no backend is available.
//!
//! The implementation is split across focused submodules:
//! - [`types`] — [`NetworkPolicy`], [`SandboxMode`], [`SandboxTelemetry`].
//! - [`paths`] — the read allow-list and confined-root resolution.
//! - [`backend`] — the [`SandboxBackend`] trait and backend selection.
//! - [`executor`] — the [`SandboxExecutor`] dispatcher and fallback contract.

mod docker;
#[cfg(target_os = "linux")]
mod landlock_backend;
#[cfg(target_os = "macos")]
mod seatbelt;

mod backend;
mod executor;
mod paths;
mod types;

pub use docker::DockerBackend;
#[cfg(target_os = "linux")]
pub use landlock_backend::LandlockBackend;
#[cfg(target_os = "macos")]
pub use seatbelt::SeatbeltBackend;

pub use backend::SandboxBackend;
pub use executor::{SandboxExecutor, UNCONFINED_MARKER};
pub use types::{NetworkPolicy, SandboxMode, SandboxTelemetry};

// Re-exported for the backend submodules (`super::…`), which share the
// confined-root contract and read allow-list resolution.
pub(crate) use paths::{resolve_confined_root, resolve_read_paths};

/// Per-command execution timeout for every backend.
const EXEC_TIMEOUT_SECS: u64 = 30;
