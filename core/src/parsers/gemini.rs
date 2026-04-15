use super::{ConversationMessage, ParsedConversation};
use crate::error::{MemexError, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static PROMPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"body-1">\s*Prompted[\s\u{00a0}]+(.*?)<br"#).unwrap());
static TS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\w{3}\s+\d{1,2},\s+\d{4},\s+[\d:]+[\s\u{202f}\u{00a0}]+[AP]M\s+\w+)").unwrap()
});
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static IMG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<img[^>]+src="([^"]+)"#).unwrap());
static NUMERIC_ENTITY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(\d+);").unwrap());

/// Extract a Gemini Takeout archive (ZIP or TGZ) to a temp directory,
/// find MyActivity.html or MyActivity.json, parse into conversations.
/// Returns (conversations, image_filenames_per_conv, temp_dir) so the caller can
/// pass temp_dir to store_conversations for image copying, then clean up.
pub fn extract_and_parse(
    path: &Path,
) -> Result<(Vec<ParsedConversation>, Vec<Vec<String>>, PathBuf)> {
    let temp_dir = std::env::temp_dir().join(format!("memex-takeout-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)?;

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let is_tgz = ext == "tgz" || (ext == "gz" && stem.ends_with(".tar"));

    if is_tgz {
        extract_tgz(path, &temp_dir)?;
    } else {
        extract_zip(path, &temp_dir)?;
    }

    let (convs, images) = find_and_parse_activity(&temp_dir)?;
    Ok((convs, images, temp_dir))
}

fn extract_zip(path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| MemexError::ValidationFailure {
        details: format!("Invalid ZIP: {e}"),
    })?;
    archive
        .extract(dest)
        .map_err(|e| MemexError::ValidationFailure {
            details: format!("Failed to extract ZIP: {e}"),
        })?;
    Ok(())
}

fn extract_tgz(path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(dest)
        .map_err(|e| MemexError::ValidationFailure {
            details: format!("Failed to extract TGZ: {e}"),
        })?;
    Ok(())
}

/// Walk the extracted directory, find MyActivity.html or MyActivity.json, parse conversations.
/// Returns (conversations, image_filenames_per_conversation).
fn find_and_parse_activity(dir: &Path) -> Result<(Vec<ParsedConversation>, Vec<Vec<String>>)> {
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let name = entry.file_name().to_string_lossy();
        if name == "MyActivity.html"
            && let Ok(html) = std::fs::read_to_string(entry.path())
        {
            let (convs, images) = parse_gemini_html(&html);
            if !convs.is_empty() {
                return Ok((convs, images));
            }
        } else if name == "MyActivity.json"
            && let Ok(json_str) = std::fs::read_to_string(entry.path())
        {
            let convs = parse_gemini_json(&json_str)?;
            if !convs.is_empty() {
                // JSON format has no image extraction
                let images = vec![vec![]; convs.len()];
                return Ok((convs, images));
            }
        }
    }
    Ok((vec![], vec![]))
}

/// Parse MyActivity.json (array of activity objects).
fn parse_gemini_json(json_str: &str) -> Result<Vec<ParsedConversation>> {
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(json_str).map_err(|e| MemexError::ValidationFailure {
            details: format!("Invalid MyActivity.json: {e}"),
        })?;

    let mut conversations = Vec::new();
    for entry in entries.iter() {
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        // Extract subtitles (user prompts) and description (response)
        let mut messages = Vec::new();

        // Some JSON formats have "subtitles" array with user queries
        if let Some(subs) = entry.get("subtitles").and_then(|v| v.as_array()) {
            for sub in subs {
                if let Some(text) = sub.get("name").and_then(|v| v.as_str()) {
                    messages.push(ConversationMessage {
                        role: "user".to_string(),
                        content: text.to_string(),
                        timestamp: None,
                    });
                }
            }
        }
        // Description or products as response context
        if let Some(desc) = entry.get("description").and_then(|v| v.as_str()) {
            messages.push(ConversationMessage {
                role: "model".to_string(),
                content: desc.to_string(),
                timestamp: None,
            });
        }
        // Fallback: use title as the prompt if no subtitles
        if messages.is_empty() && title != "Untitled" {
            messages.push(ConversationMessage {
                role: "user".to_string(),
                content: title.to_string(),
                timestamp: None,
            });
        }

        if !messages.is_empty() {
            // Hash all message content for a collision-resistant conversation ID.
            // Gemini Takeout doesn't include a conversation UUID, so we derive one.
            let all_content: String = messages.iter().map(|m| m.content.as_str()).collect();
            let id = crate::storage::content_hash(all_content.as_bytes());
            conversations.push(ParsedConversation::new(
                id,
                Some(truncate(title, 60)),
                messages,
            ));
        }
    }
    Ok(conversations)
}

/// Store individual conversations under sources/gemini/conversations/.
/// Copies referenced images to sources/gemini/images/.
/// Dedup: skips conversations whose content is unchanged. Re-ingests if content differs.
/// `image_source_dir`: directory where extracted images live (from archive extraction).
/// `image_filenames`: per-conversation lists of image filenames to copy.
pub fn store_conversations(
    root: &Path,
    conversations: &[ParsedConversation],
    image_source_dir: Option<&Path>,
    image_filenames: &[Vec<String>],
) -> Result<Vec<PathBuf>> {
    let conv_dir = root.join("sources/gemini/conversations");
    let images_dir = root.join("sources/gemini/images");
    std::fs::create_dir_all(&conv_dir)?;

    // Build image source lookup map once (filename → full path)
    let img_lookup: std::collections::HashMap<String, PathBuf> =
        if let Some(src_dir) = image_source_dir {
            walkdir::WalkDir::new(src_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| (e.file_name().to_string_lossy().to_string(), e.into_path()))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    // Copy images before storing conversations (so meta can reference them)
    let mut images_dir_created = false;
    for imgs in image_filenames {
        for img_name in imgs {
            let img_dest = images_dir.join(img_name);
            if img_dest.exists() {
                continue;
            }
            if let Some(src_path) = img_lookup.get(img_name) {
                if !images_dir_created {
                    std::fs::create_dir_all(&images_dir)?;
                    images_dir_created = true;
                }
                let _ = std::fs::copy(src_path, &img_dest);
            }
        }
    }

    super::store_conversations(root, "gemini", conversations, |i, _conv| {
        let imgs = image_filenames.get(i).cloned().unwrap_or_default();
        serde_json::json!({
            "image_count": imgs.len(),
            "images": imgs.iter().map(|f| format!("images/{f}")).collect::<Vec<_>>(),
        })
    })
}

/// Parse a Google Takeout MyActivity.html file into individual conversations.
/// Each conversation is a `div.outer-cell` containing a user prompt and Gemini response.
/// Returns (conversations, image_filenames_per_conversation).
pub fn parse_gemini_html(html: &str) -> (Vec<ParsedConversation>, Vec<Vec<String>>) {
    let mut conversations = Vec::new();
    let mut all_images: Vec<Vec<String>> = Vec::new();

    // Decode numeric HTML entities first so regex can match Unicode chars
    let decoded = decode_numeric_entities(html);

    // Split on outer-cell divs (each is one conversation)
    let parts: Vec<&str> = decoded
        .split("outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp")
        .collect();

    for part in parts.iter().skip(1) {
        // Extract user prompt
        let prompt_text = if let Some(m) = PROMPT_RE.captures(part) {
            let raw = m.get(1).map(|m| m.as_str()).unwrap_or("");
            html_decode(&TAG_RE.replace_all(raw, ""))
        } else {
            continue;
        };

        // Extract timestamp
        let timestamp = TS_RE
            .captures(part)
            .and_then(|m| m.get(1))
            .map(|m| m.as_str().to_string());

        // Extract image references from this conversation block
        let images: Vec<String> = IMG_RE
            .captures_iter(part)
            .filter_map(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .filter(|s| super::is_image_file(s))
            .collect();

        // Extract response
        let response_text = if let Some(ts_match) = TS_RE.find(part) {
            let after_ts = &part[ts_match.end()..];
            let content_start = after_ts.find("<br").map(|i| i + 4).unwrap_or(0);
            let content_end = after_ts[content_start..]
                .find("content-cell mdl-cell mdl-cell--6-col")
                .unwrap_or(after_ts.len() - content_start);
            let raw_html = &after_ts[content_start..content_start + content_end];
            html_to_text(raw_html)
        } else {
            String::new()
        };

        if prompt_text.is_empty() && response_text.is_empty() {
            continue;
        }

        // Hash all content for collision-resistant ID (no UUID in Gemini Takeout)
        let all_content = format!("{prompt_text}{response_text}");
        let id = crate::storage::content_hash(all_content.as_bytes());
        let title = Some(truncate(&prompt_text, 60));
        let mut messages = Vec::new();
        messages.push(ConversationMessage {
            role: "user".to_string(),
            content: prompt_text,
            timestamp: timestamp.clone(),
        });
        if !response_text.is_empty() {
            messages.push(ConversationMessage {
                role: "model".to_string(),
                content: response_text,
                timestamp,
            });
        }

        conversations.push(ParsedConversation {
            id,
            title,
            messages,
        });
        all_images.push(images);
    }

    (conversations, all_images)
}

/// Decode numeric HTML entities (&#160; &#8239; &#39; etc.) to their Unicode chars.
fn decode_numeric_entities(s: &str) -> String {
    NUMERIC_ENTITY_RE
        .replace_all(s, |caps: &regex::Captures| {
            let num: u32 = caps[1].parse().unwrap_or(0);
            char::from_u32(num)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .to_string()
}

/// Decode common HTML entities.
fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Convert HTML to readable text, preserving paragraph/heading structure.
fn html_to_text(html: &str) -> String {
    let s = html
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</p>", "\n\n")
        .replace("</h1>", "\n\n")
        .replace("</h2>", "\n\n")
        .replace("</h3>", "\n\n")
        .replace("</li>", "\n")
        .replace("<li>", "- ")
        .replace("<hr>", "\n---\n")
        .replace("<hr/>", "\n---\n");
    let text = TAG_RE.replace_all(&s, "");
    html_decode(text.trim())
}

/// Truncate string to max chars, adding "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Find a safe UTF-8 boundary
        let end = s
            .char_indices()
            .take_while(|(i, _)| *i < max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &s[..end])
    }
}

/// Parse a Google Takeout directory containing JSON conversation files.
pub fn parse_gemini_export(dir_path: &Path) -> Result<Vec<ParsedConversation>> {
    if !dir_path.is_dir() {
        return Err(MemexError::ValidationFailure {
            details: format!("{} is not a directory", dir_path.display()),
        });
    }
    let mut conversations = Vec::new();
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let content = std::fs::read_to_string(&path)?;
            let value: serde_json::Value =
                serde_json::from_str(&content).map_err(|e| MemexError::ValidationFailure {
                    details: format!("Invalid JSON in {}: {e}", path.display()),
                })?;
            let id = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let mut messages = Vec::new();
            if let Some(entries) = value.as_array() {
                for entry_val in entries {
                    let role = entry_val
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let text = entry_val
                        .get("parts")
                        .and_then(|v| v.as_array())
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
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
            if !messages.is_empty() {
                conversations.push(ParsedConversation::new(id, None, messages));
            }
        }
    }
    Ok(conversations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_takeout_directory_with_one_conv() {
        let dir = TempDir::new().unwrap();
        let json_content = r#"[
            {"role":"user","parts":[{"text":"What is machine learning?"}]},
            {"role":"model","parts":[{"text":"Machine learning is a subset of AI."}]}
        ]"#;
        std::fs::write(dir.path().join("conversation1.json"), json_content).unwrap();

        let convs = parse_gemini_export(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.id, "conversation1");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].content, "What is machine learning?");
        assert_eq!(conv.messages[1].role, "model");
        assert_eq!(
            conv.messages[1].content,
            "Machine learning is a subset of AI."
        );
    }

    #[test]
    fn not_a_directory_returns_error() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("not_a_dir.txt");
        std::fs::write(&file_path, "some content").unwrap();

        let err = parse_gemini_export(&file_path).unwrap_err();
        assert!(format!("{err}").contains("is not a directory"));
    }

    #[test]
    fn non_json_files_ignored() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();
        std::fs::write(
            dir.path().join("conv.json"),
            r#"[{"role":"user","parts":[{"text":"hi"}]}]"#,
        )
        .unwrap();

        let convs = parse_gemini_export(dir.path()).unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn empty_conversation_excluded() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("empty.json"), "[]").unwrap();

        let convs = parse_gemini_export(dir.path()).unwrap();
        assert!(convs.is_empty());
    }

    #[test]
    fn parse_html_single_conversation() {
        let html = r#"<html><body>
        <div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
        <div class="mdl-grid"><div class="header-cell mdl-cell mdl-cell--12-col">
        <p class="mdl-typography--title">Gemini Apps<br></p></div>
        <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">Prompted&#160;What is Rust?<br>Apr 6, 2026, 3:00:00&#8239;PM CDT<br>
        <p>Rust is a systems programming language focused on safety and performance.</p>
        <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1 mdl-typography--text-right">
        </div></div></div></div>
        </body></html>"#;
        let (convs, images) = parse_gemini_html(html);
        assert_eq!(convs.len(), 1);
        assert_eq!(images.len(), 1);
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[0].content, "What is Rust?");
        assert_eq!(convs[0].messages[1].role, "model");
        assert!(convs[0].messages[1].content.contains("systems programming"));
        // ID is full SHA-256 of all message content (64 hex chars)
        assert_eq!(convs[0].id.len(), 64);
    }

    #[test]
    fn parse_html_multiple_conversations() {
        let html = r#"<html><body>
        <div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
        <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">Prompted&#160;Hello<br>Jan 1, 2026, 1:00:00&#8239;PM CDT<br><p>Hi there!</p>
        <div class="content-cell mdl-cell mdl-cell--6-col"></div></div></div>
        <div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
        <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">Prompted&#160;Goodbye<br>Jan 2, 2026, 2:00:00&#8239;PM CDT<br><p>See you!</p>
        <div class="content-cell mdl-cell mdl-cell--6-col"></div></div></div>
        </body></html>"#;
        let (convs, images) = parse_gemini_html(html);
        assert_eq!(convs.len(), 2);
        assert_eq!(images.len(), 2);
        assert_eq!(convs[0].messages[0].content, "Hello");
        assert_eq!(convs[1].messages[0].content, "Goodbye");
    }

    #[test]
    fn parse_html_empty() {
        let (convs, images) = parse_gemini_html("<html><body></body></html>");
        assert!(convs.is_empty());
        assert!(images.is_empty());
    }

    #[test]
    fn parse_html_with_html_entities() {
        let html = r#"<html><body>
        <div class="outer-cell mdl-cell mdl-cell--12-col mdl-shadow--2dp">
        <div class="content-cell mdl-cell mdl-cell--6-col mdl-typography--body-1">Prompted&#160;What&#39;s &amp; how &lt;does&gt; it work?<br>Mar 1, 2026, 10:00:00&#8239;AM CDT<br><p>It works like this.</p>
        <div class="content-cell mdl-cell mdl-cell--6-col"></div></div></div>
        </body></html>"#;
        let (convs, _images) = parse_gemini_html(html);
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages[0].content, "What's & how <does> it work?");
    }

    #[test]
    fn store_conversations_uses_json_extension_and_new_dir() {
        let root = TempDir::new().unwrap();
        let conv = ParsedConversation::new(
            "abc123def456".to_string(),
            Some("Test".to_string()),
            vec![ConversationMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
                timestamp: None,
            }],
        );
        let stored = store_conversations(root.path(), &[conv], None, &[vec![]]).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].to_string_lossy(),
            "sources/gemini/conversations/abc123def456.json"
        );
        // File should exist
        let file_path = root
            .path()
            .join("sources/gemini/conversations/abc123def456.json");
        assert!(file_path.exists());
    }

    #[test]
    fn store_conversations_dedup_by_file_existence() {
        let root = TempDir::new().unwrap();
        let conv = ParsedConversation::new(
            "dedup000test".to_string(),
            None,
            vec![ConversationMessage {
                role: "user".to_string(),
                content: "Test".to_string(),
                timestamp: None,
            }],
        );
        // Store once
        let stored1 =
            store_conversations(root.path(), std::slice::from_ref(&conv), None, &[vec![]]).unwrap();
        assert_eq!(stored1.len(), 1);
        // Store again — should be deduped
        let stored2 = store_conversations(root.path(), &[conv], None, &[vec![]]).unwrap();
        assert_eq!(stored2.len(), 0);
    }
}
