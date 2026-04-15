pub mod chatgpt;
pub mod claude;
pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod gemini_cli;

use crate::error::{MemexError, Result};
use std::path::{Path, PathBuf};

/// Check if a filename has a known image extension.
pub(crate) fn is_image_file(name: &str) -> bool {
    name.ends_with(".png")
        || name.ends_with(".jpg")
        || name.ends_with(".jpeg")
        || name.ends_with(".gif")
        || name.ends_with(".webp")
}

/// Parse JSONL content into a vec of (line_number, Value) pairs.
/// Skips empty lines; returns an error on malformed JSON lines.
pub(crate) fn parse_jsonl_lines(content: &str) -> Result<Vec<(usize, serde_json::Value)>> {
    let mut entries = Vec::new();
    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| MemexError::ValidationFailure {
                details: format!("Invalid JSON at line {}: {e}", line_num + 1),
            })?;
        entries.push((line_num, value));
    }
    Ok(entries)
}

/// Read `content_hash` from a `.meta.json` sidecar file. Returns `None` if the
/// file doesn't exist, can't be parsed, or doesn't contain a `content_hash` field.
pub(crate) fn read_cached_content_hash(meta_path: &Path) -> Option<String> {
    std::fs::read_to_string(meta_path)
        .ok()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
        .and_then(|v| {
            v.get("content_hash")
                .and_then(|h| h.as_str().map(String::from))
        })
}

/// Store a batch of parsed conversations with content-hash dedup.
///
/// For each conversation: compute hash, check meta sidecar for cached hash (skip if match),
/// write JSON + meta sidecar. Returns paths of newly stored conversations.
///
/// `platform` is the subdirectory name under `sources/` (e.g. "chatgpt", "claude", "gemini").
/// `extra_meta` is called per conversation to add platform-specific fields to the meta sidecar.
pub(crate) fn store_conversations(
    root: &Path,
    platform: &str,
    conversations: &[ParsedConversation],
    extra_meta: impl Fn(usize, &ParsedConversation) -> serde_json::Value,
) -> Result<Vec<PathBuf>> {
    let conv_dir = root.join(format!("sources/{platform}/conversations"));
    std::fs::create_dir_all(&conv_dir)?;

    let mut stored = Vec::new();
    for (i, conv) in conversations.iter().enumerate() {
        let filename = format!("{}.json", conv.id);
        let dest = conv_dir.join(&filename);
        let new_content = conv.to_json();
        let new_hash = crate::storage::content_hash(new_content.as_bytes());

        // Check cached hash (handles nonexistent file gracefully via read_cached_content_hash)
        let meta_path = conv_dir.join(format!("{filename}.meta.json"));
        if read_cached_content_hash(&meta_path).as_deref() == Some(&new_hash) {
            continue;
        }
        if dest.exists() {
            tracing::info!(id = %conv.id, "conversation updated, re-ingesting");
        }

        std::fs::write(&dest, &new_content)?;

        let mut meta = serde_json::json!({
            "id": conv.id,
            "title": conv.title,
            "message_count": conv.messages.len(),
            "content_hash": new_hash,
            "stored_at": chrono::Utc::now().to_rfc3339(),
        });
        // Merge platform-specific fields
        if let Some(extra) = extra_meta(i, conv).as_object()
            && let Some(base) = meta.as_object_mut()
        {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        std::fs::write(
            conv_dir.join(format!("{filename}.meta.json")),
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        )?;

        stored.push(PathBuf::from(format!(
            "sources/{platform}/conversations/{filename}"
        )));
    }
    Ok(stored)
}

#[derive(Debug, Clone)]
pub struct ParsedConversation {
    pub id: String,
    pub title: Option<String>,
    pub messages: Vec<ConversationMessage>,
}

#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
}

impl ParsedConversation {
    pub fn new(id: String, title: Option<String>, messages: Vec<ConversationMessage>) -> Self {
        Self {
            id,
            title,
            messages,
        }
    }

    /// Serialize as JSON for storage and LLM consumption.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "id": self.id,
            "title": self.title,
            "messages": self.messages.iter().map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                    "timestamp": m.timestamp,
                })
            }).collect::<Vec<_>>()
        }))
        .unwrap_or_default()
    }
}
