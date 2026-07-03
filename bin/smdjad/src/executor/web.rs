//! The `fetch_web` tool: SSRF-guarded HTTP fetch with size limits, plus the HTML
//! to plain-text conversion applied to `text/html` responses.

use serde_json::Value;

/// Executes the `fetch_web` tool: fetches `url` under the sandbox network policy
/// and SSRF guards, capping the body at `max_bytes` and stripping HTML to text.
pub(crate) async fn fetch_web(input: &Value) -> String {
    use futures_util::StreamExt as _;

    let url_str = match input.get("url").and_then(Value::as_str) {
        Some(u) => u.to_owned(),
        None => return "error: url field required".into(),
    };
    let max_bytes: usize = input
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(256 * 1024)
        .try_into()
        .unwrap_or(usize::MAX);

    if !crate::sandbox::NetworkPolicy::from_env().permits_public_egress() {
        return "error: network access disabled by sandbox policy".into();
    }
    if !crate::is_safe_mcp_url(&url_str) {
        return "error: URL blocked by SSRF policy".into();
    }

    // DNS-level SSRF: resolve and check every returned address.
    let Ok(parsed) = url_str.parse::<url::Url>() else {
        return "error: invalid URL".into();
    };
    let host = parsed.host_str().unwrap_or("").to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    match tokio::net::lookup_host(format!("{host}:{port}")).await {
        Ok(addrs) => {
            for addr in addrs {
                if crate::is_blocked_ip(addr.ip()) {
                    return "error: URL blocked by SSRF policy (resolved to private address)"
                        .into();
                }
            }
        }
        Err(e) => return format!("error: DNS resolution failed: {e}"),
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let resp = match client.get(&url_str).send().await {
        Ok(r) => r,
        Err(e) => return format!("error: {e}"),
    };

    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_html = ct.contains("text/html");
    if !ct.starts_with("text/")
        && !ct.contains("application/json")
        && !ct.contains("application/xml")
    {
        return format!("error: unsupported content type '{ct}'");
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(256 * 1024));
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining = max_bytes.saturating_sub(buf.len());
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let take = bytes.len().min(remaining);
                buf.extend_from_slice(&bytes[..take]);
                if take < bytes.len() {
                    truncated = true;
                    break;
                }
            }
            Err(e) => return format!("error: {e}"),
        }
    }

    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut out = if is_html { strip_html(&text) } else { text };
    if truncated {
        out.push_str("\n[truncated]");
    }
    out
}

/// Removes `<script>` and `<style>` block content from HTML, then strips all
/// remaining tags to plain text, and decodes common HTML entities.
fn strip_html(html: &str) -> String {
    let mut s = remove_html_block(html, "<script", "</script>");
    s = remove_html_block(&s, "<style", "</style>");
    let mut out = String::with_capacity(s.len());
    let mut depth: usize = 0;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => {
                depth -= 1;
                out.push(' ');
            }
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut result = String::with_capacity(out.len());
    let mut prev_ws = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                result.push('\n');
            }
            prev_ws = true;
        } else {
            result.push(ch);
            prev_ws = false;
        }
    }
    result.trim().to_owned()
}

fn remove_html_block(s: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = s.to_owned();
    loop {
        let lower = result.to_ascii_lowercase();
        let Some(start) = lower.find(open_tag) else {
            break;
        };
        let after = start + open_tag.len();
        let end = lower[after..]
            .find(close_tag)
            .map_or(result.len(), |p| after + p + close_tag.len());
        result.drain(start..end);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::strip_html;
    use crate::executor::execute_tool;
    use crate::executor::output_filter::ENV_LOCK;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    #[test]
    fn strip_html_removes_tags() {
        let result = strip_html("<p>Hello <b>world</b></p>");
        assert!(result.contains("Hello"), "text must be preserved: {result}");
        assert!(result.contains("world"), "text must be preserved: {result}");
        assert!(!result.contains('<'), "tags must be stripped: {result}");
    }

    #[test]
    fn strip_html_removes_script_block() {
        let html = "<head><script>alert(1)</script></head><body>Keep</body>";
        let result = strip_html(html);
        assert!(
            !result.contains("alert"),
            "script content must be removed: {result}"
        );
        assert!(result.contains("Keep"), "body text must be kept: {result}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fetch_web_policy_none_blocks() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("SMEDJA_SANDBOX_NETWORK");
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let result = execute_tool(
            "fetch_web",
            r#"{"url":"https://example.com"}"#,
            ws.path(),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            result.starts_with("error:"),
            "NetworkPolicy::None must block fetch_web: {result}"
        );
        assert!(
            result.contains("network access disabled"),
            "error must name the policy reason: {result}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fetch_web_ssrf_loopback_blocked() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("SMEDJA_SANDBOX_NETWORK", "open");
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let result = execute_tool(
            "fetch_web",
            r#"{"url":"http://127.0.0.1/"}"#,
            ws.path(),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        std::env::remove_var("SMEDJA_SANDBOX_NETWORK");
        assert!(
            result.starts_with("error:"),
            "loopback IP must be blocked by SSRF policy: {result}"
        );
        assert!(result.contains("SSRF"), "error must mention SSRF: {result}");
    }
}
