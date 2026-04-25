//! IPC protocol types. Newline-delimited JSON over Unix socket.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TranscriptAgent {
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl TranscriptAgent {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranscriptAgent::ClaudeCode => "claude-code",
            TranscriptAgent::Codex => "codex",
            TranscriptAgent::GeminiCli => "gemini-cli",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngestSource {
    Transcript {
        path: String,
        agent: TranscriptAgent,
    },
    Document {
        source_path: String,
        content: String,
    },
}

/// Incoming request. `op` discriminates the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping {},
    Query {
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
        title: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        force: bool,
        memex_root: String,
    },
    Ingest {
        source: IngestSource,
        #[serde(default)]
        collections: Vec<String>,
        memex_root: String,
    },
    SourceAdd {
        source_path: String,
        content: String,
        #[serde(default)]
        collections: Vec<String>,
        memex_root: String,
    },
    SourceDelete {
        /// `src-...` docid OR `path:<source-path>`
        #[serde(rename = "ref")]
        ref_: String,
        #[serde(default)]
        force: bool,
        memex_root: String,
    },
    Delete {
        slug: String,
        memex_root: String,
    },
    LintFix {
        memex_root: String,
    },
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
    SourceAdded {
        docid: String,
    },
    SourceDeleted {
        docid: String,
        source_path: String,
        /// Wiki page slugs whose frontmatter `sources:` field referenced this source.
        /// They become dangling — `memex lint` reports them.
        dangling_wiki_pages: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_request_deserializes() {
        let r: Request = serde_json::from_str(r#"{"op":"ping"}"#).unwrap();
        assert!(matches!(r, Request::Ping {}));
    }

    #[test]
    fn query_request_deserializes_with_defaults() {
        let r: Request =
            serde_json::from_str(r#"{"op":"query","question":"q","memex_root":"/x"}"#).unwrap();
        match r {
            Request::Query {
                question,
                raw,
                top_k,
                collections,
                memex_root,
            } => {
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
                title, content, tags, source, force, ..
            } => {
                assert_eq!(title, "Test");
                assert_eq!(content, "body");
                assert!(tags.is_empty());
                assert!(source.is_none());
                assert!(!force);
            }
            _ => panic!("expected Write"),
        }
    }

    #[test]
    fn write_request_with_source_docid_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"write","title":"T","content":"b","source":"src-deadbeef","memex_root":"/x"}"#,
        )
        .unwrap();
        match r {
            Request::Write { source, .. } => assert_eq!(source.as_deref(), Some("src-deadbeef")),
            _ => panic!("expected Write"),
        }
    }

    #[test]
    fn source_add_request_deserializes() {
        let r: Request = serde_json::from_str(
            r##"{"op":"source_add","source_path":"https://x/p","content":"# T\n","memex_root":"/x"}"##,
        )
        .unwrap();
        match r {
            Request::SourceAdd { source_path, content, .. } => {
                assert_eq!(source_path, "https://x/p");
                assert!(content.starts_with("# T"));
            }
            _ => panic!("expected SourceAdd"),
        }
    }

    #[test]
    fn source_added_event_serializes() {
        let e = Event::SourceAdded { docid: "src-abc".into() };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"source_added""#));
        assert!(s.contains(r#""docid":"src-abc""#));
    }

    #[test]
    fn source_delete_request_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"source_delete","ref":"src-abc","force":true,"memex_root":"/x"}"#,
        )
        .unwrap();
        match r {
            Request::SourceDelete { ref_, force, .. } => {
                assert_eq!(ref_, "src-abc");
                assert!(force);
            }
            _ => panic!("expected SourceDelete"),
        }
    }

    #[test]
    fn source_deleted_event_serializes() {
        let e = Event::SourceDeleted {
            docid: "src-abc".into(),
            source_path: "https://x/p".into(),
            dangling_wiki_pages: vec!["my-page".into()],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"source_deleted""#));
        assert!(s.contains(r#""dangling_wiki_pages":["my-page"]"#));
    }

    #[test]
    fn ingest_request_transcript_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"ingest","source":{"kind":"transcript","path":"/tmp/s.jsonl","agent":"claude-code"},"memex_root":"/x"}"#,
        )
        .unwrap();
        match r {
            Request::Ingest { source, collections, memex_root } => {
                match source {
                    IngestSource::Transcript { path, agent } => {
                        assert_eq!(path, "/tmp/s.jsonl");
                        assert_eq!(agent, TranscriptAgent::ClaudeCode);
                    }
                    _ => panic!("expected Transcript"),
                }
                assert!(collections.is_empty());
                assert_eq!(memex_root, "/x");
            }
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn ingest_request_document_deserializes() {
        let r: Request = serde_json::from_str(
            r##"{"op":"ingest","source":{"kind":"document","source_path":"https://example.com/post","content":"# Title\n\nbody\n"},"collections":["team-a"],"memex_root":"/x"}"##,
        )
        .unwrap();
        match r {
            Request::Ingest { source, collections, .. } => {
                match source {
                    IngestSource::Document { source_path, content } => {
                        assert_eq!(source_path, "https://example.com/post");
                        assert!(content.starts_with("# Title"));
                    }
                    _ => panic!("expected Document"),
                }
                assert_eq!(collections, vec!["team-a"]);
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
