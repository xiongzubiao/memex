use super::{ConversationMessage, ParsedConversation};
use crate::error::{MemexError, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn parse_chatgpt_zip(path: &Path) -> Result<Vec<ParsedConversation>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| MemexError::ValidationFailure {
        details: format!("Invalid ZIP: {e}"),
    })?;

    let mut all_conversations_data: Vec<serde_json::Value> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| MemexError::ValidationFailure {
                details: format!("ZIP entry error: {e}"),
            })?;
        let name = entry.name().to_string();
        if (name == "conversations.json"
            || (name.starts_with("conversations-") && name.ends_with(".json")))
            && !name.contains('/')
        {
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .map_err(|e| MemexError::ValidationFailure {
                    details: format!("Read error: {e}"),
                })?;
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&content).map_err(|e| MemexError::ValidationFailure {
                    details: format!("Invalid JSON in {name}: {e}"),
                })?;
            all_conversations_data.extend(parsed);
        }
    }

    if all_conversations_data.is_empty() {
        return Err(MemexError::ValidationFailure {
            details: "No conversations*.json found in ZIP".to_string(),
        });
    }

    let mut result = Vec::new();
    for conv in all_conversations_data {
        let id = conv
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let title = conv
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut messages = Vec::new();

        if let Some(mapping) = conv.get("mapping").and_then(|v| v.as_object()) {
            let mut msg_list: Vec<(&String, &serde_json::Value)> = mapping.iter().collect();
            msg_list.sort_by_key(|(_, v)| {
                v.get("message")
                    .and_then(|m| m.get("create_time"))
                    .and_then(|t| t.as_f64())
                    .map(|f| (f * 1000.0) as i64)
                    .unwrap_or(0)
            });
            for (_, node) in msg_list {
                if let Some(msg) = node.get("message") {
                    let role = msg
                        .get("author")
                        .and_then(|a| a.get("role"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    let text = msg
                        .get("content")
                        .and_then(|c| c.get("parts"))
                        .and_then(|p| p.as_array())
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        messages.push(ConversationMessage {
                            role: role.to_string(),
                            content: text,
                            timestamp: None,
                        });
                    }
                }
            }
        }
        if !messages.is_empty() {
            result.push(ParsedConversation::new(id, title, messages));
        }
    }
    Ok(result)
}

/// Store ChatGPT conversations + supporting files under sources/chatgpt/.
/// - Conversations as {id}.json under sources/chatgpt/conversations/
/// - Images copied to sources/chatgpt/images/
/// - user.json, user_settings.json, etc. copied to sources/chatgpt/
/// - Dedup by file existence
///
/// `temp_dir`: extracted ZIP contents (for copying images + support files)
pub fn store_chatgpt_conversations(
    root: &Path,
    conversations: &[ParsedConversation],
    temp_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let chatgpt_dir = root.join("sources/chatgpt");
    let conv_dir = chatgpt_dir.join("conversations");
    let images_dir = chatgpt_dir.join("images");
    std::fs::create_dir_all(&conv_dir)?;

    // Copy supporting files and images from temp_dir in a single walk
    if let Some(dir) = temp_dir {
        let support_files: std::collections::HashSet<&str> = [
            "user.json",
            "user_settings.json",
            "export_manifest.json",
            "shared_conversations.json",
        ]
        .into_iter()
        .collect();
        let mut images_dir_created = false;

        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if support_files.contains(name.as_str()) {
                let dest = chatgpt_dir.join(&name);
                if !dest.exists() {
                    let _ = std::fs::copy(entry.path(), &dest);
                }
            } else if super::is_image_file(&name) {
                if !images_dir_created {
                    std::fs::create_dir_all(&images_dir)?;
                    images_dir_created = true;
                }
                let dest = images_dir.join(&name);
                if !dest.exists() {
                    let _ = std::fs::copy(entry.path(), &dest);
                }
            }
        }
    }

    // Collect image list once (not per conversation)
    let image_list: Vec<String> = if images_dir.exists() {
        std::fs::read_dir(&images_dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| super::is_image_file(&e.file_name().to_string_lossy()))
                    .map(|e| format!("images/{}", e.file_name().to_string_lossy()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    super::store_conversations(
        root,
        "chatgpt",
        conversations,
        |_i, _conv| serde_json::json!({ "images": image_list }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn make_chatgpt_zip(dir: &TempDir, conversations_json: &str) -> std::path::PathBuf {
        let zip_path = dir.path().join("chatgpt_export.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("conversations.json", options).unwrap();
        zip.write_all(conversations_json.as_bytes()).unwrap();
        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn parse_simple_chatgpt_export() {
        let dir = TempDir::new().unwrap();
        let conversations_json = r#"[
            {
                "id": "conv-abc",
                "title": "Test Chat",
                "mapping": {
                    "node1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["Hello from user"]},
                            "create_time": 1000.0
                        }
                    },
                    "node2": {
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": ["Hello from assistant"]},
                            "create_time": 2000.0
                        }
                    }
                }
            }
        ]"#;
        let zip_path = make_chatgpt_zip(&dir, conversations_json);
        let convs = parse_chatgpt_zip(&zip_path).unwrap();
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.id, "conv-abc");
        assert_eq!(conv.title.as_deref(), Some("Test Chat"));
        assert_eq!(conv.messages.len(), 2);
        // Sorted by create_time: user first, then assistant
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "Hello from user");
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "Hello from assistant");
    }

    #[test]
    fn missing_conversations_json_errors() {
        let dir = TempDir::new().unwrap();
        let zip_path = dir.path().join("empty.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("other.json", options).unwrap();
        zip.finish().unwrap();

        let err = parse_chatgpt_zip(&zip_path).unwrap_err();
        assert!(format!("{err}").contains("No conversations*.json found"));
    }

    #[test]
    fn empty_messages_conversation_excluded() {
        let dir = TempDir::new().unwrap();
        let conversations_json = r#"[
            {
                "id": "empty-conv",
                "title": "Empty",
                "mapping": {}
            }
        ]"#;
        let zip_path = make_chatgpt_zip(&dir, conversations_json);
        let convs = parse_chatgpt_zip(&zip_path).unwrap();
        assert!(convs.is_empty());
    }

    #[test]
    fn parse_multi_file_conversations() {
        let dir = TempDir::new().unwrap();
        let zip_path = dir.path().join("chatgpt_export.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();

        // First file
        zip.start_file("conversations-000.json", options).unwrap();
        zip.write_all(br#"[{"id":"conv-1","title":"First","mapping":{"n1":{"message":{"author":{"role":"user"},"content":{"parts":["Hello"]},"create_time":1.0}}}}]"#).unwrap();

        // Second file
        zip.start_file("conversations-001.json", options).unwrap();
        zip.write_all(br#"[{"id":"conv-2","title":"Second","mapping":{"n1":{"message":{"author":{"role":"user"},"content":{"parts":["World"]},"create_time":1.0}}}}]"#).unwrap();

        zip.finish().unwrap();

        let convs = parse_chatgpt_zip(&zip_path).unwrap();
        assert_eq!(convs.len(), 2);
        assert_eq!(convs[0].id, "conv-1");
        assert_eq!(convs[1].id, "conv-2");
    }

    #[test]
    fn store_chatgpt_conversations_with_dedup() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();

        let convs = vec![ParsedConversation::new(
            "chatgpt-id-1".to_string(),
            Some("My Chat".to_string()),
            vec![ConversationMessage {
                role: "user".to_string(),
                content: "Hi there".to_string(),
                timestamp: None,
            }],
        )];

        // First store
        let stored = store_chatgpt_conversations(&root, &convs, None).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            root.join("sources/chatgpt/conversations/chatgpt-id-1.json")
                .exists()
        );
        assert!(
            root.join("sources/chatgpt/conversations/chatgpt-id-1.json.meta.json")
                .exists()
        );

        // Second store = dedup
        let stored2 = store_chatgpt_conversations(&root, &convs, None).unwrap();
        assert!(stored2.is_empty());
    }

    #[test]
    fn store_chatgpt_conversations_copies_support_files() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();

        // Set up a fake temp_dir with support files and an image
        let temp_dir = dir.path().join("extracted");
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::write(temp_dir.join("user.json"), r#"{"name":"Test"}"#).unwrap();
        std::fs::write(temp_dir.join("photo.png"), b"\x89PNG").unwrap();

        let convs = vec![ParsedConversation::new(
            "chatgpt-id-2".to_string(),
            None,
            vec![ConversationMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
                timestamp: None,
            }],
        )];

        let stored = store_chatgpt_conversations(&root, &convs, Some(temp_dir.as_path())).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(root.join("sources/chatgpt/user.json").exists());
        assert!(root.join("sources/chatgpt/images/photo.png").exists());
    }
}
