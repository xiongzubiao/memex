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
        #[serde(default)]
        collections: Vec<String>,
        memex_root: String,
    },

    // --- Mutations (daemon is the single writer) ---
    Write {
        #[serde(default = "default_version")]
        v: u32,
        title: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        sources: Vec<String>,
        #[serde(default)]
        force: bool,
        memex_root: String,
    },
    Ingest {
        #[serde(default = "default_version")]
        v: u32,
        transcript_path: String,
        agent: String,
        #[serde(default)]
        collections: Vec<String>,
        memex_root: String,
    },
    Delete {
        #[serde(default = "default_version")]
        v: u32,
        slug: String,
        memex_root: String,
    },
    LintFix {
        #[serde(default = "default_version")]
        v: u32,
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
        #[serde(default)]
        job_id: String,
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
        entries: Vec<serde_json::Value>,
    },
    Error {
        code: String,
        message: String,
        status: i32,
    },
    Done {
        status: i32,
    },

    // --- Mutation responses ---
    Written {
        slug: String,
        docid: String,
    },
    Deleted {
        slug: String,
    },
    LintResult {
        fixed: u32,
        remaining: u32,
    },
    Parsing {
        job_id: String,
        transcript_path: String,
    },
    Distilling {
        job_id: String,
        transcript_path: String,
    },
    Stored {
        job_id: String,
        source_docid: String,
        wiki_pages: Vec<String>,
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
                collections,
                memex_root,
            } => {
                assert_eq!(v, 1);
                assert_eq!(question, "q");
                assert!(!raw);
                assert_eq!(top_k, 5);
                assert!(collections.is_empty());
                assert_eq!(memex_root, "/x");
            }
            _ => panic!("expected Query"),
        }
    }

    #[test]
    fn query_request_deserializes_with_collections() {
        let r: Request = serde_json::from_str(
            r#"{"op":"query","question":"q","memex_root":"/x","collections":["default","project-a"]}"#,
        )
        .unwrap();
        match r {
            Request::Query { collections, .. } => {
                assert_eq!(collections, vec!["default", "project-a"]);
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
    fn write_request_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"write","title":"Test","content":"body","memex_root":"/x"}"#,
        )
        .unwrap();
        match r {
            Request::Write {
                title,
                content,
                tags,
                sources,
                force,
                ..
            } => {
                assert_eq!(title, "Test");
                assert_eq!(content, "body");
                assert!(tags.is_empty());
                assert!(sources.is_empty());
                assert!(!force);
            }
            _ => panic!("expected Write"),
        }
    }

    #[test]
    fn ingest_request_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"ingest","transcript_path":"/tmp/s.jsonl","agent":"claude-code","memex_root":"/x"}"#,
        )
        .unwrap();
        match r {
            Request::Ingest {
                transcript_path,
                agent,
                collections,
                memex_root,
                ..
            } => {
                assert_eq!(transcript_path, "/tmp/s.jsonl");
                assert_eq!(agent, "claude-code");
                assert!(collections.is_empty());
                assert_eq!(memex_root, "/x");
            }
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn ingest_request_deserializes_with_collections() {
        let r: Request = serde_json::from_str(
            r#"{"op":"ingest","transcript_path":"/tmp/s.jsonl","agent":"claude-code","collections":["default","project-a"],"memex_root":"/x"}"#,
        )
        .unwrap();
        match r {
            Request::Ingest { collections, .. } => {
                assert_eq!(collections, vec!["default", "project-a"]);
            }
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn delete_request_deserializes() {
        let r: Request =
            serde_json::from_str(r#"{"op":"delete","slug":"my-page","memex_root":"/x"}"#).unwrap();
        assert!(matches!(r, Request::Delete { .. }));
    }

    #[test]
    fn lint_fix_request_deserializes() {
        let r: Request = serde_json::from_str(r#"{"op":"lint_fix","memex_root":"/x"}"#).unwrap();
        assert!(matches!(r, Request::LintFix { .. }));
    }

    #[test]
    fn written_event_serializes() {
        let e = Event::Written {
            slug: "my-page".into(),
            docid: "wiki-abc".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"written""#));
        assert!(s.contains(r#""slug":"my-page""#));
    }

    #[test]
    fn stored_event_serializes() {
        let e = Event::Stored {
            job_id: "job-1".into(),
            source_docid: "src-abc".into(),
            wiki_pages: vec!["auth-debugging".into(), "sqlite-config".into()],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"stored""#));
        assert!(s.contains(r#""job_id":"job-1""#));
        assert!(s.contains("auth-debugging"));
    }

    #[test]
    fn queued_event_has_job_id() {
        let e = Event::Queued {
            job_id: "job-42".into(),
            ahead: 3,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""job_id":"job-42""#));
        assert!(s.contains(r#""ahead":3"#));
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
