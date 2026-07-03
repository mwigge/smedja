//! MCP notification bus, background reader, and protocol-version negotiation.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::types::MCP_PROTOCOL_VERSIONS;

/// A server-initiated MCP notification (no `id` field).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpNotification {
    pub server: String,
    pub method: String,
}

/// Capacity of the notification broadcast channel.
const NOTIFICATION_CHANNEL_CAP: usize = 64;

/// Type alias for the sender half of the MCP notification bus.
pub type McpNotificationTx = broadcast::Sender<McpNotification>;

/// Creates a new notification bus and returns `(tx, rx)`.
///
/// Multiple consumers may subscribe by calling [`McpNotificationTx::subscribe`].
#[must_use]
pub fn notification_bus() -> (
    broadcast::Sender<McpNotification>,
    broadcast::Receiver<McpNotification>,
) {
    broadcast::channel(NOTIFICATION_CHANNEL_CAP)
}

/// Classifies a JSON-RPC notification by its `method` field and decides
/// whether it should trigger a tool-list refresh.
///
/// Returns `true` for `notifications/tools/list_changed`.
#[must_use]
pub fn is_tools_list_change(notification: &McpNotification) -> bool {
    notification.method == "notifications/tools/list_changed"
}

/// Spawns a background Tokio task that reads newline-delimited JSON-RPC
/// messages from `reader`, parses them as notifications, and broadcasts them
/// on `tx`.  The task exits when the reader returns EOF or an error.
///
/// This is the stdio-transport notification pump: attach it to the stdout pipe
/// of a stdio MCP server process.
pub fn spawn_notification_reader<R>(
    server: String,
    reader: R,
    tx: McpNotificationTx,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncBufRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt as _;
        let mut lines = tokio::io::BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            // A notification has "method" but no "id".
            if v.get("id").is_some() {
                continue;
            }
            let Some(method) = v["method"].as_str() else {
                continue;
            };
            let notification = McpNotification {
                server: server.clone(),
                method: method.to_owned(),
            };
            // Ignore send errors (no active receivers is fine).
            let _ = tx.send(notification);
        }
    })
}

/// Returns the negotiated protocol version given the server's declared version.
///
/// Scans [`MCP_PROTOCOL_VERSIONS`] in preference order and returns the first
/// version that is ≤ the server's declared version (string comparison works
/// because the format is `YYYY-MM-DD`).  Falls back to the server's own version
/// when none of ours is older-or-equal.
#[must_use]
pub fn negotiate_protocol_version(server_version: &str) -> &'static str {
    for &v in MCP_PROTOCOL_VERSIONS {
        if v <= server_version {
            return v;
        }
    }
    // Server is older than all our known versions; accept oldest known.
    MCP_PROTOCOL_VERSIONS[MCP_PROTOCOL_VERSIONS.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_negotiation_prefers_latest() {
        // Server declares "2025-03-26" — we accept that (it's in our list).
        let negotiated = negotiate_protocol_version("2025-03-26");
        assert_eq!(negotiated, "2025-03-26");
    }

    #[test]
    fn protocol_version_negotiation_accepts_server_older_than_all_known() {
        // Server is older than all versions we know — fall back to oldest known.
        let negotiated = negotiate_protocol_version("2023-01-01");
        assert_eq!(
            negotiated,
            MCP_PROTOCOL_VERSIONS[MCP_PROTOCOL_VERSIONS.len() - 1]
        );
    }

    #[test]
    fn mcp_notification_parsed_from_json() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        // A notification has "method" but no "id".
        assert!(v.get("method").is_some());
        assert!(v.get("id").is_none());
        let n = McpNotification {
            server: "my-server".into(),
            method: v["method"].as_str().unwrap().to_owned(),
        };
        assert_eq!(n.method, "notifications/tools/list_changed");
    }

    #[tokio::test]
    async fn tools_list_change_triggers_refresh() {
        let (tx, mut rx) = notification_bus();
        // Feed a single notification line through the background reader.
        let json = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n";
        let reader = std::io::Cursor::new(json.as_bytes());
        let handle = spawn_notification_reader("test-server".into(), reader, tx);
        // Wait for the notification to arrive.
        let notification = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for notification")
            .expect("channel closed");
        // The notification should be classified as a tool-list change.
        assert!(
            is_tools_list_change(&notification),
            "notification must be classified as a tool-list change"
        );
        assert_eq!(notification.server, "test-server");
        handle.await.ok();
    }
}
