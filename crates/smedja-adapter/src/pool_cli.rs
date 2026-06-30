//! Poolside `pool` CLI subprocess adapter.
//!
//! Protocol: spawn `pool exec --output markdown -f <tmpfile> [--directory <workspace>]`.
//! The prompt is delivered via a temp file (not stdin) to avoid shell-arg limits;
//! auth is handled by the `pool` binary via `~/.config/poolside/credentials.json`.

use tokio::io::AsyncBufReadExt as _;
use tokio_stream::wrappers::ReceiverStream;

use crate::{AdapterError, CallOptions, Delta, DeltaStream, Message, Provider};

/// Provider that routes turns to Poolside via the `pool` CLI.
pub struct PoolCliProvider;

impl PoolCliProvider {
    /// Returns `Some(Self)` if the `pool` binary is on `$PATH`.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if which::which("pool").is_ok() {
            Some(Self)
        } else {
            None
        }
    }
}

impl Provider for PoolCliProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        let prompt = messages
            .iter()
            .map(|m| format!("{}: {}", m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n");
        let workspace = opts.workspace.clone();
        stream_pool_with_binary("pool", prompt, workspace.as_deref())
    }
}

fn stream_pool_with_binary(
    binary: &str,
    prompt: String,
    workspace: Option<&std::path::Path>,
) -> DeltaStream {
    let binary = binary.to_owned();
    let workspace = workspace.map(std::borrow::ToOwned::to_owned);
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    tokio::spawn(async move {
        // Write prompt to a 0600 temp file to avoid shell-arg limits.
        let tmp = match tokio::task::spawn_blocking(|| {
            let f = tempfile::NamedTempFile::new()?;
            // Restrict to owner-read/write before writing the prompt.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                f.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            Ok::<_, std::io::Error>(f)
        })
        .await
        {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                let _ = tx
                    .send(Err(AdapterError::Request(format!("pool tmpfile: {e}"))))
                    .await;
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(Err(AdapterError::Request(format!(
                        "pool tmpfile task: {e}"
                    ))))
                    .await;
                return;
            }
        };

        let tmp_path = tmp.path().to_owned();
        if let Err(e) = tokio::fs::write(&tmp_path, prompt.as_bytes()).await {
            let _ = tx
                .send(Err(AdapterError::Request(format!(
                    "pool write prompt: {e}"
                ))))
                .await;
            return;
        }

        let mut args = vec![
            "exec".to_owned(),
            "--output".to_owned(),
            "markdown".to_owned(),
            "-f".to_owned(),
            tmp_path.to_string_lossy().into_owned(),
        ];
        if let Some(ref ws) = workspace {
            args.push("--directory".to_owned());
            args.push(ws.to_string_lossy().into_owned());
        }

        let mut child = match tokio::process::Command::new(&binary)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(Err(AdapterError::Request(format!("pool spawn: {e}"))))
                    .await;
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(Ok(Delta::Text(line + "\n"))).await.is_err() {
                    break;
                }
            }
        }

        let _ = child.wait().await;
        // tmp is dropped here, cleaning up the temp file.
        drop(tmp);
    });

    Box::pin(ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    #[test]
    fn detect_returns_none_when_pool_absent() {
        // `pool` is not installed in the dev environment.
        if which::which("pool").is_ok() {
            assert!(PoolCliProvider::detect().is_some());
        } else {
            assert!(PoolCliProvider::detect().is_none());
        }
    }

    fn write_fake_pool_script(
        dir: &std::path::Path,
        name: &str,
        script: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Verifies that stdout lines from the pool CLI are forwarded as `Delta::Text`.
    #[tokio::test]
    async fn pool_adapter_maps_lines_to_text_deltas() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let fake = write_fake_pool_script(
            tmp_dir.path(),
            "fake-pool",
            "#!/bin/sh\necho '# heading'\necho 'body text'\n",
        );

        let texts: Vec<String> =
            stream_pool_with_binary(&fake.to_string_lossy(), "hello".to_owned(), None)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .filter_map(std::result::Result::ok)
                .filter_map(|d| match d {
                    Delta::Text(t) => Some(t),
                    _ => None,
                })
                .collect();

        assert!(
            texts.iter().any(|t| t.contains("heading")),
            "expected heading in output, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("body")),
            "expected body in output, got: {texts:?}"
        );
    }

    /// Verifies that when the `pool` binary is absent, the stream yields an error.
    #[tokio::test]
    async fn pool_stream_errors_when_binary_absent() {
        let deltas: Vec<_> =
            stream_pool_with_binary("__no_such_pool_binary__", "test".to_owned(), None)
                .collect()
                .await;
        assert!(
            !deltas.is_empty(),
            "expected at least one item (error) in stream"
        );
        assert!(
            deltas[0].is_err(),
            "expected error when pool binary absent, got: {deltas:?}"
        );
    }

    /// Verifies `--directory` is passed when workspace is set.
    #[tokio::test]
    async fn pool_workspace_forwarded_via_directory_flag() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let ws_dir = tempfile::TempDir::new().unwrap();
        let script = "#!/bin/sh\n\
            dir=''\n\
            while [ \"$#\" -gt 0 ]; do\n\
              if [ \"$1\" = \"--directory\" ]; then dir=\"$2\"; fi\n\
              shift\n\
            done\n\
            echo \"dir=$dir\"\n";
        let fake = write_fake_pool_script(tmp_dir.path(), "fake-pool-ws", script);
        let ws_path = ws_dir.path().to_owned();

        let texts: Vec<String> =
            stream_pool_with_binary(&fake.to_string_lossy(), "test".to_owned(), Some(&ws_path))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .filter_map(std::result::Result::ok)
                .filter_map(|d| match d {
                    Delta::Text(t) => Some(t),
                    _ => None,
                })
                .collect();

        let combined = texts.join("");
        let ws_str = ws_dir.path().to_string_lossy();
        assert!(
            combined.contains(ws_str.as_ref()),
            "workspace path {ws_str:?} not forwarded via --directory; got: {combined:?}"
        );
    }
}
