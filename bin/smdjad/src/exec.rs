//! Bounded bash execution helpers shared by the executor and fragment builders.
//!
//! [`exec_bash`] runs a command with a default timeout; [`exec_bash_ext`] adds
//! per-call timeout, extra env vars, and stdin.

/// Timeout for `exec_bash` commands (git diff on large repos, cargo clippy, etc.).
const EXEC_BASH_TIMEOUT_SECS: u64 = 30;

/// Maximum per-call timeout accepted via the `timeout_secs` input field.
pub(crate) const SMEDJA_BASH_MAX_TIMEOUT_SECS: u64 = 600;

/// Executes a bash command in `workspace` using `sh -c`, returning stdout or a
/// formatted error string. Bounded by [`EXEC_BASH_TIMEOUT_SECS`]; a hung command
/// (e.g. git diff on a large repo) returns a timeout error rather than blocking.
pub(crate) async fn exec_bash(cmd: &str, workspace: &std::path::Path) -> String {
    exec_bash_ext(cmd, workspace, None, None, None).await
}

/// Extended `exec_bash` supporting per-call timeout, extra env vars, and stdin.
///
/// Uses spawn-based execution so stdout/stderr are captured concurrently.
/// On timeout the child is killed and any partial stdout already read is
/// returned with a timeout suffix. Stderr is appended as a `[stderr]` block
/// when the exit status is non-zero.
#[allow(clippy::items_after_statements)]
pub(crate) async fn exec_bash_ext(
    cmd: &str,
    workspace: &std::path::Path,
    timeout_secs: Option<u64>,
    env_extra: Option<std::collections::HashMap<String, String>>,
    stdin_bytes: Option<Vec<u8>>,
) -> String {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let timeout = timeout_secs.map_or(EXEC_BASH_TIMEOUT_SECS, |t| {
        t.min(SMEDJA_BASH_MAX_TIMEOUT_SECS)
    });

    let mut builder = tokio::process::Command::new("sh");
    builder
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(ref env_map) = env_extra {
        for (k, v) in env_map {
            builder.env(k, v);
        }
    }

    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => return format!("error: {e}"),
    };

    if let Some(bytes) = stdin_bytes {
        if let Some(mut h) = child.stdin.take() {
            let _ = h.write_all(&bytes).await;
        }
    }

    async fn read_all(reader: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> String {
        let mut buf = String::new();
        let mut r = BufReader::new(reader);
        let mut line = String::new();
        while r.read_line(&mut line).await.unwrap_or(0) > 0 {
            buf.push_str(&line);
            line.clear();
        }
        buf
    }

    let stdout_reader = child.stdout.take().expect("stdout piped");
    let stderr_reader = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(read_all(stdout_reader));
    let stderr_task = tokio::spawn(read_all(stderr_reader));

    match tokio::time::timeout(std::time::Duration::from_secs(timeout), child.wait()).await {
        Ok(Ok(status)) => {
            let out = stdout_task.await.unwrap_or_default();
            let err = stderr_task.await.unwrap_or_default();
            if status.success() {
                out
            } else {
                let mut result = format!("error: exit status {status}\n");
                result.push_str(&out);
                if !err.is_empty() {
                    if !result.ends_with('\n') {
                        result.push('\n');
                    }
                    result.push_str("[stderr]\n");
                    result.push_str(&err);
                }
                result
            }
        }
        Ok(Err(e)) => format!("error: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            // Give readers 5 s to drain after kill.
            let out = tokio::time::timeout(std::time::Duration::from_secs(5), stdout_task)
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            let err = tokio::time::timeout(std::time::Duration::from_secs(5), stderr_task)
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            let mut result = out;
            if !err.is_empty() {
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push_str("[stderr]\n");
                result.push_str(&err);
            }
            if !result.ends_with('\n') && !result.is_empty() {
                result.push('\n');
            }
            result.push_str("error: command timed out after ");
            result.push_str(&timeout.to_string());
            result.push('s');
            result
        }
    }
}
