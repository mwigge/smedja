//! Desktop notifications (OSC 9 / OSC 777) and OSC 7 URI parsing.

/// Desktop notification from OSC 9 or OSC 777.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// Parse OSC 9 payload: `OSC 9 ; <message> ST`
///
/// The payload is the raw message string; returns a notification with a
/// fixed title of `"smedja"` and the payload as the body.
#[must_use]
pub fn parse_osc9(payload: &str) -> Option<Notification> {
    Some(Notification {
        title: "smedja".into(),
        body: payload.to_owned(),
    })
}

/// Parse OSC 777 payload: `OSC 777 ; notify ; <title> ; <body> ST`
///
/// Expects the keyword `notify` as the first segment, then title and body.
/// Returns `None` for any other format.
#[must_use]
pub fn parse_osc777(payload: &str) -> Option<Notification> {
    let parts: Vec<&str> = payload.splitn(3, ';').collect();
    if parts.first().copied() == Some("notify") && parts.len() == 3 {
        Some(Notification {
            title: parts[1].trim().to_owned(),
            body: parts[2].trim().to_owned(),
        })
    } else {
        None
    }
}

/// Parse an OSC 7 URI (`file://hostname/path` or `file:///path`) into a path string.
///
/// Returns `None` if the URI does not start with `file://`.
#[must_use]
pub fn parse_osc7_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` → hostname is empty, rest starts with `/path`
    // `file://host/path` → skip to the first `/`
    let path = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        rest.find('/').map(|i| rest[i..].to_owned())?
    };
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osc9_returns_notification_with_payload_as_body() {
        let n = parse_osc9("hello from shell").unwrap();
        assert_eq!(n.title, "smedja");
        assert_eq!(n.body, "hello from shell");
    }

    #[test]
    fn parse_osc777_valid_payload_extracts_title_and_body() {
        let n = parse_osc777("notify;My App;Something happened").unwrap();
        assert_eq!(n.title, "My App");
        assert_eq!(n.body, "Something happened");
    }

    #[test]
    fn parse_osc777_invalid_payload_returns_none() {
        assert!(parse_osc777("toast;oops").is_none());
        assert!(parse_osc777("").is_none());
        assert!(parse_osc777("notify;only-title").is_none());
    }

    #[test]
    fn parse_osc7_uri_localhost_triple_slash() {
        let path = parse_osc7_uri("file:///home/user/project").unwrap();
        assert_eq!(path, "/home/user/project");
    }

    #[test]
    fn parse_osc7_uri_with_hostname() {
        let path = parse_osc7_uri("file://myhost/home/user/project").unwrap();
        assert_eq!(path, "/home/user/project");
    }

    #[test]
    fn parse_osc7_uri_non_file_scheme_returns_none() {
        assert!(parse_osc7_uri("http://example.com/path").is_none());
        assert!(parse_osc7_uri("").is_none());
    }
}
