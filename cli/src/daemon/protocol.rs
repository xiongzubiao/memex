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
        #[serde(default)]
        intent: Option<String>,
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
    },
    Ingest {
        source: IngestSource,
        #[serde(default)]
        collections: Vec<String>,
    },
    SourceAdd {
        source_path: String,
        content: String,
        #[serde(default)]
        collections: Vec<String>,
    },
    SourceDelete {
        /// `src-...` docid OR `path:<source-path>`
        #[serde(rename = "ref")]
        ref_: String,
        #[serde(default)]
        force: bool,
    },
    Delete {
        slug: String,
        #[serde(default)]
        force: bool,
    },
    /// Title→slug lookup. BM25 + vector re-rank when ambiguous, BM25-only
    /// short-circuit when the top hit dominates. Routed through the daemon
    /// so the warm embedding model is reused — direct path would reload
    /// ONNX every CLI invocation.
    Search {
        title: String,
    },
    /// `memex lint --fix` — apply auto-fixes for `StaleIndex` and
    /// `OutdatedEmbedding` issues. Daemon-routed so the daemon
    /// remains the single writer; concurrent ingests/writes serialize
    /// against the same lock the daemon uses for everything else.
    LintFix {},
    /// Run EXTRACT (chunked if needed) + MERGE-dry-run for any overlapping
    /// slugs. Streams the resulting plan as JSON via PlanContent / EmptyExtract.
    SourcePlan {
        source_id: String,
    },
    /// Validate a plan and commit each non-dropped proposal as a wiki page
    /// write, with re-MERGE for any user-edited slugs that now overlap.
    PlanApply {
        plan_json: String,
    },
}

fn default_top_k() -> usize {
    10
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
        /// Existing pages this write auto-linked into the new body
        /// (forward link). Filtered by `auto_link_eligible` — short
        /// single-token stems are skipped to avoid false positives
        /// on common English words.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        linked: Vec<String>,
        /// Existing pages whose body now backlinks to the new page.
        /// Same eligibility filter applied to the new page's stem.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        backlinked: Vec<String>,
        /// `[[stem]]` references in the body whose target page does
        /// not exist. Pure information — the daemon does not modify
        /// the body. Surfaced as "suggest-create: ..." in CLI output.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        suggest_create: Vec<String>,
    },
    Deleted {
        slug: String,
    },
    LintResult {
        fixed: u32,
        remaining: u32,
    },
    /// One per-fix progress event from `Request::LintFix`. `kind` is
    /// the LintIssueKind that was repaired (e.g. "stale_index",
    /// "outdated_embedding").
    LintFixed {
        page: String,
        kind: String,
    },
    /// One event for an issue that was already resolved by the time
    /// the daemon got to it (e.g. user re-saved the file between scan
    /// and fix). Mirrors `apply_fix_locked`'s `FixOutcome::Stale`.
    LintAlreadyFixed {
        page: String,
    },
    /// Issue surfaced by the read-only lint scan that has no auto-fix
    /// path (dangling links, untracked files, etc.). Streamed by
    /// `Request::LintFix` after all fixable issues have been processed
    /// so the user sees what's left.
    LintRemaining {
        page: String,
        kind: String,
        target: String,
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
    /// Reply to `Request::Search`. `slug` is `None` when nothing matches.
    SearchResult {
        slug: Option<String>,
    },

    // --- Plan pipeline ---
    /// Plan JSON content for `source plan` (success) or `plan apply`
    /// (exit 3 / exit 4). The CLI writes this to stdout.
    PlanContent {
        json: String,
    },
    /// `source plan`: EXTRACT yielded zero pages. CLI emits empty stdout
    /// with exit 0; the skill surfaces a user-facing message.
    EmptyExtract {
        reason: String,
    },
    /// Advisory progress event during `plan apply`. Consumed-and-dropped
    /// by the CLI; programmatic consumers can read it from the protocol stream.
    PlanApplyProgress {
        slug: String,
        status: String,
    },
    /// `plan apply` full-success terminal: the slugs that were committed
    /// in this run. CLI maps to stdout = `committed N wiki pages\n`.
    PlanApplied {
        committed: Vec<String>,
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
        let r: Request = serde_json::from_str(r#"{"op":"query","question":"q"}"#).unwrap();
        match r {
            Request::Query {
                question,
                raw,
                top_k,
                collections,
                intent,
            } => {
                assert_eq!(question, "q");
                assert!(!raw);
                assert_eq!(top_k, 10);
                assert!(collections.is_empty());
                assert!(intent.is_none());
            }
            _ => panic!("expected Query"),
        }
    }

    #[test]
    fn query_request_deserializes_with_collections() {
        let r: Request = serde_json::from_str(
            r#"{"op":"query","question":"q","collections":["default","project-a"]}"#,
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
    fn search_request_deserializes() {
        let r: Request =
            serde_json::from_str(r#"{"op":"search","title":"Auth Tokens"}"#).unwrap();
        match r {
            Request::Search { title } => assert_eq!(title, "Auth Tokens"),
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn search_result_event_serializes_and_handles_none() {
        let hit = Event::SearchResult {
            slug: Some("auth-tokens".into()),
        };
        let s = serde_json::to_string(&hit).unwrap();
        assert!(s.contains(r#""type":"search_result""#));
        assert!(s.contains(r#""slug":"auth-tokens""#));

        let miss = Event::SearchResult { slug: None };
        let s = serde_json::to_string(&miss).unwrap();
        assert!(s.contains(r#""type":"search_result""#));
        assert!(s.contains(r#""slug":null"#));
    }

    #[test]
    fn unknown_op_is_rejected() {
        let err = serde_json::from_str::<Request>(r#"{"op":"bogus"}"#).unwrap_err();
        assert!(err.to_string().contains("bogus") || err.to_string().contains("variant"));
    }

    #[test]
    fn write_request_deserializes() {
        let r: Request =
            serde_json::from_str(r#"{"op":"write","title":"Test","content":"body"}"#).unwrap();
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
            r#"{"op":"write","title":"T","content":"b","source":"src-deadbeef"}"#,
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
            r##"{"op":"source_add","source_path":"https://x/p","content":"# T\n"}"##,
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
            r#"{"op":"source_delete","ref":"src-abc","force":true}"#,
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
            r#"{"op":"ingest","source":{"kind":"transcript","path":"/tmp/s.jsonl","agent":"claude-code"}}"#,
        )
        .unwrap();
        match r {
            Request::Ingest { source, collections } => {
                match source {
                    IngestSource::Transcript { path, agent } => {
                        assert_eq!(path, "/tmp/s.jsonl");
                        assert_eq!(agent, TranscriptAgent::ClaudeCode);
                    }
                    _ => panic!("expected Transcript"),
                }
                assert!(collections.is_empty());
            }
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn ingest_request_document_deserializes() {
        let r: Request = serde_json::from_str(
            r##"{"op":"ingest","source":{"kind":"document","source_path":"https://example.com/post","content":"# Title\n\nbody\n"},"collections":["team-a"]}"##,
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
            serde_json::from_str(r#"{"op":"delete","slug":"my-page"}"#).unwrap();
        assert!(matches!(r, Request::Delete { .. }));
    }

    #[test]
    fn written_event_serializes() {
        let e = Event::Written {
            slug: "my-page".into(),
            docid: "wiki-abc".into(),
            linked: vec![],
            backlinked: vec![],
            suggest_create: vec![],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"written""#));
        assert!(s.contains(r#""slug":"my-page""#));
        // Empty lists elided from wire payload.
        assert!(!s.contains("linked"));
        assert!(!s.contains("backlinked"));
        assert!(!s.contains("suggest_create"));

        let e = Event::Written {
            slug: "my-page".into(),
            docid: "wiki-abc".into(),
            linked: vec!["caching".into()],
            backlinked: vec!["api-design".into()],
            suggest_create: vec!["ghost".into()],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""linked":["caching"]"#));
        assert!(s.contains(r#""backlinked":["api-design"]"#));
        assert!(s.contains(r#""suggest_create":["ghost"]"#));
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

    #[test]
    fn source_plan_request_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"source_plan","source_id":"src-abc"}"#,
        )
        .unwrap();
        match r {
            Request::SourcePlan { source_id } => assert_eq!(source_id, "src-abc"),
            _ => panic!("expected SourcePlan"),
        }
    }

    #[test]
    fn plan_apply_request_deserializes() {
        let r: Request = serde_json::from_str(
            r#"{"op":"plan_apply","plan_json":"{}"}"#,
        )
        .unwrap();
        match r {
            Request::PlanApply { plan_json } => assert_eq!(plan_json, "{}"),
            _ => panic!("expected PlanApply"),
        }
    }

    #[test]
    fn plan_content_event_serializes() {
        let e = Event::PlanContent { json: "{}".into() };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"plan_content""#));
        assert!(s.contains(r#""json":"{}""#));
    }

    #[test]
    fn empty_extract_event_serializes() {
        let e = Event::EmptyExtract { reason: "no-subjects".into() };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"empty_extract""#));
        assert!(s.contains(r#""reason":"no-subjects""#));
    }

    #[test]
    fn plan_applied_event_serializes_with_committed_list() {
        let e = Event::PlanApplied {
            committed: vec!["mmai".into(), "gpu-checkpoint".into()],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""type":"plan_applied""#));
        assert!(s.contains(r#""mmai""#));
        assert!(s.contains(r#""gpu-checkpoint""#));
    }
}
