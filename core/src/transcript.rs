use regex::Regex;
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

#[derive(Debug, Clone)]
pub struct CleanedTranscript {
    pub cleaned_text: String,
    pub session_id: String,
    pub agent: String,
    pub first_user_message: String,
    pub filter: SessionFilter,
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
fn redact_secrets(s: &str) -> String {
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
fn classify(
    has_user: bool,
    has_assistant: bool,
    first_user_message: &str,
) -> SessionFilter {
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

// ---------------------------------------------------------------------------
// Parser 1: Claude Code  (.jsonl — one JSON object per line)
// ---------------------------------------------------------------------------

pub fn parse_claude_code_session(reader: impl BufRead) -> Result<CleanedTranscript, String> {
    let mut session_id = String::new();
    let mut sections: Vec<String> = Vec::new();
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

        match msg_type {
            "permission-mode" => {}
            "user" => {
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
                        sections.push(format!("## User\n{cleaned}\n"));
                    }
                }
                // Array content (tool_result blocks) — skip content bodies.
            }
            "assistant" => {
                if let Some(arr) = obj["message"]["content"].as_array() {
                    let mut text_parts: Vec<String> = Vec::new();
                    for block in arr {
                        let block_type =
                            block.get("type").and_then(Value::as_str).unwrap_or("");
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
                                let input =
                                    block.get("input").cloned().unwrap_or(Value::Null);
                                let summary = summarize_tool_input(name, &input);
                                text_parts.push(format!("[Tool: {name} — {summary}]"));
                            }
                            // "thinking" — intentionally skipped
                            _ => {}
                        }
                    }
                    if !text_parts.is_empty() {
                        has_assistant = true;
                        sections.push(format!(
                            "## Assistant\n{}\n",
                            text_parts.join("\n")
                        ));
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
        cleaned_text: sections.join("\n"),
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
    let mut sections: Vec<String> = Vec::new();
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
                        sections.push(format!("## User\n{cleaned}\n"));
                    }
                }
            }
            "response_item" => {
                let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "message" => {
                        if let Some(content) = payload.get("content").and_then(Value::as_array)
                        {
                            for part in content {
                                let part_type =
                                    part.get("type").and_then(Value::as_str).unwrap_or("");
                                if part_type == "output_text"
                                    && let Some(t) = part.get("text").and_then(Value::as_str)
                                {
                                    has_assistant = true;
                                    sections.push(format!("## Assistant\n{t}\n"));
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
                        sections.push(format!(
                            "## Assistant\n[Tool: {name} — {summary}]\n"
                        ));
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
                                sections.push(format!("## User\n{cleaned}\n"));
                            }
                        }
                    }
                    "agent_message" => {
                        if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                            has_assistant = true;
                            sections.push(format!("## Assistant\n{msg}\n"));
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
        cleaned_text: sections.join("\n"),
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

    let mut sections: Vec<String> = Vec::new();
    let mut first_user_message = String::new();
    let mut has_user = false;
    let mut has_assistant = false;

    for msg in messages {
        let msg_type = msg.get("type").and_then(Value::as_str).unwrap_or("");
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
                                sections.push(format!("## User\n{cleaned}\n"));
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
                        let name = tc
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let args = tc.get("args").cloned().unwrap_or(Value::Null);
                        let summary = summarize_tool_input(name, &args);
                        text_parts.push(format!("[Tool: {name} — {summary}]"));
                    }
                }

                if !text_parts.is_empty() {
                    has_assistant = true;
                    sections.push(format!(
                        "## Assistant\n{}\n",
                        text_parts.join("\n")
                    ));
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
        cleaned_text: sections.join("\n"),
        session_id,
        agent: "gemini-cli".to_string(),
        first_user_message,
        filter,
    })
}
