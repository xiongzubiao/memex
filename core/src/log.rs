use chrono::Utc;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Log rotation threshold in bytes (1 MB).
pub const LOG_ROTATE_BYTES: u64 = 1_048_576;

/// Append a timestamped operation entry to log.md.
///
/// Format: `## [{timestamp}] {operation} | {subject} — {details}\n`
/// If `details` is empty, the ` — {details}` part is omitted.
pub fn append_log(
    root: &Path,
    operation: &str,
    subject: &str,
    details: &str,
) -> std::io::Result<()> {
    let log_path = root.join("log.md");
    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = if details.is_empty() {
        format!("## [{timestamp}] {operation} | {subject}\n")
    } else {
        format!("## [{timestamp}] {operation} | {subject} \u{2014} {details}\n")
    };

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Rotate log.md if it exceeds LOG_ROTATE_BYTES.
/// Renames the current log to `log-{date}.md` and creates a fresh empty log.md.
/// Returns `true` if rotation occurred, `false` otherwise.
pub fn rotate_log_if_needed(root: &Path) -> std::io::Result<bool> {
    let log_path = root.join("log.md");

    let metadata = match fs::metadata(&log_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    if metadata.len() < LOG_ROTATE_BYTES {
        return Ok(false);
    }

    let date = Utc::now().format("%Y-%m-%d");
    let archive_name = format!("log-{date}.md");
    let archive_path = root.join(&archive_name);

    fs::rename(&log_path, &archive_path)?;
    fs::write(&log_path, b"")?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn append_log_creates_grep_friendly_entry() {
        let dir = TempDir::new().unwrap();
        append_log(dir.path(), "ingest", "batch", "5 created, 2 updated").unwrap();

        let log_path = dir.path().join("log.md");
        assert!(log_path.exists());
        let content = fs::read_to_string(&log_path).unwrap();
        assert!(
            content.starts_with("## ["),
            "entry should start with '## [', got: {content}"
        );
        assert!(content.contains("ingest | batch"), "got: {content}");
        assert!(
            content.contains("\u{2014} 5 created, 2 updated"),
            "got: {content}"
        );
    }

    #[test]
    fn append_log_without_details() {
        let dir = TempDir::new().unwrap();
        append_log(dir.path(), "reindex", "858 pages", "").unwrap();

        let content = fs::read_to_string(dir.path().join("log.md")).unwrap();
        assert!(content.contains("reindex | 858 pages"), "got: {content}");
        assert!(
            !content.contains('\u{2014}'),
            "should not contain em dash when details is empty, got: {content}"
        );
    }

    #[test]
    fn append_log_multiple_entries_greppable() {
        let dir = TempDir::new().unwrap();
        append_log(dir.path(), "ingest", "batch", "3 created, 0 updated").unwrap();
        append_log(
            dir.path(),
            "query",
            "\"What is caching?\"",
            "4 pages referenced",
        )
        .unwrap();
        append_log(dir.path(), "lint", "5 issues", "3 fixable").unwrap();

        let content = fs::read_to_string(dir.path().join("log.md")).unwrap();

        // Every entry must match the grep-friendly `## [` prefix
        let grep_matches: Vec<&str> = content
            .lines()
            .filter(|line| line.starts_with("## ["))
            .collect();
        assert_eq!(
            grep_matches.len(),
            3,
            "expected 3 grep-friendly lines, got: {content}"
        );
    }

    #[test]
    fn rotate_log_no_rotation_when_small() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("log.md"), "small content").unwrap();

        let rotated = rotate_log_if_needed(dir.path()).unwrap();
        assert!(!rotated);
    }

    #[test]
    fn rotate_log_rotates_when_large() {
        let dir = TempDir::new().unwrap();
        // Write content exceeding LOG_ROTATE_BYTES
        let large_content = "x".repeat((LOG_ROTATE_BYTES + 1) as usize);
        fs::write(dir.path().join("log.md"), &large_content).unwrap();

        let rotated = rotate_log_if_needed(dir.path()).unwrap();
        assert!(rotated);

        // New log.md should exist and be empty
        let new_log = dir.path().join("log.md");
        assert!(new_log.exists());
        let new_content = fs::read_to_string(&new_log).unwrap();
        assert!(
            new_content.is_empty(),
            "log.md should be empty after rotation"
        );

        // An archive file should exist
        let archive_exists = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                name_str.starts_with("log-") && name_str.ends_with(".md") && name_str != "log.md"
            });
        assert!(archive_exists, "archive file should exist after rotation");
    }
}
