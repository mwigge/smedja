/// Extract a structured JSON summary from raw context text.
/// If input starts with `{` or `[` (after trimming), parse as JSON and return Some; else None.
pub fn extract_json_structure(input: &str) -> Option<serde_json::Value> {
    let trimmed = input.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(trimmed).ok()
    } else {
        None
    }
}

/// Re-expand a compacted JSON structure back to pretty-printed JSON text.
pub fn expand(compact: &serde_json::Value) -> String {
    serde_json::to_string_pretty(compact).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_returns_some() {
        let input = r#"{"key": "value"}"#;
        let result = extract_json_structure(input);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["key"], "value");
    }

    #[test]
    fn json_array_returns_some() {
        let input = r#"[1, 2, 3]"#;
        let result = extract_json_structure(input);
        assert!(result.is_some());
    }

    #[test]
    fn non_json_returns_none() {
        let input = "Hello, this is plain text";
        assert!(extract_json_structure(input).is_none());
    }

    #[test]
    fn round_trip_expand() {
        let input = r#"{"a":1,"b":[1,2,3]}"#;
        let val = extract_json_structure(input).unwrap();
        let expanded = expand(&val);
        // Re-parsing the expanded form should produce the same value
        let reparsed: serde_json::Value = serde_json::from_str(&expanded).unwrap();
        assert_eq!(val, reparsed);
    }
}
