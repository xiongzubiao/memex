use chrono::Utc;
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Derive a session slug from the task string.
/// Lowercases, keeps alphanumeric and spaces, truncates to 40 chars, replaces spaces with dashes.
fn make_slug(task: &str) -> String {
    let cleaned: String = task
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_ascii_whitespace())
        .collect();
    let lower = cleaned.to_lowercase();
    let trimmed = lower.trim();
    let truncated: String = trimmed.chars().take(40).collect();
    truncated.trim_end().replace(' ', "-")
}

/// Creates `{memex_root}/sources/brainstorms/{date}-{slug}/` with a `meta.json`.
/// Returns `(session_id, session_dir)`.
pub fn create_session(memex_root: &Path, task: &str) -> io::Result<(String, PathBuf)> {
    let now = Utc::now();
    let date = now.format("%Y-%m-%d").to_string();
    let slug = make_slug(task);
    let session_id = format!("{}-{}", date, slug);

    let session_dir = memex_root
        .join("sources")
        .join("brainstorms")
        .join(&session_id);

    fs::create_dir_all(&session_dir)?;

    let created_at = now.to_rfc3339();
    let meta = json!({
        "session_id": session_id,
        "task": task,
        "created_at": created_at,
        "status": "in_progress"
    });

    let meta_path = session_dir.join("meta.json");
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&meta_path, meta_json)?;

    Ok((session_id, session_dir))
}

/// Appends a JSONL record to `{session_dir}/tool_calls.jsonl`.
pub fn log_tool_call(
    session_dir: &Path,
    tool_name: &str,
    args: &Value,
    result: &str,
) -> io::Result<()> {
    let preview: String = result.chars().take(500).collect();
    let timestamp = Utc::now().to_rfc3339();
    let record = json!({
        "timestamp": timestamp,
        "tool_name": tool_name,
        "args": args,
        "result_preview": preview,
    });

    let line = serde_json::to_string(&record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let jsonl_path = session_dir.join("tool_calls.jsonl");

    use io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&jsonl_path)?;
    writeln!(file, "{}", line)?;

    Ok(())
}

/// Writes `final-output.md` and updates `meta.json` status to "completed".
pub fn complete_session(session_dir: &Path, final_output: &str) -> io::Result<()> {
    let output_path = session_dir.join("final-output.md");
    fs::write(&output_path, final_output)?;

    let meta_path = session_dir.join("meta.json");
    let meta_bytes = fs::read(&meta_path)?;
    let mut meta: Value = serde_json::from_slice(&meta_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    meta["status"] = json!("completed");
    meta["completed_at"] = json!(Utc::now().to_rfc3339());

    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&meta_path, meta_json)?;

    Ok(())
}

/// Lists all brainstorm sessions under `{memex_root}/sources/brainstorms/`.
/// Returns a Vec of `(session_id, meta_json_value)` for sessions that have a valid `meta.json`.
pub fn list_sessions(memex_root: &Path) -> Vec<(String, Value)> {
    let brainstorms_dir = memex_root.join("sources").join("brainstorms");
    let mut sessions = Vec::new();

    let read_dir = match fs::read_dir(&brainstorms_dir) {
        Ok(rd) => rd,
        Err(_) => return sessions,
    };

    let mut entries: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("meta.json");
        if let Ok(bytes) = fs::read(&meta_path)
            && let Ok(meta) = serde_json::from_slice::<Value>(&bytes)
        {
            let session_id = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            sessions.push((session_id, meta));
        }
    }

    sessions
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_session_creates_dir_and_meta() {
        let tmp = TempDir::new().unwrap();
        let (session_id, session_dir) = create_session(tmp.path(), "Design a caching API").unwrap();

        assert!(session_dir.exists(), "session directory should exist");

        let meta_path = session_dir.join("meta.json");
        assert!(meta_path.exists(), "meta.json should exist");

        let meta_bytes = fs::read(&meta_path).unwrap();
        let meta: Value = serde_json::from_slice(&meta_bytes).unwrap();

        assert_eq!(meta["task"], "Design a caching API");
        assert_eq!(meta["status"], "in_progress");
        assert!(
            session_id.contains("design-a-caching-api"),
            "session_id '{}' should contain slug",
            session_id
        );
        assert_eq!(meta["session_id"], session_id);
    }

    #[test]
    fn log_tool_call_appends_jsonl() {
        let tmp = TempDir::new().unwrap();
        let (_, session_dir) = create_session(tmp.path(), "Test task").unwrap();

        let args = json!({"query": "rust async"});
        log_tool_call(&session_dir, "web_search", &args, "Some result text").unwrap();

        let jsonl_path = session_dir.join("tool_calls.jsonl");
        assert!(jsonl_path.exists(), "tool_calls.jsonl should exist");

        let content = fs::read_to_string(&jsonl_path).unwrap();
        let line = content.lines().next().unwrap();
        let record: Value = serde_json::from_str(line).unwrap();

        assert_eq!(record["tool_name"], "web_search");
        assert!(record["timestamp"].is_string());
        assert_eq!(record["result_preview"], "Some result text");
    }

    #[test]
    fn log_tool_call_truncates_result_preview() {
        let tmp = TempDir::new().unwrap();
        let (_, session_dir) = create_session(tmp.path(), "Truncation test").unwrap();

        let long_result = "x".repeat(1000);
        log_tool_call(&session_dir, "fetch", &json!({}), &long_result).unwrap();

        let content = fs::read_to_string(session_dir.join("tool_calls.jsonl")).unwrap();
        let record: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        let preview = record["result_preview"].as_str().unwrap();
        assert_eq!(preview.len(), 500);
    }

    #[test]
    fn complete_session_writes_output() {
        let tmp = TempDir::new().unwrap();
        let (_, session_dir) = create_session(tmp.path(), "My brainstorm task").unwrap();

        complete_session(&session_dir, "# Final output\n\nSome content.").unwrap();

        let output_path = session_dir.join("final-output.md");
        assert!(output_path.exists(), "final-output.md should exist");

        let content = fs::read_to_string(&output_path).unwrap();
        assert_eq!(content, "# Final output\n\nSome content.");

        let meta_bytes = fs::read(session_dir.join("meta.json")).unwrap();
        let meta: Value = serde_json::from_slice(&meta_bytes).unwrap();

        assert_eq!(meta["status"], "completed");
        assert!(
            meta["completed_at"].is_string(),
            "completed_at should be set"
        );
    }

    #[test]
    fn list_sessions_returns_all_valid_sessions() {
        let tmp = TempDir::new().unwrap();
        create_session(tmp.path(), "Task one").unwrap();
        create_session(tmp.path(), "Task two").unwrap();

        let sessions = list_sessions(tmp.path());
        assert_eq!(sessions.len(), 2);

        for (id, meta) in &sessions {
            assert!(!id.is_empty());
            assert!(meta["task"].is_string());
        }
    }

    #[test]
    fn list_sessions_empty_when_no_dir() {
        let tmp = TempDir::new().unwrap();
        let sessions = list_sessions(tmp.path());
        assert!(sessions.is_empty());
    }
}
