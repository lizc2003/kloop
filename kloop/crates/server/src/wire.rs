//! Wire envelopes, after codex app-server's rpc.rs: JSON-RPC 2.0 in shape
//! but without the `"jsonrpc"` field. One JSON object per line, both ways.
//!
//! Inbound lines are distinguished structurally: a `method` field makes it a
//! request (client → server); an `id` without `method` is the client's
//! response to a server-initiated request (approvals). Server request ids
//! live in their own `srv-{n}` namespace so they can never collide with
//! client-chosen ids.

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

/// JSON-RPC request ids may be numbers or strings; both are preserved
/// verbatim so responses match whatever the client sent.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(i64),
    Str(String),
}

/// Any inbound line, before interpretation.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    pub id: Option<RequestId>,
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub result: Value,
}

/// Every outbound line.
#[derive(Debug)]
pub enum Outgoing {
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        id: Option<RequestId>,
        code: i64,
        message: String,
    },
    Notification {
        method: &'static str,
        params: Value,
    },
    /// Server-initiated request (approvals); the client answers with a
    /// response carrying the same id.
    ServerRequest {
        id: RequestId,
        method: &'static str,
        params: Value,
    },
}

pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const SERVER_ERROR: i64 = -32000;

impl Outgoing {
    pub fn to_json(&self) -> Value {
        match self {
            Outgoing::Response { id, result } => json!({"id": id, "result": result}),
            Outgoing::Error { id, code, message } => {
                json!({"id": id, "error": {"code": code, "message": message}})
            }
            Outgoing::Notification { method, params } => {
                json!({"method": method, "params": params})
            }
            Outgoing::ServerRequest { id, method, params } => {
                json!({"id": id, "method": method, "params": params})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_round_trip_both_shapes() {
        assert_eq!(
            serde_json::from_str::<RequestId>("7").unwrap(),
            RequestId::Num(7)
        );
        assert_eq!(
            serde_json::from_str::<RequestId>("\"srv-1\"").unwrap(),
            RequestId::Str("srv-1".into())
        );
        assert_eq!(serde_json::to_string(&RequestId::Num(7)).unwrap(), "7");
        assert_eq!(
            serde_json::to_string(&RequestId::Str("srv-1".into())).unwrap(),
            "\"srv-1\""
        );
    }

    #[test]
    fn incoming_distinguishes_requests_from_responses() {
        let req: Incoming =
            serde_json::from_str(r#"{"id":1,"method":"thread/start","params":{}}"#).unwrap();
        assert_eq!(req.method.as_deref(), Some("thread/start"));
        assert_eq!(req.id, Some(RequestId::Num(1)));

        let resp: Incoming =
            serde_json::from_str(r#"{"id":"srv-1","result":{"decision":"allow"}}"#).unwrap();
        assert!(resp.method.is_none());
        assert_eq!(resp.id, Some(RequestId::Str("srv-1".into())));
        assert_eq!(resp.result["decision"], "allow");

        // Params may be absent entirely.
        let bare: Incoming = serde_json::from_str(r#"{"id":2,"method":"thread/list"}"#).unwrap();
        assert!(bare.params.is_null());
    }

    #[test]
    fn outgoing_shapes_match_the_wire_contract() {
        let cases = [
            (
                Outgoing::Response {
                    id: RequestId::Num(1),
                    result: json!({"threadId": "t"}),
                },
                r#"{"id":1,"result":{"threadId":"t"}}"#,
            ),
            (
                Outgoing::Error {
                    id: None,
                    code: PARSE_ERROR,
                    message: "bad line".into(),
                },
                r#"{"error":{"code":-32700,"message":"bad line"},"id":null}"#,
            ),
            (
                Outgoing::Notification {
                    method: "text/delta",
                    params: json!({"threadId": "t", "text": "hi"}),
                },
                r#"{"method":"text/delta","params":{"text":"hi","threadId":"t"}}"#,
            ),
            (
                Outgoing::ServerRequest {
                    id: RequestId::Str("srv-1".into()),
                    method: "approval/request",
                    params: json!({"threadId": "t", "description": "bash: rm x"}),
                },
                r#"{"id":"srv-1","method":"approval/request","params":{"description":"bash: rm x","threadId":"t"}}"#,
            ),
        ];
        for (outgoing, expected) in cases {
            assert_eq!(
                serde_json::to_string(&outgoing.to_json()).unwrap(),
                expected
            );
        }
    }
}
