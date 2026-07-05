//! In-process bash command execution for the daemon's tool runner.
//!
//! [`exec_bash_ext`] spawns `sh -c`, draining stdout/stderr concurrently and
//! feeding stdin on its own task so large-output commands never deadlock. It is
//! re-exported from the crate root because the executor and fragment layers call
//! it as `crate::exec_bash_ext`.

use std::sync::Arc;

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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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

    async fn read_into(
        mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        target: Arc<std::sync::Mutex<String>>,
    ) {
        let mut chunk = [0u8; 8192];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let text = String::from_utf8_lossy(&chunk[..n]);
            if let Ok(mut buf) = target.lock() {
                buf.push_str(&text);
            }
        }
    }

    fn snapshot(target: &Arc<std::sync::Mutex<String>>) -> String {
        target
            .lock()
            .map_or_else(|_| String::new(), |buf| buf.clone())
    }

    async fn finish_reader(
        task: tokio::task::JoinHandle<()>,
        target: &Arc<std::sync::Mutex<String>>,
    ) -> String {
        let abort = task.abort_handle();
        if tokio::time::timeout(std::time::Duration::from_millis(250), task)
            .await
            .is_err()
        {
            abort.abort();
        }
        snapshot(target)
    }

    let stdout_reader = child.stdout.take().expect("stdout piped");
    let stderr_reader = child.stderr.take().expect("stderr piped");
    let stdout_buf = Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_buf = Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_task = tokio::spawn(read_into(stdout_reader, Arc::clone(&stdout_buf)));
    let stderr_task = tokio::spawn(read_into(stderr_reader, Arc::clone(&stderr_buf)));

    // Feed stdin only AFTER the stdout/stderr readers are draining. Writing all
    // of stdin first (as before) deadlocks whenever the child emits more than one
    // pipe buffer (~64 KB) of output: the child blocks on a full stdout pipe that
    // nothing is reading yet, while we block writing stdin. Running the writer on
    // its own task lets both directions make progress; dropping the handle on
    // completion signals EOF to the child.
    if let Some(bytes) = stdin_bytes {
        if let Some(mut h) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = h.write_all(&bytes).await;
                // `h` drops here, closing the child's stdin (EOF).
            });
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(timeout), child.wait()).await {
        Ok(Ok(status)) => {
            let out = finish_reader(stdout_task, &stdout_buf).await;
            let err = finish_reader(stderr_task, &stderr_buf).await;
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
            let _ = tokio::time::timeout(std::time::Duration::from_millis(100), child.wait()).await;
            let out = snapshot(&stdout_buf);
            let err = snapshot(&stderr_buf);
            stdout_task.abort();
            stderr_task.abort();
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
