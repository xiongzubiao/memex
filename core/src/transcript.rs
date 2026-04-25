use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::BufRead;
use std::sync::LazyLock;

static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"sk-[A-Za-z0-9]{20,}",
        r"AKIA[A-Z0-9]{16}",
        r"(?:password|token|secret|key)\s*[:=]\s*[A-Za-z0-9+/=]{32,}",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

/// Tags injected by the system or by memex itself that should be stripped
/// from transcript content before knowledge extraction.
/// One pattern per tag since the regex crate doesn't support backreferences.
static TAG_STRIP_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?s)<system-reminder>.*?</system-reminder>",
        r"(?s)<private>.*?</private>",
        r"(?s)<memex-context>.*?</memex-context>",
        r"(?s)<persisted-output>.*?</persisted-output>",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFilter {
    Pass,
    NonSubstantive,
    InternalSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptAgent {
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl TranscriptAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptAgent::ClaudeCode => "claude-code",
            TranscriptAgent::Codex => "codex",
            TranscriptAgent::GeminiCli => "gemini-cli",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CleanedTranscript {
    pub turns: Vec<TranscriptTurn>,
    pub session_id: String,
    pub agent: String,
    pub first_user_message: String,
    pub filter: SessionFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptTurn {
    pub role: String,
    pub timestamp: Option<String>,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Safe UTF-8 truncation: never panics on multi-byte chars.
pub fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect::<String>()
}

/// Strip system-injected tags from content.
/// Removes <system-reminder>, <private>, <memex-context>, <persisted-output> and their contents.
pub fn strip_tags(s: &str) -> String {
    let mut result = std::borrow::Cow::Borrowed(s);
    for re in TAG_STRIP_PATTERNS.iter() {
        if let std::borrow::Cow::Owned(replaced) = re.replace_all(&result, "") {
            result = std::borrow::Cow::Owned(replaced);
        }
    }
    result.into_owned()
}

/// Redact common secret patterns from a string.
/// Uses Cow to avoid allocation when no secrets are found.
pub fn redact_secrets(s: &str) -> String {
    let mut result = std::borrow::Cow::Borrowed(s);
    for re in SECRET_PATTERNS.iter() {
        if let std::borrow::Cow::Owned(replaced) = re.replace_all(&result, "[REDACTED]") {
            result = std::borrow::Cow::Owned(replaced);
        }
    }
    result.into_owned()
}

/// Summarize tool inputs for metadata, keeping paths and patterns but
/// stripping bulky content like file bodies and command output.
pub fn summarize_tool_input(tool_name: &str, input: &Value) -> String {
    let raw = if tool_name.eq_ignore_ascii_case("read")
        || tool_name.eq_ignore_ascii_case("edit")
        || tool_name.eq_ignore_ascii_case("write")
    {
        let path = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        format!("file: {path}")
    } else if tool_name.eq_ignore_ascii_case("bash") || tool_name.eq_ignore_ascii_case("shell") {
        let cmd = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        format!("command: {}", truncate(cmd, 100))
    } else if tool_name.eq_ignore_ascii_case("grep") {
        let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("?");
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        format!("pattern: {pattern} in {path}")
    } else if tool_name.eq_ignore_ascii_case("glob") {
        let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("?");
        format!("pattern: {pattern}")
    } else {
        let first_str = input
            .as_object()
            .and_then(|m| m.values().find_map(Value::as_str))
            .or_else(|| input.as_str())
            .unwrap_or("");
        truncate(first_str, 80)
    };
    redact_secrets(&raw)
}

/// Determine the filter category for a parsed transcript.
/// InternalSession catches daemon-spawned sessions (distillation, expansion, synthesis)
/// even if the MEMEX_INTERNAL env var guard wasn't set.
fn classify(has_user: bool, has_assistant: bool, first_user_message: &str) -> SessionFilter {
    if !has_user || !has_assistant {
        SessionFilter::NonSubstantive
    } else if is_internal_prompt(first_user_message) {
        SessionFilter::InternalSession
    } else {
        SessionFilter::Pass
    }
}

/// Belt-and-suspenders detection of daemon-spawned sessions.
/// Matches known prompt patterns from daemon expansion, synthesis, and distillation.
fn is_internal_prompt(first_msg: &str) -> bool {
    let trimmed = first_msg.trim();
    trimmed.starts_with("/memex-distill")
        || trimmed.starts_with("/memex-ingest")
        || trimmed.starts_with("Extract knowledge from this transcript")
        || trimmed.starts_with("Merge the new content into the existing page")
        || trimmed.starts_with("You are a memex synthesis agent")
        || trimmed.starts_with("You are a memex expansion agent")
}

/// Extract full ISO timestamp from a message object's `timestamp` field.
/// Returns `None` when the field is missing or empty.
fn extract_timestamp(obj: &Value) -> Option<String> {
    let ts = obj.get("timestamp").and_then(Value::as_str)?.trim();
    if ts.is_empty() {
        None
    } else {
        Some(ts.to_string())
    }
}

/// Format a section heading with an optional timestamp prefix.
fn heading(role: &str, ts: Option<&str>) -> String {
    match ts {
        Some(t) => format!("## {role} [{t}]"),
        None => format!("## {role}"),
    }
}

/// Render structured turns to canonical cleaned-text format used by storage
/// and retrieval (`## User/Assistant [<ISO timestamp>]` headings + body text).
pub fn render_turns(turns: &[TranscriptTurn]) -> String {
    let mut sections = Vec::new();
    for t in turns {
        let role_heading = if t.role.eq_ignore_ascii_case("user") {
            "User".to_string()
        } else if t.role.eq_ignore_ascii_case("assistant") {
            "Assistant".to_string()
        } else {
            t.role.clone()
        };
        sections.push(format!(
            "{}\n{}\n",
            heading(&role_heading, t.timestamp.as_deref()),
            t.text.trim()
        ));
    }
    sections.join("\n")
}

// ---------------------------------------------------------------------------
// Parser 1: Claude Code  (.jsonl — one JSON object per line)
// ---------------------------------------------------------------------------

pub fn parse_claude_code_session(reader: impl BufRead) -> Result<CleanedTranscript, String> {
    let mut session_id = String::new();
    let mut turns: Vec<TranscriptTurn> = Vec::new();
    let mut first_user_message = String::new();
    let mut has_user = false;
    let mut has_assistant = false;

    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("IO error reading line {}: {e}", line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // Skip malformed lines (partial writes, truncation)
        };

        let msg_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        // Capture session ID from any line that has one.
        if session_id.is_empty()
            && let Some(sid) = obj.get("sessionId").and_then(Value::as_str)
        {
            session_id = sid.to_string();
        }

        let timestamp = extract_timestamp(&obj);
        match msg_type {
            "permission-mode" => {}
            "user" => {
                let parsed_role = obj
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("user")
                    .to_string();
                let content = &obj["message"]["content"];
                if let Some(text) = content.as_str() {
                    // Plain user text — strip system-injected tags
                    let cleaned = strip_tags(text);
                    let cleaned = cleaned.trim();
                    if !cleaned.is_empty() {
                        has_user = true;
                        if first_user_message.is_empty() {
                            first_user_message = cleaned.to_string();
                        }
                        turns.push(TranscriptTurn {
                            role: parsed_role,
                            timestamp: timestamp.clone(),
                            text: cleaned.to_string(),
                        });
                    }
                }
                // Array content (tool_result blocks) — skip content bodies.
            }
            "assistant" => {
                let parsed_role = obj
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("assistant")
                    .to_string();
                if let Some(arr) = obj["message"]["content"].as_array() {
                    let mut text_parts: Vec<String> = Vec::new();
                    for block in arr {
                        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    let cleaned = strip_tags(t);
                                    let cleaned = cleaned.trim();
                                    if !cleaned.is_empty() {
                                        text_parts.push(cleaned.to_string());
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let input = block.get("input").cloned().unwrap_or(Value::Null);
                                let summary = summarize_tool_input(name, &input);
                                text_parts.push(format!("[Tool: {name} — {summary}]"));
                            }
                            // "thinking" — intentionally skipped
                            _ => {}
                        }
                    }
                    if !text_parts.is_empty() {
                        has_assistant = true;
                        let text = text_parts.join("\n");
                        turns.push(TranscriptTurn {
                            role: parsed_role,
                            timestamp: timestamp.clone(),
                            text,
                        });
                    }
                }
            }
            other => {
                // Format version detection (#11): warn on truly unknown types.
                if !other.is_empty()
                    && !matches!(
                        other,
                        "file-history-snapshot" | "attachment" | "queue-operation" | "last-prompt"
                    )
                {
                    eprintln!("memex: unknown Claude Code message type: {other}");
                }
            }
        }
    }

    let filter = classify(has_user, has_assistant, &first_user_message);

    Ok(CleanedTranscript {
        turns,
        session_id,
        agent: "claude-code".to_string(),
        first_user_message,
        filter,
    })
}

// ---------------------------------------------------------------------------
// Parser 2: Codex CLI  (.jsonl — one JSON object per line)
// ---------------------------------------------------------------------------

pub fn parse_codex_session(reader: impl BufRead) -> Result<CleanedTranscript, String> {
    let mut session_id = String::new();
    let mut turns: Vec<TranscriptTurn> = Vec::new();
    let mut first_user_message = String::new();
    let mut has_user = false;
    let mut has_assistant = false;

    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("IO error reading line {}: {e}", line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // Skip malformed lines
        };

        let msg_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = &obj["payload"];
        let timestamp = extract_timestamp(&obj);

        match msg_type {
            "session_meta" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    session_id = id.to_string();
                }
                // Version detection (#11): log for debugging
                if let Some(ver) = payload.get("cli_version").and_then(Value::as_str) {
                    let _ = ver; // version noted for format detection
                }
            }
            "turn_context" => {
                if let Some(prompt) = payload.get("prompt").and_then(Value::as_str) {
                    let cleaned = strip_tags(prompt);
                    let cleaned = cleaned.trim();
                    if !cleaned.is_empty() {
                        has_user = true;
                        if first_user_message.is_empty() {
                            first_user_message = cleaned.to_string();
                        }
                        turns.push(TranscriptTurn {
                            role: "user".to_string(),
                            timestamp: timestamp.clone(),
                            text: cleaned.to_string(),
                        });
                    }
                }
            }
            "response_item" => {
                let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "message" => {
                        if let Some(content) = payload.get("content").and_then(Value::as_array) {
                            for part in content {
                                let part_type =
                                    part.get("type").and_then(Value::as_str).unwrap_or("");
                                if part_type == "output_text"
                                    && let Some(t) = part.get("text").and_then(Value::as_str)
                                {
                                    has_assistant = true;
                                    turns.push(TranscriptTurn {
                                        role: "assistant".to_string(),
                                        timestamp: timestamp.clone(),
                                        text: t.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    "function_call" => {
                        let name = payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        // Review finding #7: arguments is a JSON string, parse it.
                        let input = payload
                            .get("arguments")
                            .and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .unwrap_or(Value::Null);
                        let summary = summarize_tool_input(name, &input);
                        has_assistant = true;
                        let text = format!("[Tool: {name} — {summary}]");
                        turns.push(TranscriptTurn {
                            role: "assistant".to_string(),
                            timestamp: timestamp.clone(),
                            text,
                        });
                    }
                    "function_call_output" => {
                        // Intentionally skipped — tool results are bulk data.
                    }
                    _ => {}
                }
            }
            "event_msg" => {
                let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match event_type {
                    "user_message" => {
                        if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                            let cleaned = strip_tags(msg);
                            let cleaned = cleaned.trim();
                            if !cleaned.is_empty() {
                                has_user = true;
                                if first_user_message.is_empty() {
                                    first_user_message = cleaned.to_string();
                                }
                                turns.push(TranscriptTurn {
                                    role: "user".to_string(),
                                    timestamp: timestamp.clone(),
                                    text: cleaned.to_string(),
                                });
                            }
                        }
                    }
                    "agent_message" => {
                        if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                            has_assistant = true;
                            turns.push(TranscriptTurn {
                                role: "assistant".to_string(),
                                timestamp: timestamp.clone(),
                                text: msg.to_string(),
                            });
                        }
                    }
                    _ => {} // other event_msg types are informational
                }
            }
            _ => {}
        }
    }

    let filter = classify(has_user, has_assistant, &first_user_message);

    Ok(CleanedTranscript {
        turns,
        session_id,
        agent: "codex".to_string(),
        first_user_message,
        filter,
    })
}

// ---------------------------------------------------------------------------
// Parser 3: Gemini CLI  (single JSON object, NOT JSONL)
// ---------------------------------------------------------------------------

pub fn parse_gemini_cli_session(json_str: &str) -> Result<CleanedTranscript, String> {
    let root: Value = serde_json::from_str(json_str).map_err(|e| {
        format!(
            "Gemini JSON parse error: {e}. \
             Expected a single JSON object with \"sessionId\" and \"messages\"."
        )
    })?;

    let session_id = root
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let messages = root
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "Gemini JSON missing \"messages\" array. \
             Expected top-level {\"sessionId\": ..., \"messages\": [...]}."
                .to_string()
        })?;

    let mut turns: Vec<TranscriptTurn> = Vec::new();
    let mut first_user_message = String::new();
    let mut has_user = false;
    let mut has_assistant = false;

    for msg in messages {
        let msg_type = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let timestamp = extract_timestamp(msg);
        match msg_type {
            "user" => {
                if let Some(content) = msg.get("content").and_then(Value::as_array) {
                    for part in content {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            let cleaned = strip_tags(text);
                            let cleaned = cleaned.trim();
                            if !cleaned.is_empty() {
                                has_user = true;
                                if first_user_message.is_empty() {
                                    first_user_message = cleaned.to_string();
                                }
                                turns.push(TranscriptTurn {
                                    role: "user".to_string(),
                                    timestamp: timestamp.clone(),
                                    text: cleaned.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            "gemini" => {
                let mut text_parts: Vec<String> = Vec::new();

                if let Some(content) = msg.get("content").and_then(Value::as_str) {
                    let cleaned = strip_tags(content);
                    let cleaned = cleaned.trim();
                    if !cleaned.is_empty() {
                        text_parts.push(cleaned.to_string());
                    }
                }

                if let Some(tool_calls) = msg.get("toolCalls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let name = tc.get("name").and_then(Value::as_str).unwrap_or("unknown");
                        let args = tc.get("args").cloned().unwrap_or(Value::Null);
                        let summary = summarize_tool_input(name, &args);
                        text_parts.push(format!("[Tool: {name} — {summary}]"));
                    }
                }

                if !text_parts.is_empty() {
                    has_assistant = true;
                    let text = text_parts.join("\n");
                    turns.push(TranscriptTurn {
                        role: "assistant".to_string(),
                        timestamp: timestamp.clone(),
                        text,
                    });
                }
            }
            "tool" => {
                // Intentionally skipped — tool result content.
            }
            _ => {}
        }
    }

    let filter = classify(has_user, has_assistant, &first_user_message);

    Ok(CleanedTranscript {
        turns,
        session_id,
        agent: "gemini-cli".to_string(),
        first_user_message,
        filter,
    })
}

/// Inspect a file's content to determine which agent's transcript format it
/// matches, if any. Returns None if the file doesn't look like a transcript
/// (extension mismatch, malformed JSON, no recognizable signature keys).
///
/// Used by the CLI when `--agent` is omitted to choose between transcript
/// mode and document file mode. The daemon never calls this — it receives
/// fully-typed IngestSource requests.
pub fn detect_transcript_agent(path: &std::path::Path) -> Option<TranscriptAgent> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    match ext {
        "json" => {
            // Gemini: single JSON object with `messages` field.
            let raw = std::fs::read_to_string(path).ok()?;
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            if v.is_object()
                && v.get("messages").map(|m| m.is_array()).unwrap_or(false)
            {
                Some(TranscriptAgent::GeminiCli)
            } else {
                None
            }
        }
        "jsonl" => {
            use std::io::BufRead;
            let f = std::fs::File::open(path).ok()?;
            let r = std::io::BufReader::new(f);
            for line in r.lines() {
                let line = line.ok()?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let v: serde_json::Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => return None,
                };
                if v.get("type").is_some()
                    && (v.get("uuid").is_some() || v.get("parentUuid").is_some())
                {
                    return Some(TranscriptAgent::ClaudeCode);
                }
                if v.get("event_msg").is_some() || v.get("response_id").is_some() {
                    return Some(TranscriptAgent::Codex);
                }
                return None;
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secrets_redacts_known_patterns() {
        let s = "key sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA bla AKIAABCDEFGHIJKLMNOP after\npassword=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let out = redact_secrets(s);
        assert!(!out.contains("sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), "got: {out}");
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"), "got: {out}");
        assert!(!out.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), "got: {out}");
    }

    #[test]
    fn detect_claude_code_jsonl() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","uuid":"abc","message":{"role":"user","content":"hi"}}
{"type":"assistant","parentUuid":"abc","message":{"role":"assistant","content":"hello"}}
"#,
        ).unwrap();
        assert_eq!(detect_transcript_agent(&path), Some(TranscriptAgent::ClaudeCode));
    }

    #[test]
    fn detect_codex_jsonl() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"event_msg":"started","response_id":"r1"}
"#,
        ).unwrap();
        assert_eq!(detect_transcript_agent(&path), Some(TranscriptAgent::Codex));
    }

    #[test]
    fn detect_gemini_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"messages": [{"role": "user", "parts": [{"text": "hi"}]}]}"#,
        ).unwrap();
        assert_eq!(detect_transcript_agent(&path), Some(TranscriptAgent::GeminiCli));
    }

    #[test]
    fn detect_returns_none_for_markdown_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Title\n\nbody\n").unwrap();
        assert_eq!(detect_transcript_agent(&path), None);
    }

    #[test]
    fn detect_returns_none_for_jsonl_with_unknown_schema() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("data.jsonl");
        std::fs::write(&path, "{\"id\": 1, \"value\": \"foo\"}\n").unwrap();
        assert_eq!(detect_transcript_agent(&path), None);
    }

    #[test]
    fn detect_returns_none_for_malformed_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("broken.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        assert_eq!(detect_transcript_agent(&path), None);
    }

    #[test]
    fn detect_returns_none_for_nonexistent_file() {
        let path = std::path::PathBuf::from("/tmp/does-not-exist-12345.jsonl");
        assert_eq!(detect_transcript_agent(&path), None);
    }
}
