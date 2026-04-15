use super::{ConversationMessage, ParsedConversation};
use crate::error::{MemexError, Result};
use std::path::{Path, PathBuf};

/// Parse a Claude.ai export folder. Reads conversations.json, splits per conversation.
/// Returns parsed conversations.
pub fn parse_claude_export(folder: &Path) -> Result<Vec<ParsedConversation>> {
    let conv_path = folder.join("conversations.json");
    if !conv_path.exists() {
        return Err(MemexError::ValidationFailure {
            details: format!("conversations.json not found in {}", folder.display()),
        });
    }
    let raw = std::fs::read_to_string(&conv_path)?;
    let conversations: Vec<serde_json::Value> =
        serde_json::from_str(&raw).map_err(|e| MemexError::ValidationFailure {
            details: format!("Invalid JSON: {e}"),
        })?;

    let mut result = Vec::new();
    for conv in conversations {
        let uuid = conv
            .get("uuid")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let name = conv
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut messages = Vec::new();

        if let Some(msgs) = conv.get("chat_messages").and_then(|v| v.as_array()) {
            for msg in msgs {
                let sender = msg
                    .get("sender")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let role = match sender {
                    "human" => "user",
                    "assistant" => "assistant",
                    _ => sender,
                };
                // Use "text" field (plain text), fall back to "content" if needed
                let text = msg
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                let timestamp = msg
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                messages.push(ConversationMessage {
                    role: role.to_string(),
                    content: text,
                    timestamp,
                });
            }
        }

        if !messages.is_empty() {
            result.push(ParsedConversation::new(uuid, name, messages));
        }
    }
    Ok(result)
}

/// Store Claude conversations + supporting files under sources/claude/.
///
/// - Conversations stored as {uuid}.json under sources/claude/conversations/
/// - memories.json, projects.json, users.json copied to sources/claude/
/// - Dedup: skip if {uuid}.json already exists
///
/// Returns list of newly stored conversation paths.
pub fn store_claude_export(
    root: &Path,
    folder: &Path,
    conversations: &[ParsedConversation],
) -> Result<Vec<PathBuf>> {
    let claude_dir = root.join("sources/claude");
    let conv_dir = claude_dir.join("conversations");
    std::fs::create_dir_all(&conv_dir)?;

    // Copy supporting files (memories.json, projects.json, users.json)
    for filename in &["memories.json", "projects.json", "users.json"] {
        let src = folder.join(filename);
        let dest = claude_dir.join(filename);
        if src.exists() && !dest.exists() {
            std::fs::copy(&src, &dest)?;
        }
    }

    super::store_conversations(
        root,
        "claude",
        conversations,
        |_i, _conv| serde_json::json!({ "images": Vec::<String>::new() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_claude_conversations() {
        let dir = TempDir::new().unwrap();
        let conv_json = r#"[
            {
                "uuid": "abc-123",
                "name": "Test conversation",
                "chat_messages": [
                    {"uuid": "m1", "text": "Hello Claude", "sender": "human", "created_at": "2026-04-06T00:00:00Z", "attachments": [], "files": []},
                    {"uuid": "m2", "text": "Hello! How can I help?", "sender": "assistant", "created_at": "2026-04-06T00:00:01Z", "attachments": [], "files": []}
                ]
            }
        ]"#;
        std::fs::write(dir.path().join("conversations.json"), conv_json).unwrap();

        let convs = parse_claude_export(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].id, "abc-123");
        assert_eq!(convs[0].title, Some("Test conversation".to_string()));
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
    }

    #[test]
    fn parse_claude_missing_conversations_json() {
        let dir = TempDir::new().unwrap();
        let err = parse_claude_export(dir.path()).unwrap_err();
        assert!(format!("{err}").contains("conversations.json not found"));
    }

    #[test]
    fn store_claude_conversations_with_dedup() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();

        let export_dir = dir.path().join("export");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("memories.json"), r#"[{"memory": "test"}]"#).unwrap();

        let convs = vec![ParsedConversation::new(
            "uuid-1".to_string(),
            Some("Conv 1".to_string()),
            vec![ConversationMessage {
                role: "user".to_string(),
                content: "Hi".to_string(),
                timestamp: None,
            }],
        )];

        // First store
        let stored = store_claude_export(&root, &export_dir, &convs).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            root.join("sources/claude/conversations/uuid-1.json")
                .exists()
        );
        assert!(root.join("sources/claude/memories.json").exists());

        // Second store = dedup
        let stored2 = store_claude_export(&root, &export_dir, &convs).unwrap();
        assert!(stored2.is_empty());
    }

    #[test]
    fn to_json_roundtrip() {
        let conv = ParsedConversation::new(
            "test-id".to_string(),
            Some("Test".to_string()),
            vec![ConversationMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
                timestamp: None,
            }],
        );
        let json = conv.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], "test-id");
        assert_eq!(parsed["messages"][0]["role"], "user");
    }
}
