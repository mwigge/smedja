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

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use which::which;

mod docker;
#[cfg(target_os = "linux")]
mod landlock_backend;
#[cfg(target_os = "macos")]
mod seatbelt;

pub use docker::DockerBackend;
#[cfg(target_os = "linux")]
pub use landlock_backend::LandlockBackend;
#[cfg(target_os = "macos")]
pub use seatbelt::SeatbeltBackend;

/// Tools exempt from sandboxing (read-only; no side-effects).
const EXEMPT_TOOLS: &[&str] = &["read_file", "list_files", "graph_query"];

/// Per-command execution timeout for every backend.
const EXEC_TIMEOUT_SECS: u64 = 30;

/// Marker prefixed to a tool result that ran on the host without confinement.
pub const UNCONFINED_MARKER: &str = "[unconfined] sandbox unavailable; command ran on the host\n";

/// System directories a sandboxed command may *read* from by default.
///
/// This is the read allow-list floor: the directories a shell and common tools
/// need to load (binaries and shared libraries) and resolve basic system
/// configuration. It deliberately excludes the user's home directory and its
/// secret subpaths (`~/.ssh`, `~/.aws`, `~/.config`, `~/.gnupg`) so a sandboxed
/// command cannot read host credentials. The macOS-only entries
/// (`/System`, `/Library`, `/private/var/db/dyld`) cover the dyld shared cache
/// the Seatbelt backend needs. Operators widen the list via
/// `SMEDJA_SANDBOX_READ_PATHS`; they never shrink it.
pub(crate) const DEFAULT_READ_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/opt",
    #[cfg(target_os = "macos")]
    "/System",
    #[cfg(target_os = "macos")]
    "/Library",
    #[cfg(target_os = "macos")]
    "/private/var/db/dyld",
];

/// Resolves the read allow-list for sandboxed commands.
///
/// Starts from [`DEFAULT_READ_PATHS`] and *appends* the colon-separated paths in
/// `SMEDJA_SANDBOX_READ_PATHS` (operators widen, never replace). Paths that do
/// not exist on the host are skipped so a missing default (for example
/// `/lib64` on macOS) is not an error. Backends share this one source of truth
/// so the read floor is identical across Landlock and Seatbelt.
#[must_use]
pub(crate) fn resolve_read_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = DEFAULT_READ_PATHS.iter().map(PathBuf::from).collect();

    if let Ok(extra) = std::env::var("SMEDJA_SANDBOX_READ_PATHS") {
        for entry in extra.split(':') {
            let entry = entry.trim();
            if !entry.is_empty() {
                paths.push(PathBuf::from(entry));
            }
        }
    }

    // Skip paths that do not exist on this host (no error); backends that open
    // an fd per path would otherwise fail on an absent default.
    paths.retain(|p| p.exists());
    paths
}

/// Structured attributes for the `smedja.sandbox.exec` span and the
/// `smedja.sandbox.unconfined` event. Built once per sandboxed execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxTelemetry {
    /// Selected backend name (`docker`/`seatbelt`/`landlock`/`none`).
    pub backend: &'static str,
    /// Active network policy.
    pub network_policy: &'static str,
    /// Active fallback mode.
    pub mode: &'static str,
    /// The confined writable root for this execution.
    pub confined_root: String,
    /// `true` when the active backend confines the command's filesystem reads
    /// to the system-dir allow-list plus the confined root.
    pub read_confined: bool,
    /// `true` when the active backend denies the command all network egress
    /// (network policy `none` with an available backend).
    pub net_confined: bool,
}

/// Declarative network policy for sandboxed commands.
///
/// Parsed from `SMEDJA_SANDBOX_NETWORK` (default [`NetworkPolicy::None`]). The
/// `allowlist`/`open` policies share the daemon's `is_blocked_ip` predicate as
/// the egress floor, so private/loopback/IMDS ranges stay blocked under every
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No egress at all.
    None,
    /// Egress only to destinations not rejected by `is_blocked_ip`.
    Allowlist,
    /// General egress, but `is_blocked_ip` ranges stay blocked.
    Open,
}

impl NetworkPolicy {
    /// Parses the policy from `SMEDJA_SANDBOX_NETWORK`, defaulting to
    /// [`NetworkPolicy::None`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_str_value(&std::env::var("SMEDJA_SANDBOX_NETWORK").unwrap_or_default())
    }

    /// Parses the policy from a raw string value (case-insensitive).
    #[must_use]
    pub fn from_str_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "allowlist" => Self::Allowlist,
            "open" => Self::Open,
            _ => Self::None,
        }
    }

    /// Returns the canonical string form used in telemetry and status output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Allowlist => "allowlist",
            Self::Open => "open",
        }
    }

    /// Returns `true` when egress to a *publicly routable* destination is
    /// permitted by this policy. The `is_blocked_ip` floor is applied
    /// separately by [`NetworkPolicy::permits_dest`].
    #[must_use]
    pub fn permits_public_egress(self) -> bool {
        matches!(self, Self::Allowlist | Self::Open)
    }

    /// Returns `true` when egress to `addr` is permitted under this policy.
    ///
    /// `None` denies everything. `Allowlist`/`Open` permit only destinations
    /// not rejected by the shared `is_blocked_ip` SSRF floor, so the sandbox
    /// and the SSRF guard share one source of truth.
    #[must_use]
    pub fn permits_dest(self, addr: std::net::IpAddr) -> bool {
        match self {
            Self::None => false,
            Self::Allowlist | Self::Open => !crate::is_blocked_ip(addr),
        }
    }
}

/// Fallback behaviour when no isolation backend is available.
///
/// Parsed from `SMEDJA_SANDBOX_MODE` (default [`SandboxMode::Auto`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Best-effort: fall back to host execution with an unconfined marker.
    Auto,
    /// Fail closed: error if no backend is available; never run unconfined.
    Required,
    /// Skip the sandbox entirely.
    Off,
}

impl SandboxMode {
    /// Parses the mode from `SMEDJA_SANDBOX_MODE`, defaulting to
    /// [`SandboxMode::Auto`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_str_value(&std::env::var("SMEDJA_SANDBOX_MODE").unwrap_or_default())
    }

    /// Parses the mode from a raw string value (case-insensitive).
    #[must_use]
    pub fn from_str_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => Self::Required,
            "off" => Self::Off,
            _ => Self::Auto,
        }
    }

    /// Returns the canonical string form used in telemetry and status output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
}

/// A platform isolation mechanism that can confine a shell command.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Stable identifier reported in telemetry and `smj sandbox status`.
    fn name(&self) -> &'static str;

    /// Returns `true` when this backend can actually confine a command on the
    /// current host (binary present, kernel support, image built, …).
    fn available(&self) -> bool;

    /// Executes `cmd` confined to `confined_root` under `policy`.
    ///
    /// Returns the combined stdout on success.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a diagnostic string when the confined root is
    /// invalid, the command times out, or the command exits non-zero.
    async fn exec(
        &self,
        cmd: &str,
        confined_root: &Path,
        policy: NetworkPolicy,
    ) -> Result<String, String>;
}

/// Selects the active backend from the available implementations.
///
/// Precedence: Docker when opted in and reachable, else the current platform's
/// OS-native backend (when available), else `None`. `docker_opt_in` reflects
/// whether the operator pinned Docker (legacy `SMEDJA_TOOL_SANDBOX=docker` or
/// an explicit selection).
fn select_backend(
    docker_opt_in: bool,
    docker: Box<dyn SandboxBackend>,
    native: Option<Box<dyn SandboxBackend>>,
) -> Option<Box<dyn SandboxBackend>> {
    if docker_opt_in && docker.available() {
        return Some(docker);
    }
    match native {
        Some(n) if n.available() => Some(n),
        _ => None,
    }
}

/// Constructs the OS-native backend for the current platform, if any.
// The return type is `Option` because the unsupported-platform arm yields
// `None`; per-platform cfg makes some arms always-`Some`, which clippy can't see.
#[cfg_attr(
    any(target_os = "macos", target_os = "linux"),
    allow(clippy::unnecessary_wraps)
)]
fn native_backend() -> Option<Box<dyn SandboxBackend>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(SeatbeltBackend::detect()))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Box::new(LandlockBackend::detect()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Dispatches shell/tool execution to the selected [`SandboxBackend`].
pub struct SandboxExecutor {
    backend: Option<Box<dyn SandboxBackend>>,
    mode: SandboxMode,
    network: NetworkPolicy,
}

impl SandboxExecutor {
    /// Creates an executor, selecting the backend by capability detection.
    ///
    /// The legacy `SMEDJA_TOOL_SANDBOX=docker` alias pins Docker and sets mode
    /// `Auto`. Otherwise the mode comes from `SMEDJA_SANDBOX_MODE` and the
    /// network policy from `SMEDJA_SANDBOX_NETWORK`.
    ///
    /// # Note
    ///
    /// Capability detection (`new()`) runs once at daemon startup, before the
    /// Tokio runtime is accepting work, so brief blocking probes (`which`,
    /// `docker image inspect`) are acceptable here.
    #[must_use]
    pub fn new() -> Self {
        let legacy_docker = std::env::var("SMEDJA_TOOL_SANDBOX").is_ok_and(|v| v == "docker");
        let mode = if legacy_docker {
            SandboxMode::Auto
        } else {
            SandboxMode::from_env()
        };
        let network = NetworkPolicy::from_env();

        if matches!(mode, SandboxMode::Off) {
            return Self {
                backend: None,
                mode,
                network,
            };
        }

        // Docker is opted into via the legacy alias, or implicitly considered
        // when its daemon is reachable on PATH.
        let docker_opt_in = legacy_docker || which("docker").is_ok();
        let docker: Box<dyn SandboxBackend> = Box::new(DockerBackend::detect());
        let backend = select_backend(docker_opt_in, docker, native_backend());

        Self {
            backend,
            mode,
            network,
        }
    }

    /// Returns `true` when a usable backend was selected.
    #[must_use]
    pub fn available(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.available())
    }

    /// Returns the selected backend's name, or `"none"` when none is available.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.backend.as_ref().map_or("none", |b| b.name())
    }

    /// Returns the active network policy.
    #[must_use]
    pub fn network_policy(&self) -> NetworkPolicy {
        self.network
    }

    /// Returns the active fallback mode.
    #[must_use]
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// Returns `true` if `tool_name` is exempt from sandboxing.
    #[must_use]
    pub fn is_exempt(tool_name: &str) -> bool {
        EXEMPT_TOOLS.contains(&tool_name)
    }

    /// Executes `cmd` confined to `confined_root` via the selected backend.
    ///
    /// # Errors
    ///
    /// Returns `Err` when no backend is available or the backend fails.
    pub async fn exec(&self, cmd: &str, confined_root: &Path) -> Result<String, String> {
        match self.backend.as_ref() {
            Some(b) => b.exec(cmd, confined_root, self.network).await,
            None => Err("sandbox not available".into()),
        }
    }

    /// Builds the structured telemetry attributes for an execution rooted at
    /// `confined_root`.
    #[must_use]
    pub fn telemetry(&self, confined_root: &Path) -> SandboxTelemetry {
        // Read confinement applies whenever a backend is active (every backend
        // tightens reads to the allow-list / structural floor). Network
        // confinement applies only under policy `none`, the sole policy that
        // denies the subprocess all egress; `allowlist`/`open` retain host
        // network (open-minus-blocked-ranges for the subprocess).
        let active = self.available();
        let read_confined = active;
        let net_confined = active && matches!(self.network, NetworkPolicy::None);

        SandboxTelemetry {
            backend: self.backend_name(),
            network_policy: self.network.as_str(),
            mode: self.mode.as_str(),
            confined_root: confined_root.display().to_string(),
            read_confined,
            net_confined,
        }
    }

    /// Runs `cmd` confined to `confined_root`, applying the fallback contract
    /// and emitting the `smedja.sandbox.exec` span and (on host fallback) the
    /// `smedja.sandbox.unconfined` event. `host_run` is the unconfined host
    /// runner used by the `Auto` fallback.
    ///
    /// Returns the tool-result string (an `error:`-form string under
    /// `Required` with no backend; an [`UNCONFINED_MARKER`]-prefixed result
    /// under `Auto` with no backend; the backend output otherwise).
    pub async fn run_confined<F, Fut>(&self, cmd: &str, confined_root: &Path, host_run: F) -> String
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = String>,
    {
        let tel = self.telemetry(confined_root);
        let span = tracing::info_span!(
            "smedja.sandbox.exec",
            backend = tel.backend,
            network_policy = tel.network_policy,
            mode = tel.mode,
            confined_root = %tel.confined_root,
            read_confined = tel.read_confined,
            net_confined = tel.net_confined,
        );
        let _enter = span.enter();

        if self.available() {
            return match self.exec(cmd, confined_root).await {
                Ok(out) => out,
                Err(e) => format!("error: {e}"),
            };
        }

        // No backend available: apply the mode-driven fallback.
        match self.mode {
            SandboxMode::Off => host_run().await,
            SandboxMode::Required => {
                format!(
                    "error: sandbox required but no isolation backend is available \
                     (mode=required, network={}); install Docker or enable a supported \
                     OS-native backend, or set SMEDJA_SANDBOX_MODE=auto",
                    tel.network_policy
                )
            }
            SandboxMode::Auto => {
                tracing::info!(
                    backend = tel.backend,
                    network_policy = tel.network_policy,
                    mode = tel.mode,
                    confined_root = %tel.confined_root,
                    reason = "no isolation backend available",
                    "smedja.sandbox.unconfined"
                );
                let out = host_run().await;
                format!("{UNCONFINED_MARKER}{out}")
            }
        }
    }
}

impl Default for SandboxExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonicalises `confined_root` and resolves the writable subtree, the
/// read-only `.git` path (when present), and the path string used for mounts.
///
/// Shared by the backends so they agree on the confined-root contract.
///
/// # Errors
///
/// Returns `Err` when the root cannot be canonicalised, is outside an
/// `SMEDJA_WORKSPACE_ROOT` (when set), or contains non-UTF-8 bytes.
pub(crate) fn resolve_confined_root(
    confined_root: &Path,
) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>), String> {
    let root = confined_root
        .canonicalize()
        .map_err(|e| format!("invalid confined root: {e}"))?;

    if let Ok(allowed) = std::env::var("SMEDJA_WORKSPACE_ROOT") {
        let allowed = std::path::PathBuf::from(allowed);
        if !root.starts_with(&allowed) {
            return Err(format!(
                "confined root {} is outside allowed root {}",
                root.display(),
                allowed.display()
            ));
        }
    }

    let git_dir = root.join(".git");
    let git = if git_dir.exists() {
        Some(git_dir)
    } else {
        None
    };
    Ok((root, git))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub backend that records its inputs and returns a canned string.
    struct StubBackend {
        name: &'static str,
        avail: bool,
    }

    #[async_trait]
    impl SandboxBackend for StubBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        fn available(&self) -> bool {
            self.avail
        }
        async fn exec(
            &self,
            cmd: &str,
            confined_root: &Path,
            policy: NetworkPolicy,
        ) -> Result<String, String> {
            Ok(format!(
                "stub:{}:{}:{}",
                cmd,
                confined_root.display(),
                policy.as_str()
            ))
        }
    }

    // ── 1.1 trait dispatch ────────────────────────────────────────────────────

    #[tokio::test]
    async fn backend_trait_dispatches_to_selected_impl() {
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Auto,
            network: NetworkPolicy::None,
        };
        assert!(ex.available());
        assert_eq!(ex.backend_name(), "stub");
        let out = ex.exec("echo hi", Path::new("/tmp")).await.unwrap();
        assert!(out.starts_with("stub:echo hi:"), "got: {out}");
        assert!(
            out.ends_with(":none"),
            "policy must be threaded; got: {out}"
        );
    }

    // ── 1.2 selection precedence ──────────────────────────────────────────────

    #[test]
    fn selection_prefers_docker_then_native_then_none() {
        let docker_avail = || -> Box<dyn SandboxBackend> {
            Box::new(StubBackend {
                name: "docker",
                avail: true,
            })
        };
        let docker_unavail = || -> Box<dyn SandboxBackend> {
            Box::new(StubBackend {
                name: "docker",
                avail: false,
            })
        };
        let native_avail = || -> Box<dyn SandboxBackend> {
            Box::new(StubBackend {
                name: "native",
                avail: true,
            })
        };

        // Docker opted in and available → Docker wins.
        let sel = select_backend(true, docker_avail(), Some(native_avail()));
        assert_eq!(sel.unwrap().name(), "docker");

        // Docker opted in but unavailable → native wins.
        let sel = select_backend(true, docker_unavail(), Some(native_avail()));
        assert_eq!(sel.unwrap().name(), "native");

        // Docker not opted in → native wins even if docker is available.
        let sel = select_backend(false, docker_avail(), Some(native_avail()));
        assert_eq!(sel.unwrap().name(), "native");

        // No native available → none.
        let sel = select_backend(true, docker_unavail(), None);
        assert!(sel.is_none());
    }

    // ── 1.4 env parsing ───────────────────────────────────────────────────────

    #[test]
    fn network_policy_parses_from_env_default_none() {
        assert_eq!(
            NetworkPolicy::from_str_value("allowlist"),
            NetworkPolicy::Allowlist
        );
        assert_eq!(NetworkPolicy::from_str_value("open"), NetworkPolicy::Open);
        assert_eq!(NetworkPolicy::from_str_value(""), NetworkPolicy::None);
        assert_eq!(
            NetworkPolicy::from_str_value("garbage"),
            NetworkPolicy::None
        );
    }

    #[test]
    fn sandbox_mode_parses_from_env_default_auto() {
        assert_eq!(
            SandboxMode::from_str_value("required"),
            SandboxMode::Required
        );
        assert_eq!(SandboxMode::from_str_value("off"), SandboxMode::Off);
        assert_eq!(SandboxMode::from_str_value(""), SandboxMode::Auto);
        assert_eq!(SandboxMode::from_str_value("garbage"), SandboxMode::Auto);
    }

    #[test]
    fn read_file_is_exempt() {
        assert!(SandboxExecutor::is_exempt("read_file"));
    }

    #[test]
    fn bash_is_not_exempt() {
        assert!(!SandboxExecutor::is_exempt("bash"));
    }

    #[test]
    fn mcp_call_is_not_exempt() {
        assert!(!SandboxExecutor::is_exempt("mcp_call"));
    }

    #[tokio::test]
    async fn exec_unavailable_returns_err() {
        let ex = SandboxExecutor {
            backend: None,
            mode: SandboxMode::Auto,
            network: NetworkPolicy::None,
        };
        assert!(!ex.available());
        assert!(ex.exec("ls", Path::new("/tmp")).await.is_err());
    }

    // ── 6.1 fallback contract ─────────────────────────────────────────────────

    fn no_backend(mode: SandboxMode) -> SandboxExecutor {
        SandboxExecutor {
            backend: None,
            mode,
            network: NetworkPolicy::None,
        }
    }

    #[tokio::test]
    async fn required_fails_closed() {
        let ex = no_backend(SandboxMode::Required);
        let mut ran = false;
        let out = ex
            .run_confined("echo hi", Path::new("/tmp"), || {
                ran = true;
                async { "host-output".to_owned() }
            })
            .await;
        assert!(
            out.starts_with("error:"),
            "required must fail closed; got: {out}"
        );
        assert!(
            out.contains("no isolation backend"),
            "must name the missing capability; got: {out}"
        );
        assert!(!ran, "required must NOT execute the command");
    }

    #[tokio::test]
    async fn auto_falls_back_with_marker() {
        let ex = no_backend(SandboxMode::Auto);
        let out = ex
            .run_confined("echo hi", Path::new("/tmp"), || async {
                "host-output".to_owned()
            })
            .await;
        assert!(
            out.starts_with(UNCONFINED_MARKER),
            "auto must stamp the marker; got: {out}"
        );
        assert!(
            out.contains("host-output"),
            "auto must run on the host; got: {out}"
        );
    }

    #[tokio::test]
    async fn off_skips_sandbox() {
        let ex = no_backend(SandboxMode::Off);
        let out = ex
            .run_confined("echo hi", Path::new("/tmp"), || async {
                "host-output".to_owned()
            })
            .await;
        assert_eq!(
            out, "host-output",
            "off must run on the host with no marker"
        );
    }

    // ── 7.1 telemetry attributes ──────────────────────────────────────────────

    #[test]
    fn sandbox_exec_emits_span_with_backend_attributes() {
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Required,
            network: NetworkPolicy::Allowlist,
        };
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert_eq!(tel.backend, "stub");
        assert_eq!(tel.network_policy, "allowlist");
        assert_eq!(tel.mode, "required");
        assert_eq!(tel.confined_root, "/tmp/wt");

        // No backend → telemetry records "none".
        let ex = no_backend(SandboxMode::Auto);
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert_eq!(tel.backend, "none");
    }

    // ── 1.1 shared read-path resolution ───────────────────────────────────────

    #[test]
    fn resolve_read_paths_uses_defaults_and_appends_env() {
        // The defaults must contain core system dirs and must NOT contain the
        // user's home or secret directories.
        let home = std::env::var("HOME").unwrap_or_default();
        for d in DEFAULT_READ_PATHS {
            // Defaults are absolute system dirs, never under $HOME.
            assert!(d.starts_with('/'), "default path must be absolute: {d}");
            if !home.is_empty() {
                assert!(
                    !std::path::Path::new(d).starts_with(&home),
                    "default read paths must not include the home dir: {d}"
                );
            }
        }
        assert!(
            DEFAULT_READ_PATHS.contains(&"/usr"),
            "defaults must include /usr"
        );
        assert!(
            DEFAULT_READ_PATHS.contains(&"/bin"),
            "defaults must include /bin"
        );

        // A colon-separated override is appended to (not replacing) the defaults.
        // Use real, existing directories so the existence filter keeps them.
        let tmp = tempfile::tempdir().unwrap();
        let extra_a = tmp.path().join("toola");
        let extra_b = tmp.path().join("toolb");
        std::fs::create_dir(&extra_a).unwrap();
        std::fs::create_dir(&extra_b).unwrap();
        let override_val = format!("{}:{}", extra_a.display(), extra_b.display());

        // SAFETY: single-threaded test; restored below.
        unsafe {
            std::env::set_var("SMEDJA_SANDBOX_READ_PATHS", &override_val);
        }
        let resolved = resolve_read_paths();
        unsafe {
            std::env::remove_var("SMEDJA_SANDBOX_READ_PATHS");
        }

        // The override entries are present, appended after the defaults.
        assert!(
            resolved.contains(&extra_a),
            "override path A must be appended; got: {resolved:?}"
        );
        assert!(
            resolved.contains(&extra_b),
            "override path B must be appended; got: {resolved:?}"
        );
        // Non-existent default paths are skipped, but at least one default that
        // exists on every host (`/usr` or `/etc`) must survive.
        assert!(
            resolved
                .iter()
                .any(|p| p == std::path::Path::new("/usr") || p == std::path::Path::new("/etc")),
            "at least one existing default must remain; got: {resolved:?}"
        );
    }

    // ── 1.3 telemetry records read/net confinement ────────────────────────────

    #[test]
    fn telemetry_records_read_and_net_confinement() {
        // A backend-backed executor under network=none reports both confinements.
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Auto,
            network: NetworkPolicy::None,
        };
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert!(tel.read_confined, "active backend confines reads");
        assert!(
            tel.net_confined,
            "network=none with an active backend confines the network"
        );

        // No backend → neither confinement applies.
        let ex = no_backend(SandboxMode::Auto);
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert!(!tel.read_confined, "no backend → reads not confined");
        assert!(!tel.net_confined, "no backend → network not confined");

        // open network with a backend → reads confined, network not confined.
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Auto,
            network: NetworkPolicy::Open,
        };
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert!(tel.read_confined);
        assert!(!tel.net_confined, "open egress is not a confined network");
    }

    // ── 5.1 / 5.2 network policy reuses is_blocked_ip floor ────────────────────

    #[test]
    fn network_policy_reuses_is_blocked_ip_floor() {
        use std::net::IpAddr;
        let imds: IpAddr = "169.254.169.254".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let private: IpAddr = "10.0.0.5".parse().unwrap();
        let public: IpAddr = "93.184.216.34".parse().unwrap(); // example.com

        // none: deny all egress.
        assert!(!NetworkPolicy::None.permits_dest(public));
        assert!(!NetworkPolicy::None.permits_dest(imds));

        // allowlist: public allowed, blocked ranges denied.
        assert!(NetworkPolicy::Allowlist.permits_dest(public));
        assert!(!NetworkPolicy::Allowlist.permits_dest(imds));
        assert!(!NetworkPolicy::Allowlist.permits_dest(loopback));
        assert!(!NetworkPolicy::Allowlist.permits_dest(private));

        // open: public allowed, but is_blocked_ip ranges stay blocked.
        assert!(NetworkPolicy::Open.permits_dest(public));
        assert!(!NetworkPolicy::Open.permits_dest(imds));
        assert!(!NetworkPolicy::Open.permits_dest(loopback));
    }

    // ── 7.1 is_blocked_ip floor stays intact under open ───────────────────────

    #[test]
    fn is_blocked_ip_floor_unchanged_under_open() {
        use std::net::IpAddr;
        let imds: IpAddr = "169.254.169.254".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let public: IpAddr = "93.184.216.34".parse().unwrap();

        // The SSRF floor for smedja's own clients is untouched: under `open`
        // the IMDS and loopback addresses stay blocked, public stays allowed.
        assert!(
            !NetworkPolicy::Open.permits_dest(imds),
            "IMDS must stay blocked under open"
        );
        assert!(
            !NetworkPolicy::Open.permits_dest(loopback),
            "loopback must stay blocked under open"
        );
        assert!(
            NetworkPolicy::Open.permits_dest(public),
            "public must stay reachable under open"
        );
    }
}
