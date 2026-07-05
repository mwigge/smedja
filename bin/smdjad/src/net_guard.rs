//! SSRF / URL-safety guards shared crate-wide.
//!
//! [`is_safe_mcp_url`] is the URL-level gate for outbound MCP/web fetches; it
//! defers to [`is_blocked_ip`], the address-range floor the sandbox network
//! policies also reuse.

/// Returns `true` when `addr` falls in a range the daemon must never reach
/// (SSRF defence): loopback, unspecified, RFC-1918 private, link-local
/// (incl. the cloud IMDS endpoint), CGNAT, IPv6 ULA, and IPv6 link-local.
///
/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are unwrapped to their embedded
/// IPv4 first, so a mapped private address cannot bypass the IPv4 rules.
pub(crate) fn is_blocked_ip(addr: std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    // Unwrap IPv4-mapped IPv6 so the IPv4 range checks apply.
    let addr = match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(v4) => IpAddr::V4(v4),
    };

    if addr.is_loopback() || addr.is_unspecified() {
        return true;
    }

    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || v4.is_link_local() // 169.254.0.0/16 (incl. IMDS 169.254.169.254)
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            (seg[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// Returns `true` only for publicly routable HTTP/HTTPS URLs.
///
/// Rejects non-HTTP schemes, the `localhost` hostname, and any host that parses
/// to an IP address blocked by [`is_blocked_ip`]. Hostnames that do not parse as
/// an IP are allowed (DNS resolution is the caller's network policy).
pub(crate) fn is_safe_mcp_url(url: &str) -> bool {
    let Ok(parsed) = url.parse::<url::Url>() else {
        return false;
    };
    if !matches!(parsed.scheme(), "https" | "http") {
        return false;
    }
    let host = parsed.host_str().unwrap_or("");
    if host == "localhost" {
        return false;
    }
    // IPv6 literals come bracketed from the URL host (e.g. "[::1]"); strip the
    // brackets so the address parses.
    let host_ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(addr) = host_ip.parse::<std::net::IpAddr>() {
        if is_blocked_ip(addr) {
            return false;
        }
    }
    true
}
