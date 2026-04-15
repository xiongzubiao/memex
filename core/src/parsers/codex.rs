use super::{ConversationMessage, ParsedConversation};
use crate::error::Result;
use std::path::Path;

pub fn parse_codex_jsonl(path: &Path) -> Result<ParsedConversation> {
    let content = std::fs::read_to_string(path)?;
    let session_id = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    parse_codex_content(&content, session_id)
}

pub fn parse_codex_content(content: &str, session_id: String) -> Result<ParsedConversation> {
    let mut messages = Vec::new();
    let mut real_session_id = None;
    for (_line_num, value) in super::parse_jsonl_lines(content)? {
        // Extract session ID from session_meta
        if value.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            if let Some(id) = value.pointer("/payload/id").and_then(|v| v.as_str()) {
                real_session_id = Some(id.to_string());
            }
            continue;
        }

        // Real Codex format: {"type":"response_item","payload":{"role":"...","content":[{"type":"input_text","text":"..."}]}}
        let (role, text) = if let Some(payload) = value.get("payload") {
            let role = payload
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let text = payload
                .get("content")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            (role.to_string(), text)
        } else {
            // Fallback: flat format {"role":"...","content":"..."}
            let role = value
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let text = value
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (role.to_string(), text)
        };

        if text.is_empty() || role == "unknown" || role == "developer" {
            continue;
        }
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        messages.push(ConversationMessage {
            role,
            content: text,
            timestamp,
        });
    }
    let id = real_session_id.unwrap_or(session_id);
    Ok(ParsedConversation::new(id, None, messages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parse_two_message_session() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"role":"user","content":"What is Rust?"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"role":"assistant","content":"A systems programming language."}}"#
        )
        .unwrap();

        let conv = parse_codex_jsonl(&path).unwrap();
        assert_eq!(conv.id, "session");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What is Rust?");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "A systems programming language.");
    }

    #[test]
    fn parse_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::File::create(&path).unwrap();
        let conv = parse_codex_jsonl(&path).unwrap();
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        let err = parse_codex_jsonl(&path).unwrap_err();
        assert!(format!("{err}").contains("Invalid JSON at line 1"));
    }

    #[test]
    fn parse_real_codex_format() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-04-03T21:14:54.883Z","type":"session_meta","payload":{{"id":"019d5532-ef9c","timestamp":"2026-04-03T21:14:54.748Z","cwd":"/tmp"}}}}"#).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-04-03T21:14:54.886Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"What is Rust?"}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"timestamp":"2026-04-03T21:15:24.656Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"A systems programming language."}}]}}}}"#).unwrap();

        let conv = parse_codex_jsonl(&path).unwrap();
        assert_eq!(conv.id, "019d5532-ef9c");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What is Rust?");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "A systems programming language.");
    }

    #[test]
    fn empty_content_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("skip.jsonl");
        std::fs::write(&path, "{\"role\":\"user\",\"content\":\"\"}\n{\"role\":\"assistant\",\"content\":\"response\"}\n").unwrap();
        let conv = parse_codex_jsonl(&path).unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, "assistant");
    }
}
