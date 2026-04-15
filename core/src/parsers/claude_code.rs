use super::{ConversationMessage, ParsedConversation};
use crate::error::Result;
use std::path::Path;

pub fn parse_claude_code_jsonl(path: &Path) -> Result<ParsedConversation> {
    let content = std::fs::read_to_string(path)?;
    let session_id = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    parse_claude_code_content(&content, session_id)
}

pub fn parse_claude_code_content(content: &str, session_id: String) -> Result<ParsedConversation> {
    let mut messages = Vec::new();
    for (_line_num, value) in super::parse_jsonl_lines(content)? {
        let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let role = match msg_type {
            "user" => "user",
            "assistant" => "assistant",
            "tool_use" => "tool",
            "tool_result" => "tool_result",
            _ => continue,
        };
        // Content can be at top level or nested under "message"
        let content_value = value
            .get("content")
            .or_else(|| value.get("message").and_then(|m| m.get("content")));
        let text = content_value
            .and_then(|v| {
                if v.is_string() {
                    v.as_str().map(|s| s.to_string())
                } else if v.is_array() {
                    Some(
                        v.as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    None
                }
            })
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        messages.push(ConversationMessage {
            role: role.to_string(),
            content: text,
            timestamp,
        });
    }
    Ok(ParsedConversation::new(session_id, None, messages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_jsonl(dir: &TempDir, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn parse_simple_session() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "session1.jsonl",
            &[
                r#"{"type":"user","content":"Hello"}"#,
                r#"{"type":"assistant","content":"Hi there"}"#,
                r#"{"type":"user","content":"How are you?"}"#,
                r#"{"type":"assistant","content":"I am fine"}"#,
            ],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(conv.id, "session1");
        assert_eq!(conv.messages.len(), 4);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Hello");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[2].role, "user");
        assert_eq!(conv.messages[3].role, "assistant");
    }

    #[test]
    fn parse_tool_calls() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "tools.jsonl",
            &[
                r#"{"type":"tool_use","content":"run bash"}"#,
                r#"{"type":"tool_result","content":"exit 0"}"#,
            ],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "tool");
        assert_eq!(conv.messages[1].role, "tool_result");
    }

    #[test]
    fn parse_array_content() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "array_content.jsonl",
            &[
                r#"{"type":"assistant","content":[{"type":"text","text":"Part one"},{"type":"text","text":"Part two"}]}"#,
            ],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Part one\nPart two");
    }

    #[test]
    fn parse_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(&dir, "empty.jsonl", &[]);
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(conv.id, "empty");
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn to_json_output() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "json_test.jsonl",
            &[
                r#"{"type":"user","content":"Question"}"#,
                r#"{"type":"assistant","content":"Answer"}"#,
            ],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        let json = conv.to_json();
        assert!(json.contains("Question"));
        assert!(json.contains("Answer"));
        assert!(json.contains("\"role\""));
    }

    #[test]
    fn parse_with_timestamp() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "ts.jsonl",
            &[r#"{"type":"user","content":"Hello","timestamp":"2026-04-06T00:00:00Z"}"#],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(
            conv.messages[0].timestamp.as_deref(),
            Some("2026-04-06T00:00:00Z")
        );
    }

    #[test]
    fn parse_nested_message_format() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "nested.jsonl",
            &[
                r#"{"type":"permission-mode","permissionMode":"default","sessionId":"test-123"}"#,
                r#"{"type":"user","message":{"role":"user","content":"What is Rust?"},"uuid":"m1","timestamp":"2026-04-06T00:00:00Z"}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Rust is a systems language."}]},"uuid":"m2","timestamp":"2026-04-06T00:00:01Z"}"#,
            ],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What is Rust?");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "Rust is a systems language.");
    }

    #[test]
    fn unknown_type_skipped() {
        let dir = TempDir::new().unwrap();
        let path = write_jsonl(
            &dir,
            "mixed.jsonl",
            &[
                r#"{"type":"system","content":"ignored"}"#,
                r#"{"type":"user","content":"kept"}"#,
            ],
        );
        let conv = parse_claude_code_jsonl(&path).unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "kept");
    }
}
