//! Task 51 — `SmartCrusher`: JSON null and empty-array field stripping.

use super::bypass_enabled;

/// Strips JSON null and empty-array fields recursively from a serialised JSON string.
///
/// Non-JSON input is returned unchanged.  Honouring `SMEDJA_NO_TOOL_COMPRESS=1`
/// bypasses all processing and returns the content as-is.
#[must_use]
pub fn compress_tool_result(content: &str) -> String {
    if bypass_enabled() {
        return content.to_owned();
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return content.to_owned();
    };

    let stripped = strip_nulls_and_empty_arrays(value);
    serde_json::to_string(&stripped).unwrap_or_else(|_| content.to_owned())
}

/// Recursively removes all JSON null and empty-array fields from an object or array.
fn strip_nulls_and_empty_arrays(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let filtered = map
                .into_iter()
                .filter(|(_, v)| {
                    !v.is_null() && !matches!(v, serde_json::Value::Array(arr) if arr.is_empty())
                })
                .map(|(k, v)| (k, strip_nulls_and_empty_arrays(v)))
                .collect();
            serde_json::Value::Object(filtered)
        }
        serde_json::Value::Array(arr) => {
            let filtered = arr.into_iter().map(strip_nulls_and_empty_arrays).collect();
            serde_json::Value::Array(filtered)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_top_level_null_fields() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = r#"{"a":1,"b":null,"c":"hello"}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("b").is_none(), "null field 'b' must be removed");
        assert_eq!(v["a"], 1);
        assert_eq!(v["c"], "hello");
    }

    #[test]
    fn strips_nested_null_fields() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = r#"{"outer":{"x":null,"y":42},"arr":[{"z":null,"w":1}]}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v["outer"].get("x").is_none());
        assert_eq!(v["outer"]["y"], 42);
        assert!(v["arr"][0].get("z").is_none());
        assert_eq!(v["arr"][0]["w"], 1);
    }

    #[test]
    fn strips_empty_array_fields() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = r#"{"keep":1,"drop":[],"nested":{"also_drop":[],"keep":"value"}}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("drop").is_none(), "empty array field must be removed");
        assert!(v["nested"].get("also_drop").is_none());
        assert_eq!(v["keep"], 1);
        assert_eq!(v["nested"]["keep"], "value");
    }

    #[test]
    fn non_json_input_returned_unchanged() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = "not json at all";
        let output = compress_tool_result(input);
        assert_eq!(output, input);
    }

    #[test]
    fn bypass_env_skips_compression() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let input = r#"{"a":null,"b":1}"#;
        let output = compress_tool_result(input);
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        // Must be returned verbatim — nulls still present.
        assert_eq!(output, input);
    }
}
