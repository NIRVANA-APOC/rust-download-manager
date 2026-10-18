//! Minimal JSON-RPC 2.0 / MCP framing for the stdio transport
//! (`docs/agent-integration.md` §3): one JSON object per line.

use serde_json::{json, Value};

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC + MCP error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// MCP: resource/prompt/task not found, and "server not initialized".
pub const RESOURCE_NOT_FOUND: i64 = -32002;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }

    pub fn not_initialized() -> Self {
        Self::new(
            RESOURCE_NOT_FOUND,
            "server not initialized: send initialize first",
        )
    }

    pub fn resource_not_found(uri: &str) -> Self {
        Self::new(RESOURCE_NOT_FOUND, format!("resource not found: {uri}"))
    }
}

/// One parsed inbound message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    Notification { method: String, params: Value },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// Parse one newline-delimited JSON-RPC message.
pub fn parse_message(line: &str) -> Result<Message, RpcError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(RpcError::parse("empty message"));
    }
    let value: Value = serde_json::from_str(line)
        .map_err(|error| RpcError::parse(format!("invalid JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| RpcError::invalid_request("JSON-RPC message must be an object"))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(RpcError::invalid_request(
            "missing or unsupported `jsonrpc` version (expected \"2.0\")",
        ));
    }
    if object.contains_key("result") || object.contains_key("error") {
        return Err(RpcError::invalid_request(
            "unexpected response message on a server transport",
        ));
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| RpcError::invalid_request("missing or invalid `method`"))?
        .to_string();
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match object.get("id") {
        Some(id) => Ok(Message::Request(Request {
            id: id.clone(),
            method,
            params,
        })),
        None => Ok(Message::Notification { method, params }),
    }
}

/// Pick the protocol version to answer `initialize` with.
///
/// Contract: prefer `2025-06-18`, echo any supported client version
/// (`2024-11-05` / `2025-03-26` / `2025-06-18`), otherwise answer with the
/// latest version this server implements (the client decides whether to
/// continue).
pub fn negotiate_protocol_version(client_version: &str) -> &'static str {
    let supported = rdm_agent::PROTOCOL_VERSIONS;
    for version in supported {
        if *version == client_version {
            return version;
        }
    }
    rdm_agent::LATEST_PROTOCOL_VERSION
}

pub fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result })
}

pub fn error_response(id: Option<Value>, error: &RpcError) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".into(), json!(JSONRPC_VERSION));
    object.insert("id".into(), id.unwrap_or(Value::Null));
    let mut error_object = json!({
        "code": error.code,
        "message": error.message,
    });
    if let Some(data) = &error.data {
        error_object["data"] = data.clone();
    }
    object.insert("error".into(), error_object);
    Value::Object(object)
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": JSONRPC_VERSION, "method": method, "params": params })
}

/// Stable string key for a JSON-RPC id (used to correlate
/// `notifications/cancelled` with an in-flight request).
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => format!("s:{text}"),
        other => format!("j:{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_notifications() {
        let request = parse_message(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#).unwrap();
        assert_eq!(
            request,
            Message::Request(Request {
                id: json!(7),
                method: "ping".to_string(),
                params: Value::Null
            })
        );
        let note =
            parse_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#)
                .unwrap();
        assert!(
            matches!(note, Message::Notification { method, .. } if method == "notifications/initialized")
        );
        // String ids are preserved verbatim.
        let request =
            parse_message(r#"{"jsonrpc":"2.0","id":"a-1","method":"tools/list","params":{}}"#)
                .unwrap();
        match request {
            Message::Request(request) => assert_eq!(request.id, json!("a-1")),
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn rejects_malformed_messages() {
        assert_eq!(parse_message("not json").unwrap_err().code, PARSE_ERROR);
        assert_eq!(
            parse_message(r#"{"id":1,"method":"x"}"#).unwrap_err().code,
            INVALID_REQUEST
        );
        assert_eq!(
            parse_message(r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#)
                .unwrap_err()
                .code,
            INVALID_REQUEST
        );
        assert_eq!(
            parse_message(r#"{"jsonrpc":"2.0","id":1}"#)
                .unwrap_err()
                .code,
            INVALID_REQUEST
        );
        assert_eq!(
            parse_message(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
                .unwrap_err()
                .code,
            INVALID_REQUEST
        );
        assert_eq!(parse_message("").unwrap_err().code, PARSE_ERROR);
    }

    #[test]
    fn negotiates_protocol_versions() {
        for version in rdm_agent::PROTOCOL_VERSIONS {
            assert_eq!(negotiate_protocol_version(version), *version);
        }
        assert_eq!(
            negotiate_protocol_version("2024-10-07"),
            rdm_agent::LATEST_PROTOCOL_VERSION
        );
        assert_eq!(
            negotiate_protocol_version("2099-01-01"),
            rdm_agent::LATEST_PROTOCOL_VERSION
        );
    }

    #[test]
    fn builds_responses_and_notifications() {
        let response = success_response(json!(1), json!({"ok": true}));
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["ok"], true);
        let error = error_response(Some(json!("x")), &RpcError::method_not_found("nope"));
        assert_eq!(error["error"]["code"], METHOD_NOT_FOUND);
        let error = error_response(None, &RpcError::parse("bad"));
        assert_eq!(error["id"], Value::Null);
        let note = notification("notifications/progress", json!({"progress": 1}));
        assert_eq!(note["method"], "notifications/progress");
        assert!(note.get("id").is_none());
    }

    #[test]
    fn ids_get_stable_keys() {
        assert_eq!(id_key(&json!(1)), "j:1");
        assert_eq!(id_key(&json!("abc")), "s:abc");
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
    }
}
