//! Anthropic Messages API request-body construction.

use serde_json::json;

use crate::{CallOptions, Message, Role};

fn role_to_str(role: &Role) -> &'static str {
    match role {
        // System messages are extracted to the top-level field before the
        // message array is built; this arm should never be reached in practice.
        Role::System | Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Builds the Anthropic request body from messages and call options.
pub(crate) fn build_body(messages: &[Message], opts: &CallOptions) -> serde_json::Value {
    let cache = opts.stable_prefix_len.is_some();

    // Anthropic requires the system prompt as a top-level field; extract it
    // from `opts.system` first, then fall back to the first System message.
    let system_content: Option<String> = opts.system.clone().or_else(|| {
        messages
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone())
    });

    // Filter non-system messages and apply cache_control to the stable-prefix boundary.
    let prefix_len = opts.stable_prefix_len.unwrap_or(0);
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != Role::System).collect();

    let msg_array: Vec<serde_json::Value> = non_system
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = cache && prefix_len > 0 && i + 1 == prefix_len;
            if mark {
                json!({
                    "role": role_to_str(&m.role),
                    "content": [{"type": "text", "text": m.content, "cache_control": {"type": "ephemeral"}}],
                })
            } else {
                json!({
                    "role": role_to_str(&m.role),
                    "content": m.content,
                })
            }
        })
        .collect();

    let mut body = json!({
        "model": opts.model,
        "messages": msg_array,
        "stream": true,
        "max_tokens": opts.max_tokens.unwrap_or(1024),
    });

    // System prompt: content-block array with cache_control when caching is enabled.
    if let Some(sys) = system_content {
        if cache {
            body["system"] =
                json!([{"type": "text", "text": sys, "cache_control": {"type": "ephemeral"}}]);
        } else {
            body["system"] = json!(sys);
        }
    }

    // Tools: mark the last definition with cache_control when caching is enabled.
    if let Some(tools) = &opts.tools {
        if !tools.is_empty() {
            if cache {
                let mut tools_with_cache: Vec<serde_json::Value> = tools.clone();
                let last = tools_with_cache.len() - 1;
                if let Some(obj) = tools_with_cache[last].as_object_mut() {
                    obj.insert("cache_control".to_owned(), json!({"type": "ephemeral"}));
                }
                body["tools"] = serde_json::Value::Array(tools_with_cache);
            } else {
                body["tools"] = serde_json::Value::Array(tools.clone());
            }
        }
    }

    if let Some(temp) = opts.temperature {
        body["temperature"] = json!(temp);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_opts() -> CallOptions {
        CallOptions {
            model: "claude-opus-4-5".into(),
            max_tokens: Some(512),
            temperature: None,
            system: None,
            tools: None,
            provider_session_id: None,
            smedja_session_id: None,
            permission_mode: None,
            stable_prefix_len: None,
            cache_strategy: crate::types::CacheStrategy::None,
            workspace: None,
        }
    }

    #[test]
    fn build_body_omits_tools_key_when_tools_is_none() {
        let opts = base_opts();
        let body = build_body(&[], &opts);
        assert!(
            body.get("tools").is_none(),
            "tools key must be absent when CallOptions::tools is None"
        );
    }

    #[test]
    fn build_body_omits_tools_key_when_tools_vec_is_empty() {
        let mut opts = base_opts();
        opts.tools = Some(vec![]);
        let body = build_body(&[], &opts);
        assert!(
            body.get("tools").is_none(),
            "tools key must be absent when the tools Vec is empty"
        );
    }

    #[test]
    fn build_body_injects_tools_when_non_empty() {
        let tool = serde_json::json!({
            "name": "bash",
            "description": "Run a bash command",
            "input_schema": { "type": "object" }
        });
        let mut opts = base_opts();
        opts.tools = Some(vec![tool.clone()]);

        let body = build_body(&[], &opts);
        let injected = body
            .get("tools")
            .expect("tools key must be present when tools Vec is non-empty");
        let arr = injected.as_array().expect("tools value must be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], tool);
    }

    #[test]
    fn build_body_preserves_tool_order() {
        let tool_a = serde_json::json!({ "name": "a" });
        let tool_b = serde_json::json!({ "name": "b" });
        let mut opts = base_opts();
        opts.tools = Some(vec![tool_a.clone(), tool_b.clone()]);

        let body = build_body(&[], &opts);
        let arr = body["tools"].as_array().unwrap();
        assert_eq!(arr[0], tool_a);
        assert_eq!(arr[1], tool_b);
    }

    // ── cache hint tests ──────────────────────────────────────────────────────

    fn cache_opts() -> CallOptions {
        CallOptions {
            stable_prefix_len: Some(0),
            system: Some("You are a test assistant.".to_owned()),
            ..base_opts()
        }
    }

    #[test]
    fn build_body_without_cache_hint_keeps_system_as_string() {
        let mut opts = base_opts();
        opts.system = Some("sys".to_owned());
        let body = build_body(&[], &opts);
        assert_eq!(body["system"], serde_json::json!("sys"));
    }

    #[test]
    fn build_body_with_cache_hint_formats_system_as_content_block() {
        let opts = cache_opts();
        let body = build_body(&[], &opts);
        let sys = body["system"]
            .as_array()
            .expect("system must be an array when caching");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_body_with_cache_hint_marks_last_tool() {
        let tool_a = serde_json::json!({ "name": "tool_a" });
        let tool_b = serde_json::json!({ "name": "tool_b" });
        let mut opts = cache_opts();
        opts.tools = Some(vec![tool_a, tool_b]);
        let body = build_body(&[], &opts);
        let arr = body["tools"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(
            arr[0].get("cache_control").is_none(),
            "first tool must not be marked"
        );
        assert_eq!(
            arr[1]["cache_control"]["type"], "ephemeral",
            "last tool must be marked"
        );
    }

    #[test]
    fn build_body_with_cache_hint_marks_message_at_prefix_boundary() {
        let mut opts = cache_opts();
        opts.stable_prefix_len = Some(1); // cache message at index 0
        let messages = vec![
            Message {
                role: Role::User,
                content: "initial context".to_owned(),
            },
            Message {
                role: Role::Assistant,
                content: "ok".to_owned(),
            },
        ];
        let body = build_body(&messages, &opts);
        let arr = body["messages"].as_array().unwrap();
        // Message at index 0 should be a content-block array with cache_control.
        assert!(
            arr[0]["content"].is_array(),
            "message at prefix boundary must use content-block format"
        );
        assert_eq!(arr[0]["content"][0]["cache_control"]["type"], "ephemeral");
        // Message at index 1 should be a plain string.
        assert!(
            arr[1]["content"].is_string(),
            "message after prefix boundary must be a plain string"
        );
    }

    #[test]
    fn build_body_with_prefix_zero_does_not_mark_any_message() {
        let mut opts = cache_opts();
        opts.stable_prefix_len = Some(0); // only system/tools cached
        let messages = vec![Message {
            role: Role::User,
            content: "hello".to_owned(),
        }];
        let body = build_body(&messages, &opts);
        let arr = body["messages"].as_array().unwrap();
        assert!(
            arr[0]["content"].is_string(),
            "no message should be marked when prefix_len == 0"
        );
    }
}
