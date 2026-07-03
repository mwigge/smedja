//! Data types and constants for the MCP HTTP transport.

use serde::{Deserialize, Serialize};

/// MCP protocol versions in preference order (newest first).
pub const MCP_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Reconnect delay sequence for stdio MCP clients (in seconds).
///
/// Exponential backoff capped at 30 s, up to 5 retries.
pub const RECONNECT_DELAYS_SECS: &[u64] = &[1, 2, 4, 8, 30];

/// An MCP resource entry as returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// An MCP prompt entry as returned by `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpPrompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A tool entry as returned by an MCP `tools/list` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_deserializes() {
        let json = r#"{"name":"read_file","description":"Read a file","input_schema":{}}"#;
        let tool: McpTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "read_file");
    }

    #[test]
    fn mcp_resource_serializes_correctly() {
        let resource = McpResource {
            uri: "file:///foo/bar.txt".into(),
            name: "bar.txt".into(),
            description: Some("A text file".into()),
            mime_type: Some("text/plain".into()),
        };
        let json = serde_json::to_string(&resource).unwrap();
        let back: McpResource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resource);
    }

    #[test]
    fn mcp_prompt_serializes_correctly() {
        let prompt = McpPrompt {
            name: "summarize".into(),
            description: Some("Summarize a document".into()),
        };
        let json = serde_json::to_string(&prompt).unwrap();
        let back: McpPrompt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prompt);
    }

    #[test]
    fn reconnect_backoff_delays_grow() {
        let delays = RECONNECT_DELAYS_SECS;
        assert_eq!(delays.len(), 5, "must have exactly 5 retry delays");
        assert_eq!(delays[0], 1);
        assert_eq!(delays[1], 2);
        assert_eq!(delays[2], 4);
        assert_eq!(delays[3], 8);
        assert_eq!(delays[4], 30);
        // Verify monotone growth up to the cap.
        for w in delays.windows(2) {
            assert!(w[1] >= w[0], "delays must be non-decreasing");
        }
    }
}
