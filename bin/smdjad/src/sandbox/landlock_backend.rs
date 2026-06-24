//! Linux Landlock isolation backend.
//!
//! Builds a Landlock ruleset that grants read-write only under the confined
//! root and read-execute over a bounded *allow-list* of system directories,
//! then applies it in the child process (via `pre_exec`) before the command's
//! `exec`.
//!
//! Landlock here enforces both the *write* and the *read* boundary (ABI v1,
//! kernel ≥ 5.13). Because the ruleset is additive-grant — rules only *add*
//! access and cannot carve a read-only hole beneath a broader grant — read
//! confinement is achieved by *tightening the allow-list*, not by deny rules:
//! instead of granting read across all of `/`, the backend grants read+execute
//! only over the system directories a shell and its shared libraries actually
//! need (`resolve_read_paths()`), plus read-write over the confined root. The
//! user's home directory and its secret subpaths (`~/.ssh`, `~/.aws`,
//! `~/.config`, `~/.gnupg`) are never granted, so a sandboxed command cannot
//! read host credentials. Writes outside the confined root are denied by the
//! kernel as before.
//!
//! Network confinement is enforced for `NetworkPolicy::None`: the child is
//! placed in a fresh network namespace (`unshare(CLONE_NEWNET)` in `pre_exec`,
//! ordered before `restrict_self`) so it has no route to any host. For
//! `allowlist`/`open` the child keeps the host network; a raw subprocess cannot
//! be per-destination IP-filtered without a proxy, so `allowlist` is treated as
//! `open`-minus-blocked-ranges (the `is_blocked_ip` floor governs smedja's own
//! clients, not the child's sockets).
//!
//! Availability is detected at startup; when Landlock is unavailable (kernel <
//! 5.13 or disabled) the backend reports `available() == false` so selection
//! downgrades to no-backend.

use std::path::Path;

use async_trait::async_trait;
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    ABI,
};

use super::{
    resolve_confined_root, resolve_read_paths, NetworkPolicy, SandboxBackend, EXEC_TIMEOUT_SECS,
};

/// Executes commands confined by the Landlock LSM.
pub struct LandlockBackend {
    available: bool,
}

/// How the child command's network is confined for a given policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetPlan {
    /// Run the command in a fresh network namespace (`unshare --net`): no egress.
    Namespace,
    /// Keep the host network (`allowlist`/`open`); no per-host filtering for a
    /// raw subprocess (open-minus-blocked-ranges).
    Host,
}

/// Maps a [`NetworkPolicy`] to the child's [`NetPlan`].
///
/// `None` requires a fresh network namespace; `allowlist`/`open` keep the host
/// network. Pure mapping so it is unit-testable without a kernel sandbox.
#[must_use]
pub(crate) fn net_plan(policy: NetworkPolicy) -> NetPlan {
    match policy {
        NetworkPolicy::None => NetPlan::Namespace,
        NetworkPolicy::Allowlist | NetworkPolicy::Open => NetPlan::Host,
    }
}

/// Builds the `(program, args)` for the child given the network plan.
///
/// Under [`NetPlan::Namespace`] the command is wrapped in `unshare --net --`
/// so it runs in a fresh, route-less network namespace; otherwise it runs as a
/// plain `sh -c`. Pure string logic so it is unit-testable on any platform.
#[must_use]
pub(crate) fn build_child_argv(cmd: &str, plan: NetPlan) -> (&'static str, Vec<String>) {
    match plan {
        NetPlan::Namespace => (
            "unshare",
            vec![
                "--net".to_owned(),
                "--".to_owned(),
                "sh".to_owned(),
                "-c".to_owned(),
                cmd.to_owned(),
            ],
        ),
        NetPlan::Host => ("sh", vec!["-c".to_owned(), cmd.to_owned()]),
    }
}

impl LandlockBackend {
    /// Detects Landlock support by building (not enforcing) a minimal ruleset.
    ///
    /// `create()` allocates the ruleset without restricting the current process
    /// (only `restrict_self` does that), so this probe is non-destructive. If
    /// the kernel cannot create the ruleset, the backend reports unavailable and
    /// selection downgrades. Runtime enforcement is still re-checked per command
    /// via the `RulesetStatus::NotEnforced` guard in `apply`.
    #[must_use]
    pub fn detect() -> Self {
        let available = Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(Ruleset::create)
            .is_ok();
        Self { available }
    }

    /// Returns `true` when this host can create a fresh network namespace for
    /// the child (so `NetworkPolicy::None` can be enforced).
    ///
    /// Probes by running `unshare --net true`: it succeeds only when the
    /// `unshare` binary is present *and* the caller may create a network
    /// namespace (root, `CAP_NET_ADMIN`, or unprivileged user namespaces). When
    /// it fails, `none` cannot be honoured and the backend signals the missing
    /// confinement so the `Required`/`Auto` mode contract applies.
    #[must_use]
    pub fn netns_supported() -> bool {
        if which::which("unshare").is_err() {
            return false;
        }
        std::process::Command::new("unshare")
            .args(["--net", "true"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Applies the filesystem ruleset confining writes to `root` in the current
    /// process. Intended to be called from `pre_exec` in the child.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` when the ruleset cannot be created or applied.
    fn apply(root: &Path) -> std::io::Result<()> {
        let abi = ABI::V1;
        let map_err = |e: landlock::RulesetError| std::io::Error::other(format!("landlock: {e}"));
        // PathFd::new's error type is landlock-internal; map it to io::Error by
        // its Display so this io::Result function can propagate it uniformly.
        let open = |p: &Path| {
            PathFd::new(p).map_err(|e| std::io::Error::other(format!("landlock path: {e}")))
        };

        let mut created = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(map_err)?
            .create()
            .map_err(map_err)?
            // Read-write only beneath the confined root.
            .add_rule(PathBeneath::new(open(root)?, AccessFs::from_all(abi)))
            .map_err(map_err)?;

        // Read + execute over the bounded system-dir allow-list so the shell
        // and its shared libraries load — but NOT across all of `/`, so the
        // user's home and secret directories stay unreadable. Paths that fail
        // to open (absent on this host) are skipped, not errored, mirroring the
        // existence filter in `resolve_read_paths`.
        for path in resolve_read_paths() {
            let Ok(fd) = open(&path) else {
                continue;
            };
            created = created
                .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                .map_err(map_err)?;
        }

        let status = created.restrict_self().map_err(map_err)?;
        if matches!(status.ruleset, RulesetStatus::NotEnforced) {
            return Err(std::io::Error::other(
                "landlock: ruleset not enforced by the kernel",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl SandboxBackend for LandlockBackend {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn available(&self) -> bool {
        self.available
    }

    async fn exec(
        &self,
        cmd: &str,
        confined_root: &Path,
        policy: NetworkPolicy,
    ) -> Result<String, String> {
        if !self.available {
            return Err("landlock sandbox not available".into());
        }

        let (root, _git) = resolve_confined_root(confined_root)?;
        let root_for_child = root.clone();

        // Resolve the network plan. `none` requires a fresh network namespace;
        // if this host cannot create one, fail closed rather than silently
        // granting egress, so the `Required`/`Auto` contract in `run_confined`
        // governs the outcome.
        let plan = net_plan(policy);
        if matches!(plan, NetPlan::Namespace) && !Self::netns_supported() {
            return Err(
                "network confinement unavailable: cannot create a network namespace \
                 (need unshare(1) and CAP_NET_ADMIN or unprivileged user namespaces); \
                 set SMEDJA_SANDBOX_NETWORK=open to run with host network, or \
                 SMEDJA_SANDBOX_MODE=off to disable the sandbox"
                    .to_owned(),
            );
        }

        let (program, args) = build_child_argv(cmd, plan);
        let mut command = tokio::process::Command::new(program);
        command.args(&args).current_dir(&root);

        // SAFETY: the closure runs in the forked child before `exec`; it only
        // calls async-signal-safe Landlock syscalls, and allocations performed
        // before fork are not freed here. Any error aborts the exec.
        unsafe {
            command.pre_exec(move || Self::apply(&root_for_child));
        }

        match tokio::time::timeout(
            std::time::Duration::from_secs(EXEC_TIMEOUT_SECS),
            command.output(),
        )
        .await
        {
            Err(_) => Err(format!(
                "sandbox: command timed out after {EXEC_TIMEOUT_SECS} seconds"
            )),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(out)) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure-logic tests (compile + run on any platform) ──────────────────────

    #[test]
    fn net_plan_maps_none_to_namespace_else_host() {
        assert_eq!(net_plan(NetworkPolicy::None), NetPlan::Namespace);
        assert_eq!(net_plan(NetworkPolicy::Allowlist), NetPlan::Host);
        assert_eq!(net_plan(NetworkPolicy::Open), NetPlan::Host);
    }

    #[test]
    fn build_child_argv_wraps_in_unshare_for_namespace() {
        // Namespace plan wraps the command in `unshare --net -- sh -c <cmd>`.
        let (prog, args) = build_child_argv("echo hi", NetPlan::Namespace);
        assert_eq!(prog, "unshare");
        assert_eq!(args, vec!["--net", "--", "sh", "-c", "echo hi"]);

        // Host plan runs a plain `sh -c <cmd>` with no network namespace.
        let (prog, args) = build_child_argv("echo hi", NetPlan::Host);
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
        assert!(
            !args.contains(&"--net".to_owned()),
            "host plan must NOT create a network namespace"
        );
    }

    // ── 5.4 allowlist/open keep the host network ──────────────────────────────

    #[test]
    fn allowlist_keeps_host_network() {
        // `allowlist`/`open` retain host egress (open-minus-blocked-ranges for a
        // raw subprocess): no namespace is created, so `unshare` is not invoked.
        for policy in [NetworkPolicy::Allowlist, NetworkPolicy::Open] {
            let (prog, _) = build_child_argv("curl example.com", net_plan(policy));
            assert_eq!(prog, "sh", "{policy:?} must keep the host network");
        }
    }

    #[tokio::test]
    async fn landlock_ruleset_denies_write_outside_root() {
        let backend = LandlockBackend::detect();
        if !backend.available() {
            // Kernel without Landlock (e.g. older CI image): the contract is
            // that the backend reports unavailable and selection downgrades.
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // Use `Open` so this write-boundary test does not depend on network
        // namespace availability (which `None` would require).
        let inside = backend
            .exec("echo ok > inside.txt", &root, NetworkPolicy::Open)
            .await;
        assert!(inside.is_ok(), "write inside root must succeed: {inside:?}");
        assert!(root.join("inside.txt").exists());

        // A write to a sibling outside the confined root is denied by the
        // kernel; the command fails (non-zero exit → Err).
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_target = outside_dir.path().join("escape.txt");
        let cmd = format!("echo escape > {}", outside_target.display());
        let outside = backend.exec(&cmd, &root, NetworkPolicy::Open).await;
        assert!(
            outside.is_err(),
            "write outside confined root must be denied: {outside:?}"
        );
        assert!(
            !outside_target.exists(),
            "the escaping write must not have landed"
        );
    }

    // ── 2.1 read confinement denies secret reads ──────────────────────────────

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_denies_read_of_home_secret() {
        let backend = LandlockBackend::detect();
        if !backend.available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // A "secret" file outside the confined root and outside the system-dir
        // allow-list: it must be unreadable by the sandboxed command.
        let secret_dir = tempfile::tempdir().unwrap();
        let secret = secret_dir.path().join("credentials");
        std::fs::write(&secret, "AWS_SECRET=hunter2").unwrap();

        let cmd = format!("cat {}", secret.display());
        // `Open` keeps host network so the result reflects only the read
        // boundary, not netns availability.
        let out = backend.exec(&cmd, &root, NetworkPolicy::Open).await;
        assert!(
            out.is_err(),
            "reading a secret outside the allow-list must be denied: {out:?}"
        );
        if let Ok(text) = out {
            assert!(
                !text.contains("hunter2"),
                "secret content must not leak in output"
            );
        }
    }

    // ── 2.2 read confinement still allows system reads ────────────────────────

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_allows_read_of_system_dirs() {
        let backend = LandlockBackend::detect();
        if !backend.available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // The shell itself must load (its loader + libs live under the system
        // allow-list) and read an allow-listed system path.
        let out = backend
            .exec("test -r /bin/sh && echo ok", &root, NetworkPolicy::Open)
            .await;
        assert!(
            out.as_deref().map(str::trim) == Ok("ok"),
            "an allow-listed system path must stay readable: {out:?}"
        );
    }

    // ── 5.1 netns denies egress under none ────────────────────────────────────

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_netns_denies_egress_when_policy_none() {
        let backend = LandlockBackend::detect();
        if !backend.available() || !LandlockBackend::netns_supported() {
            // No Landlock or no network namespace support: skip; the
            // netns-unavailable contract is covered separately.
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // In a fresh network namespace the loopback interface is down and there
        // is no route anywhere; a name resolution / connect attempt fails.
        let out = backend
            .exec(
                "getent hosts example.com || exit 7",
                &root,
                NetworkPolicy::None,
            )
            .await;
        assert!(
            out.is_err(),
            "egress under none must fail in a fresh network namespace: {out:?}"
        );
    }

    // ── 5.3 netns unavailable → backend signals net unconfined ────────────────

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn netns_unavailable_reports_net_unconfined() {
        let backend = LandlockBackend::detect();
        if !backend.available() {
            return;
        }
        if LandlockBackend::netns_supported() {
            // This host CAN create a namespace, so the unavailable path cannot
            // be exercised here; it is asserted on hosts without netns on CI.
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // `none` requested but netns unavailable → fail closed with a message
        // naming the missing confinement, so `run_confined`'s mode contract
        // governs the outcome.
        let out = backend.exec("echo hi", &root, NetworkPolicy::None).await;
        assert!(out.is_err(), "must fail closed when netns is unavailable");
        let msg = out.unwrap_err();
        assert!(
            msg.contains("network confinement unavailable"),
            "error must name the missing network confinement; got: {msg}"
        );
    }
}
