use super::*;
use async_trait::async_trait;

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

// ── 1. read-confinement escape via a symlinked .git ───────────────────────

#[cfg(unix)]
#[test]
fn resolve_confined_root_rejects_symlinked_git() {
    // Attack: `ln -s <secret> .git` inside the confined root. The old
    // `.exists()` check followed the symlink and mounted the secret at
    // `/workspace/.git`. The guard must refuse a symlinked `.git`.
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().canonicalize().unwrap();
    std::fs::write(secret.join("id_rsa"), "HOSTKEY").unwrap();

    std::os::unix::fs::symlink(&secret, root.join(".git")).unwrap();

    let (_r, git) = resolve_confined_root(&root).unwrap();
    assert!(
        git.is_none(),
        "a symlinked .git must never be mounted; got: {git:?}"
    );
}

#[test]
fn resolve_confined_root_mounts_real_git_dir() {
    // A genuine `.git` directory inside the root is still mounted (canonical
    // path under the root), preserving the read-only .git shadow behaviour.
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();

    let (_r, git) = resolve_confined_root(&root).unwrap();
    assert_eq!(
        git,
        Some(root.join(".git")),
        "a real .git directory must still be mounted"
    );
}
