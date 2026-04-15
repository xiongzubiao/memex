use super::{ConversationMessage, ParsedConversation};
use crate::error::Result;
use std::path::Path;

/// Parse a Gemini CLI session file. Supports two formats:
/// - JSON: Single object with `sessionId` and `messages[]` array (real Gemini CLI format)
/// - JSONL: One JSON object per line with `role`/`parts` fields (legacy/test format)
pub fn parse_gemini_cli_session(path: &Path) -> Result<ParsedConversation> {
    let content = std::fs::read_to_string(path)?;
    let session_id = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    parse_gemini_cli_content(&content, session_id)
}

pub fn parse_gemini_cli_content(content: &str, session_id: String) -> Result<ParsedConversation> {
    let trimmed = content.trim();

    // Try JSON format first (real Gemini CLI: single object with sessionId + messages)
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && value.get("sessionId").is_some()
        && value.get("messages").is_some()
    {
        return parse_gemini_cli_json(&value);
    }

    // Fallback: JSONL format (one message per line)
    parse_gemini_cli_jsonl(content, session_id)
}

/// Parse real Gemini CLI JSON format:
/// ```json
/// {"sessionId":"...","messages":[{"type":"user","content":[{"text":"..."}]},{"type":"gemini","content":"..."}]}
/// ```
fn parse_gemini_cli_json(value: &serde_json::Value) -> Result<ParsedConversation> {
    let session_id = value
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut messages = Vec::new();
    if let Some(msgs) = value.get("messages").and_then(|v| v.as_array()) {
        for msg in msgs {
            let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let role = match msg_type {
                "user" => "user",
                "gemini" => "model",
                _ => continue,
            };

            let text = if let Some(content) = msg.get("content") {
                if content.is_string() {
                    content.as_str().unwrap_or("").to_string()
                } else if content.is_array() {
                    content
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            if text.is_empty() {
                continue;
            }

            let timestamp = msg
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            messages.push(ConversationMessage {
                role: role.to_string(),
                content: text,
                timestamp,
            });
        }
    }
    Ok(ParsedConversation::new(session_id, None, messages))
}

/// Parse legacy JSONL format (one message per line).
fn parse_gemini_cli_jsonl(content: &str, session_id: String) -> Result<ParsedConversation> {
    let mut messages = Vec::new();
    for (_line_num, value) in super::parse_jsonl_lines(content)? {
        let role = value
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let text = value
            .get("parts")
            .and_then(|v| v.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .or_else(|| {
                value
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        messages.push(ConversationMessage {
            role: role.to_string(),
            content: text,
            timestamp: None,
        });
    }
    Ok(ParsedConversation::new(session_id, None, messages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parse_real_gemini_cli_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"sessionId":"4df482b9","messages":[{"type":"user","content":[{"text":"What is Rust?"}],"timestamp":"2026-04-08T14:09:16.820Z"},{"type":"gemini","content":"Rust is a systems programming language.","timestamp":"2026-04-08T14:09:18.532Z"}]}"#,
        )
        .unwrap();

        let conv = parse_gemini_cli_session(&path).unwrap();
        assert_eq!(conv.id, "4df482b9");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What is Rust?");
        assert_eq!(conv.messages[1].role, "model");
        assert_eq!(
            conv.messages[1].content,
            "Rust is a systems programming language."
        );
        assert_eq!(
            conv.messages[0].timestamp.as_deref(),
            Some("2026-04-08T14:09:16.820Z")
        );
    }

    #[test]
    fn parse_jsonl_parts_format() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gemini_session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"role":"user","parts":[{{"text":"Tell me about Rust"}}]}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"role":"model","parts":[{{"text":"Rust is a systems language"}},{{"text":" with safety guarantees"}}]}}"#
        )
        .unwrap();

        let conv = parse_gemini_cli_session(&path).unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Tell me about Rust");
    }

    #[test]
    fn parse_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::File::create(&path).unwrap();
        let conv = parse_gemini_cli_session(&path).unwrap();
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        let err = parse_gemini_cli_session(&path).unwrap_err();
        assert!(format!("{err}").contains("Invalid JSON at line 1"));
    }
}
