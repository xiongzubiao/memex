use memex_core::transcript::{
    parse_claude_code_session, parse_codex_session, parse_gemini_cli_session, SessionFilter,
};
use std::io::Cursor;

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

fn claude_code_sample() -> String {
    let lines = [
        r#"{"type":"permission-mode","permissionMode":"default","sessionId":"abc-123"}"#,
        r#"{"type":"user","message":{"role":"user","content":"Fix the auth bug"},"uuid":"msg-1","timestamp":"2026-04-15T10:00:00Z","sessionId":"abc-123"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me look at auth.rs"},{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"src/auth.rs"}},{"type":"text","text":"I found the issue in auth.rs. The JWT token expiry is set to 1 hour."}]},"uuid":"msg-2","timestamp":"2026-04-15T10:00:05Z","sessionId":"abc-123"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"pub fn validate_token(token: &str) -> Result<Claims> {\n    // ... 200 lines ...\n}"}]},"uuid":"msg-3","timestamp":"2026-04-15T10:00:06Z","sessionId":"abc-123"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll fix the expiry."}]},"uuid":"msg-4","timestamp":"2026-04-15T10:00:10Z","sessionId":"abc-123"}"#,
    ];
    lines.join("\n")
}

#[test]
fn parse_claude_code_extracts_messages() {
    let t = parse_claude_code_session(Cursor::new(claude_code_sample())).unwrap();

    // User text present
    assert!(t.cleaned_text.contains("Fix the auth bug"));
    // Assistant text present
    assert!(t.cleaned_text.contains("I found the issue in auth.rs"));
    assert!(t.cleaned_text.contains("I'll fix the expiry"));
    // Tool name present with summarized input
    assert!(t.cleaned_text.contains("[Tool: Read"));
    assert!(t.cleaned_text.contains("file: src/auth.rs"));
    // Tool result content (the file body) must NOT appear
    assert!(!t.cleaned_text.contains("validate_token"));
    assert!(!t.cleaned_text.contains("200 lines"));
    // Thinking block must NOT appear
    assert!(!t.cleaned_text.contains("Let me look at auth.rs"));
}

#[test]
fn parse_claude_code_metadata() {
    let t = parse_claude_code_session(Cursor::new(claude_code_sample())).unwrap();
    assert_eq!(t.session_id, "abc-123");
    assert_eq!(t.agent, "claude-code");
    assert_eq!(t.first_user_message, "Fix the auth bug");
    assert_eq!(t.filter, SessionFilter::Pass);
}

// ---------------------------------------------------------------------------
// Filter tests
// ---------------------------------------------------------------------------

#[test]
fn filter_aborted_session() {
    // Only a permission-mode line — no user/assistant messages.
    let jsonl = r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s1"}"#;
    let t = parse_claude_code_session(Cursor::new(jsonl)).unwrap();
    assert_eq!(t.filter, SessionFilter::NonSubstantive);
}

#[test]
fn filter_no_user_messages() {
    // Only an assistant message, no user.
    let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]}}"#;
    let t = parse_claude_code_session(Cursor::new(jsonl)).unwrap();
    assert_eq!(t.filter, SessionFilter::NonSubstantive);
}

#[test]
fn filter_internal_session() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"/memex-distill summarize recent work"},"uuid":"m1","sessionId":"d1"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Distilling..."}]},"uuid":"m2","sessionId":"d1"}"#,
    ];
    let t = parse_claude_code_session(Cursor::new(lines.join("\n"))).unwrap();
    assert_eq!(t.filter, SessionFilter::InternalSession);
    assert_eq!(t.first_user_message, "/memex-distill summarize recent work");
}

#[test]
fn filter_internal_session_extract_prompt() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"Extract knowledge from this transcript into wiki pages."},"uuid":"m1","sessionId":"d1"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Extracting..."}]},"uuid":"m2","sessionId":"d1"}"#,
    ];
    let t = parse_claude_code_session(Cursor::new(lines.join("\n"))).unwrap();
    assert_eq!(t.filter, SessionFilter::InternalSession);
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

fn codex_sample() -> String {
    let lines = [
        r#"{"timestamp":"2026-04-03T21:14:54Z","type":"session_meta","payload":{"id":"codex-123"}}"#,
        r#"{"timestamp":"2026-04-03T21:15:00Z","type":"event_msg","payload":{"type":"user_message","message":"Fix the auth bug"}}"#,
        r#"{"timestamp":"2026-04-03T21:15:01Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"cat src/auth.rs\"}"}}"#,
        r#"{"timestamp":"2026-04-03T21:15:02Z","type":"response_item","payload":{"type":"function_call_output","output":"pub fn validate_token() { ... }"}}"#,
        r#"{"timestamp":"2026-04-03T21:15:03Z","type":"response_item","payload":{"type":"message","content":[{"type":"output_text","text":"The auth module uses JWT with 1-hour expiry."}]}}"#,
    ];
    lines.join("\n")
}

#[test]
fn parse_codex_extracts_messages() {
    let t = parse_codex_session(Cursor::new(codex_sample())).unwrap();

    // User prompt present
    assert!(t.cleaned_text.contains("Fix the auth bug"));
    // Assistant text present
    assert!(t.cleaned_text.contains("The auth module uses JWT"));
    // Tool call metadata present (function_call parsed as JSON)
    assert!(t.cleaned_text.contains("[Tool: shell"));
    assert!(t.cleaned_text.contains("command: cat src/auth.rs"));
    // function_call_output stripped
    assert!(!t.cleaned_text.contains("validate_token"));

    assert_eq!(t.session_id, "codex-123");
    assert_eq!(t.agent, "codex");
    assert_eq!(t.filter, SessionFilter::Pass);
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

fn gemini_sample() -> &'static str {
    r#"{
    "sessionId": "gemini-456",
    "messages": [
        {"id":"m1","type":"user","content":[{"text":"Fix the auth bug"}]},
        {"id":"m2","type":"gemini","content":"The JWT expiry is too long.","toolCalls":[{"name":"read_file","args":{"path":"src/auth.rs"}}]},
        {"id":"m3","type":"tool","content":"pub fn validate_token() { ... }"}
    ]
}"#
}

#[test]
fn parse_gemini_extracts_messages() {
    let t = parse_gemini_cli_session(gemini_sample()).unwrap();

    // User text present
    assert!(t.cleaned_text.contains("Fix the auth bug"));
    // Gemini content present
    assert!(t.cleaned_text.contains("The JWT expiry is too long"));
    // Tool call metadata present
    assert!(t.cleaned_text.contains("[Tool: read_file"));
    // Tool type messages stripped
    assert!(!t.cleaned_text.contains("validate_token"));

    assert_eq!(t.session_id, "gemini-456");
    assert_eq!(t.agent, "gemini-cli");
    assert_eq!(t.filter, SessionFilter::Pass);
}

// ---------------------------------------------------------------------------
// Malformed JSON recovery (#8b)
// ---------------------------------------------------------------------------

#[test]
fn malformed_json_recovery() {
    // Malformed lines are skipped, not fatal. A session with only malformed
    // lines produces a successful parse with NonSubstantive filter.
    let result = parse_claude_code_session(Cursor::new("not json\n"));
    assert!(result.is_ok());
    let transcript = result.unwrap();
    assert_eq!(transcript.filter, memex_core::transcript::SessionFilter::NonSubstantive);
}

// ---------------------------------------------------------------------------
// Tag stripping
// ---------------------------------------------------------------------------

#[test]
fn strip_tags_removes_system_reminder() {
    let lines = [
        r#"{"type":"user","message":{"content":"Hello <system-reminder>CLAUDE.md contents here</system-reminder> world"},"sessionId":"t1"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Response <private>secret stuff</private> here"}]},"sessionId":"t1"}"#,
    ];
    let t = parse_claude_code_session(Cursor::new(lines.join("\n"))).unwrap();
    assert!(t.cleaned_text.contains("Hello"), "should keep text before tag");
    assert!(t.cleaned_text.contains("world"), "should keep text after tag");
    assert!(!t.cleaned_text.contains("CLAUDE.md"), "should strip system-reminder content");
    assert!(t.cleaned_text.contains("Response"), "should keep assistant text before tag");
    assert!(t.cleaned_text.contains("here"), "should keep assistant text after tag");
    assert!(!t.cleaned_text.contains("secret stuff"), "should strip private content");
}

#[test]
fn strip_tags_memex_context() {
    use memex_core::transcript::strip_tags;
    let input = "Before <memex-context>injected wiki results</memex-context> after";
    let result = strip_tags(input);
    assert_eq!(result.trim(), "Before  after");
    assert!(!result.contains("injected wiki results"));
}

#[test]
fn strip_tags_no_match_returns_unchanged() {
    use memex_core::transcript::strip_tags;
    let input = "Normal text with no tags";
    let result = strip_tags(input);
    assert_eq!(result, input);
}

// ---------------------------------------------------------------------------
// Secret redaction (#10)
// ---------------------------------------------------------------------------

#[test]
fn secret_redaction_in_tool_summary() {
    use memex_core::transcript::summarize_tool_input;
    use serde_json::json;

    // OpenAI-style key in a bash command
    let input = json!({"cmd": "export OPENAI_API_KEY=sk-abc123def456ghi789jkl012mno345"});
    let summary = summarize_tool_input("bash", &input);
    assert!(summary.contains("[REDACTED]"), "should redact sk- key: {summary}");
    assert!(!summary.contains("sk-abc123"), "raw key must not remain: {summary}");
}

// ---------------------------------------------------------------------------
// UTF-8 safe truncation (#6)
// ---------------------------------------------------------------------------

#[test]
fn truncation_does_not_panic_on_multibyte() {
    use memex_core::transcript::summarize_tool_input;
    use serde_json::json;

    // A command with multi-byte chars that would panic with &s[..100]
    let long_cmd = "echo ".to_string() + &"日本語".repeat(50); // 155+ chars
    let input = json!({"cmd": long_cmd});
    let summary = summarize_tool_input("bash", &input);
    // Must not panic, and should be truncated
    assert!(summary.starts_with("command: echo "));
    // 100 chars for the command portion + "command: " prefix
    assert!(summary.chars().count() <= 110);
}
