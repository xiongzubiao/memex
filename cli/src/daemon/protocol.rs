//! IPC protocol types. Newline-delimited JSON over Unix socket.

use serde::{Deserialize, Serialize};

/// Incoming request. `op` discriminates the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping {
        #[serde(default = "default_version")]
        v: u32,
    },
    Query {
        #[serde(default = "default_version")]
        v: u32,
        question: String,
        #[serde(default)]
        raw: bool,
        #[serde(default = "default_top_k")]
        top_k: usize,
        memex_root: String,
    },
}

fn default_version() -> u32 {
    1
}
fn default_top_k() -> usize {
    5
}

/// Outgoing message on the response stream. Multiple events may be sent
/// per request (progress, final answer, done).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Pong {
        pid: u32,
        started_at: String,
    },
    Queued {
        ahead: usize,
    },
    Answer {
        text: String,
        citations: Vec<String>,
    },
    /// Emitted by the synthesis path when signal is weak and the agent
    /// generated expansion terms. Comes BEFORE the final Answer event.
    Expansion {
        lex: String,
        vec: String,
        hyde: String,
    },
    Context {
        pages: Vec<serde_json::Value>,
    },
    Error {
        code: String,
        message: String,
        status: i32,
    },
    Done {
        status: i32,
    },
}

pub const SUPPORTED_VERSIONS: &[u32] = &[1];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_request_deserializes() {
        let r: Request = serde_json::from_str(r#"{"op":"ping","v":1}"#).unwrap();
        assert!(matches!(r, Request::Ping { v: 1 }));
    }

    #[test]
    fn ping_request_without_v_uses_default() {
        let r: Request = serde_json::from_str(r#"{"op":"ping"}"#).unwrap();
        assert!(matches!(r, Request::Ping { v: 1 }));
    }

    #[test]
    fn query_request_deserializes_with_defaults() {
        let r: Request =
            serde_json::from_str(r#"{"op":"query","question":"q","memex_root":"/x"}"#).unwrap();
        match r {
            Request::Query {
                v,
                question,
                raw,
                top_k,
                memex_root,
            } => {
                assert_eq!(v, 1);
                assert_eq!(question, "q");
                assert!(!raw);
                assert_eq!(top_k, 5);
                assert_eq!(memex_root, "/x");
            }
            _ => panic!("expected Query"),
        }
    }

    #[test]
    fn pong_event_serializes() {
        let e = Event::Pong {
            pid: 42,
            started_at: "2026-04-17T00:00:00Z".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"pong""#));
        assert!(s.contains(r#""pid":42"#));
    }

    #[test]
    fn error_event_serializes() {
        let e = Event::Error {
            code: "bad_request".into(),
            message: "malformed".into(),
            status: 1,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"error""#));
        assert!(s.contains(r#""code":"bad_request""#));
        assert!(s.contains(r#""status":1"#));
    }

    #[test]
    fn unknown_op_is_rejected() {
        let err = serde_json::from_str::<Request>(r#"{"op":"bogus"}"#).unwrap_err();
        assert!(err.to_string().contains("bogus") || err.to_string().contains("variant"));
    }

    #[test]
    fn expansion_event_serializes() {
        let e = Event::Expansion {
            lex: "deployment".into(),
            vec: "rollout schedule".into(),
            hyde: "We rolled out to production on 2026-04-16.".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"expansion""#));
        assert!(s.contains(r#""lex":"deployment""#));
    }
}
