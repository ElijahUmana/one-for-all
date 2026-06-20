//! Error taxonomy for the MCP server. Codes match SPEC §D17.

use serde::Serialize;
use thiserror::Error;

/// JSON-RPC error codes — locked per SPEC §D17. The full table lives here as
/// public constants so any consumer (broker, tests, doctor) can pattern-match
/// without parsing message strings. Some are unused locally; they're public
/// API.
#[allow(dead_code)]
pub mod jsonrpc_code {
    // Standard JSON-RPC.
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// MCP cancellation convention.
    pub const REQUEST_CANCELLED: i64 = -32800;
    // SPEC §D17 server-error band -32001..-32012.
    pub const SESSION_NOT_FOUND: i64 = -32001;
    pub const TAB_NOT_FOUND: i64 = -32002;
    pub const CONTEXT_NOT_FOUND: i64 = -32003;
    pub const ELEMENT_STALE: i64 = -32004;
    pub const ELEMENT_NOT_ACTIONABLE: i64 = -32005;
    pub const NAVIGATION_FAILED: i64 = -32006;
    pub const TIMEOUT: i64 = -32007;
    pub const CHROMIUM_LAUNCH_FAILED: i64 = -32008;
    pub const PERMISSION_DENIED: i64 = -32009;
    pub const PROTOCOL_ERROR: i64 = -32010;
    pub const BROKER_UNAVAILABLE: i64 = -32011;
    pub const SESSION_LIMIT_EXCEEDED: i64 = -32012;
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("broker unavailable: {0}")]
    BrokerUnavailable(String),
    #[error("broker error {code}: {message}")]
    BrokerError {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },
    #[error("request cancelled")]
    Cancelled,
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("invalid params for {method}: {reason}")]
    InvalidParams {
        method: &'static str,
        reason: String,
    },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<serde_json::Error> for BridgeError {
    fn from(e: serde_json::Error) -> Self {
        BridgeError::InvalidParams {
            method: "<unknown>",
            reason: e.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

pub fn to_jsonrpc_error(e: &BridgeError) -> JsonRpcError {
    match e {
        BridgeError::BrokerUnavailable(msg) => JsonRpcError {
            code: jsonrpc_code::BROKER_UNAVAILABLE,
            message: format!("BrokerUnavailable: {msg}"),
            data: None,
        },
        BridgeError::BrokerError {
            code,
            message,
            data,
        } => JsonRpcError {
            code: *code,
            message: message.clone(),
            data: data.clone(),
        },
        BridgeError::Cancelled => JsonRpcError {
            code: jsonrpc_code::REQUEST_CANCELLED,
            message: "request cancelled".into(),
            data: None,
        },
        BridgeError::Timeout(d) => JsonRpcError {
            code: jsonrpc_code::TIMEOUT,
            message: format!("Timeout after {d:?}"),
            data: Some(serde_json::json!({ "millis": d.as_millis() })),
        },
        BridgeError::InvalidParams { method, reason } => JsonRpcError {
            code: jsonrpc_code::INVALID_PARAMS,
            message: format!("invalid params for {method}: {reason}"),
            data: Some(serde_json::json!({ "method": method })),
        },
        BridgeError::Protocol(msg) => JsonRpcError {
            code: jsonrpc_code::PROTOCOL_ERROR,
            message: format!("ProtocolError: {msg}"),
            data: None,
        },
        BridgeError::Internal(msg) => JsonRpcError {
            code: jsonrpc_code::INTERNAL_ERROR,
            message: msg.clone(),
            data: None,
        },
    }
}
