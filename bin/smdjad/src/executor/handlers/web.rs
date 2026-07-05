//! `fetch_web` tool body plus its HTML-to-text helpers.

use serde_json::Value;

/// Fetches `url` over HTTP with SSRF vetting and returns the body as text.
///
/// `Err` is returned for every guard/failure that the original arm exited via
/// `return` (bypassing the output scan); `Ok` carries the fetched body.
pub(crate) async fn fetch_web(input: &Value) -> Result<String, String> {
    use futures_util::StreamExt as _;

    let url_str = match input.get("url").and_then(Value::as_str) {
        Some(u) => u.to_owned(),
        None => return Err("error: url field required".into()),
    };
    let max_bytes: usize = input
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(256 * 1024)
        .try_into()
        .unwrap_or(usize::MAX);

    if !crate::sandbox::NetworkPolicy::from_env().permits_public_egress() {
        return Err("error: network access disabled by sandbox policy".into());
    }
    if !crate::is_safe_mcp_url(&url_str) {
        return Err("error: URL blocked by SSRF policy".into());
    }

    // DNS-level SSRF: resolve once, vet every returned address, and PIN
    // the vetted set so the HTTP client cannot re-resolve the hostname to
    // a different (private/IMDS) address between this check and the
    // connect (DNS-rebind TOCTOU).
    let Ok(parsed) = url_str.parse::<url::Url>() else {
        return Err("error: invalid URL".into());
    };
    let host = parsed.host_str().unwrap_or("").to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let mut vetted: Vec<std::net::SocketAddr> = Vec::new();
    match tokio::net::lookup_host(format!("{host}:{port}")).await {
        Ok(addrs) => {
            for addr in addrs {
                if crate::is_blocked_ip(addr.ip()) {
                    return Err(
                        "error: URL blocked by SSRF policy (resolved to private address)".into(),
                    );
                }
                vetted.push(addr);
            }
        }
        Err(e) => return Err(format!("error: DNS resolution failed: {e}")),
    }
    if vetted.is_empty() {
        return Err("error: URL blocked by SSRF policy (no addresses resolved)".into());
    }

    // Pin the vetted addresses for this host and refuse redirects: a
    // `302 -> http://169.254.169.254/…` (or any cross-host hop) is NOT
    // followed, so a redirect cannot bounce the request to an internal
    // address that was never vetted. Propagate the builder error instead
    // of silently dropping the timeout via `unwrap_or_default`.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&host, &vetted)
        .build()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("error: failed to build HTTP client: {e}")),
    };
    let resp = match client.get(&url_str).send().await {
        Ok(r) => r,
        Err(e) => return Err(format!("error: {e}")),
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
        return Err(format!("error: unsupported content type '{ct}'"));
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
            Err(e) => return Err(format!("error: {e}")),
        }
    }

    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut out = if is_html { strip_html(&text) } else { text };
    if truncated {
        out.push_str("\n[truncated]");
    }
    Ok(out)
}

/// Removes `<script>` and `<style>` block content from HTML, then strips all
/// remaining tags to plain text, and decodes common HTML entities.
pub(crate) fn strip_html(html: &str) -> String {
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
