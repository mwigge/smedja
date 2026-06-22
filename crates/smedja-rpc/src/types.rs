use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("JSON-RPC error {code}: {message}")]
pub struct Error {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl Request {
    pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: crate::JSONRPC.into(),
            id: Some(id.into()),
            method: method.into(),
            params,
        }
    }

    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: crate::JSONRPC.into(),
            id: None,
            method: method.into(),
            params,
        }
    }

    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

impl Response {
    #[must_use]
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: crate::JSONRPC.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn err(id: Option<Value>, error: Error) -> Self {
        Self {
            jsonrpc: crate::JSONRPC.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_new_sets_jsonrpc_version() {
        let r = Request::new(1, "ping", json!({}));
        assert_eq!(r.jsonrpc, "2.0");
    }

    #[test]
    fn request_new_has_id() {
        let r = Request::new(42_i64, "ping", json!({}));
        assert_eq!(r.id, Some(json!(42)));
    }

    #[test]
    fn notification_has_no_id() {
        let r = Request::notification("session.end", json!({}));
        assert!(r.id.is_none());
        assert!(r.is_notification());
    }

    #[test]
    fn non_notification_is_not_notification() {
        let r = Request::new(1, "ping", json!({}));
        assert!(!r.is_notification());
    }

    #[test]
    fn response_ok_has_no_error() {
        let r = Response::ok(Some(json!(1)), json!("pong"));
        assert!(r.result.is_some());
        assert!(r.error.is_none());
    }

    #[test]
    fn response_err_has_no_result() {
        let e = Error::new(-32601, "method not found");
        let r = Response::err(Some(json!(1)), e);
        assert!(r.result.is_none());
        assert!(r.error.is_some());
    }

    #[test]
    fn error_display_includes_code_and_message() {
        let e = Error::new(-32601, "method not found");
        let s = format!("{e}");
        assert!(s.contains("-32601"));
        assert!(s.contains("method not found"));
    }

    #[test]
    fn request_roundtrip_serde() {
        let r = Request::new(7_i64, "turn.start", json!({"session": "abc"}));
        let json = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, "turn.start");
        assert_eq!(back.id, Some(json!(7)));
    }

    #[test]
    fn response_ok_omits_error_in_json() {
        let r = Response::ok(Some(json!(1)), json!("pong"));
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn response_err_omits_result_in_json() {
        let e = Error::new(-32601, "nope");
        let r = Response::err(Some(json!(1)), e);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"result\""));
    }
}
