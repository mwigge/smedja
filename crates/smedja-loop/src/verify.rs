//! Verification gate — runs the configured verification command and captures
//! its exit code, stdout, and stderr.
//!
//! A non-zero exit code or timeout is a normal outcome, not a Rust error.
//! [`run_verification`] returns `Err` only when the child process cannot be
//! spawned (e.g. the binary does not exist).

use std::time::Duration;

use anyhow::Result;

/// The result of running the verification command.
#[derive(Debug)]
pub struct VerifyResult {
    /// Process exit code, or `-1` on timeout.
    pub exit_code: i32,
    /// Standard output captured from the command.
    pub stdout: String,
    /// Standard error captured from the command, or `"timed out"` on timeout.
    pub stderr: String,
    /// `true` when the command exceeded the allowed wall-clock duration.
    pub timed_out: bool,
}

impl VerifyResult {
    /// Returns `true` when the command exited successfully (code 0, no timeout).
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit_code == 0
    }
}

/// Runs `command` as a shell process with the given `timeout`.
///
/// The command string is split on whitespace into program + arguments; simple
/// quoting is not handled — for complex commands wrap them in a shell script.
///
/// Returns error only on spawn failure.  Timeouts and non-zero exit codes are
/// returned as [`VerifyResult`] fields.
///
/// # Errors
///
/// Returns [`anyhow::Error`] when the command is empty or the subprocess cannot
/// be spawned.
pub async fn run_verification(command: &str, timeout: Duration) -> Result<VerifyResult> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (prog, args) = parts
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty verification command"))?;

    let spawn_result = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(prog).args(args).output(),
    )
    .await;

    match spawn_result {
        Err(_elapsed) => Ok(VerifyResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: "timed out".into(),
            timed_out: true,
        }),
        Ok(Ok(output)) => Ok(VerifyResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            timed_out: false,
        }),
        Ok(Err(e)) => Err(e.into()),
    }
}

/// Returns the verification timeout from the `SMEDJA_LOOP_VERIFY_TIMEOUT`
/// environment variable, falling back to 300 seconds.
#[must_use]
pub fn verification_timeout() -> Duration {
    let secs = std::env::var("SMEDJA_LOOP_VERIFY_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises tests that mutate the process-global
    /// `SMEDJA_LOOP_VERIFY_TIMEOUT` env var so they cannot race each other.
    static VERIFY_TIMEOUT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const VERIFY_TIMEOUT_ENV: &str = "SMEDJA_LOOP_VERIFY_TIMEOUT";

    #[tokio::test]
    async fn verification_timeout_recorded() {
        // `sleep 999` will exceed the 100 ms budget.
        let result = run_verification("sleep 999", Duration::from_millis(100))
            .await
            .unwrap();
        assert!(result.timed_out);
        assert_eq!(result.exit_code, -1);
        assert_eq!(result.stderr, "timed out");
    }

    #[tokio::test]
    async fn successful_command_passes() {
        let result = run_verification("true", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!result.timed_out);
        assert_eq!(result.exit_code, 0);
        assert!(result.passed());
    }

    #[tokio::test]
    async fn failing_command_captures_exit_code() {
        let result = run_verification("false", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!result.timed_out);
        assert_ne!(result.exit_code, 0);
        assert!(!result.passed());
    }

    #[tokio::test]
    async fn empty_command_returns_error() {
        let result = run_verification("", Duration::from_secs(5)).await;
        assert!(result.is_err());
    }

    #[test]
    fn verification_timeout_reads_env_var() {
        let _guard = VERIFY_TIMEOUT_ENV_LOCK.lock().unwrap();
        let previous = std::env::var(VERIFY_TIMEOUT_ENV).ok();

        std::env::set_var(VERIFY_TIMEOUT_ENV, "42");
        assert_eq!(verification_timeout(), Duration::from_secs(42));

        match previous {
            Some(value) => std::env::set_var(VERIFY_TIMEOUT_ENV, value),
            None => std::env::remove_var(VERIFY_TIMEOUT_ENV),
        }
    }

    #[test]
    fn verification_timeout_defaults_to_300s_when_unset() {
        let _guard = VERIFY_TIMEOUT_ENV_LOCK.lock().unwrap();
        let previous = std::env::var(VERIFY_TIMEOUT_ENV).ok();

        std::env::remove_var(VERIFY_TIMEOUT_ENV);
        assert_eq!(verification_timeout(), Duration::from_mins(5));

        if let Some(value) = previous {
            std::env::set_var(VERIFY_TIMEOUT_ENV, value);
        }
    }

    #[test]
    fn verify_result_passed_is_false_when_timed_out() {
        let r = VerifyResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: "timed out".into(),
            timed_out: true,
        };
        assert!(!r.passed());
    }
}
