use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::types::IndexEntry;

/// Placeholder text for an empty wiki index.
pub const EMPTY_INDEX_PLACEHOLDER: &str = "No wiki pages yet.";

/// Check if index content represents an empty wiki (blank or contains the placeholder).
pub fn is_empty_index(content: &str) -> bool {
    content.trim().is_empty() || content.contains(EMPTY_INDEX_PLACEHOLDER)
}

/// Parse index.md lines into entries.
/// Each line has the format: `- [Title](wiki/page.md) -- summary`
pub fn parse_index_entries(content: &str) -> Vec<IndexEntry> {
    let mut entries = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if !line.starts_with("- [") {
            continue;
        }

        // Extract title: between `[` and `]`
        let title_start = match line.find('[') {
            Some(i) => i + 1,
            None => continue,
        };
        let title_end = match line[title_start..].find(']') {
            Some(i) => title_start + i,
            None => continue,
        };
        let title = line[title_start..title_end].to_string();

        // Extract path: between `(` and `)`
        let path_start = match line[title_end..].find('(') {
            Some(i) => title_end + i + 1,
            None => continue,
        };
        let path_end = match line[path_start..].find(')') {
            Some(i) => path_start + i,
            None => continue,
        };
        let path = PathBuf::from(&line[path_start..path_end]);

        // Extract summary: after ` -- ` separator (also accept em dash `—` for back-compat)
        let rest = &line[path_end + 1..];
        let summary = if let Some(idx) = rest.find(" -- ") {
            rest[idx + 4..].trim().to_string()
        } else if let Some(idx) = rest.find('—') {
            rest[idx + '—'.len_utf8()..].trim().to_string()
        } else {
            String::new()
        };

        entries.push(IndexEntry {
            title,
            path,
            summary,
        });
    }

    entries
}

/// Read index.md and parse it into entries.
pub fn read_index(root: &Path) -> std::io::Result<Vec<IndexEntry>> {
    let index_path = root.join("index.md");
    let content = fs::read_to_string(&index_path)?;
    Ok(parse_index_entries(&content))
}

/// Rebuild index.md by scanning all .md files in the wiki/ directory.
/// Extracts title from YAML frontmatter and sorts entries alphabetically.
/// Returns "# Index\n\nNo wiki pages yet.\n" if no pages are found.
pub fn rebuild_index(root: &Path) -> crate::error::Result<String> {
    let wiki_dir = root.join("wiki");

    let mut entries: Vec<IndexEntry> = Vec::new();

    if wiki_dir.is_dir() {
        for entry in WalkDir::new(&wiki_dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        {
            let path = entry.path();
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let (title, summary) = match extract_title_and_summary(&content, 120) {
                Some(ts) => ts,
                None => continue,
            };

            // Make the path relative to root
            let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            entries.push(IndexEntry {
                title,
                path: rel_path,
                summary,
            });
        }
    }

    if entries.is_empty() {
        return Ok(format!("# Index\n\n{EMPTY_INDEX_PLACEHOLDER}\n"));
    }

    entries.sort_by(|a, b| a.title.cmp(&b.title));

    let mut output = String::from("# Index\n\n");
    for entry in &entries {
        let path_str = entry.path.to_string_lossy();
        output.push_str(&format!(
            "- [{}]({}) -- {}\n",
            entry.title, path_str, entry.summary
        ));
    }

    Ok(output)
}

/// Update or append a single entry in index.md for `page_path`.
///
/// Format: `- [Title](page_path) -- summary`
///
/// - If a line containing `](page_path)` already exists, it is replaced in-place.
/// - Otherwise the new entry is appended.
///
/// Creates index.md with a header if the file does not exist.
pub fn update_index_entry(
    index_path: &Path,
    page_path: &str,
    title: &str,
    summary: &str,
) -> std::io::Result<()> {
    let new_line = format!("- [{title}]({page_path}) -- {summary}");
    let needle = format!("]({page_path})");

    let existing = if index_path.exists() {
        fs::read_to_string(index_path)?
    } else {
        "# Index\n\n".to_string()
    };

    // Try to replace an existing entry for this path.
    let mut replaced = false;
    let mut new_lines: Vec<&str> = Vec::new();
    for line in existing.lines() {
        if line.contains(&needle) {
            new_lines.push(&new_line);
            replaced = true;
        } else if line == EMPTY_INDEX_PLACEHOLDER {
            // Drop the placeholder when we have real entries.
            // (skip this line)
        } else {
            new_lines.push(line);
        }
    }

    let mut output = new_lines.join("\n");
    if !output.ends_with('\n') {
        output.push('\n');
    }

    if !replaced {
        // Append new entry.
        output.push_str(&new_line);
        output.push('\n');
    }

    fs::write(index_path, output)
}

/// Update or append multiple entries in one read-write cycle.
/// Each entry is `(page_path, title, summary)`.
pub fn update_index_entries_batch(
    index_path: &Path,
    entries: &[(&str, &str, &str)],
) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut content = if index_path.exists() {
        fs::read_to_string(index_path)?
    } else {
        "# Index\n\n".to_string()
    };

    // Remove empty-wiki placeholder
    content = content.replace(&format!("{EMPTY_INDEX_PLACEHOLDER}\n"), "");

    for &(page_path, title, summary) in entries {
        let new_line = format!("- [{title}]({page_path}) -- {summary}");
        let needle = format!("]({page_path})");

        if let Some(start) = content.find(&needle) {
            // Find the full line containing this entry and replace it
            let line_start = content[..start].rfind('\n').map_or(0, |i| i + 1);
            let line_end = content[start..]
                .find('\n')
                .map_or(content.len(), |i| start + i);
            content.replace_range(line_start..line_end, &new_line);
        } else {
            // Append
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&new_line);
            content.push('\n');
        }
    }

    fs::write(index_path, content)
}

/// Remove the index entry for `page_path` from index.md.
///
/// Finds the line containing `](page_path)` and deletes it.
/// If no such line exists (or the file is absent) this is a no-op.
pub fn remove_index_entry(index_path: &Path, page_path: &str) -> std::io::Result<()> {
    if !index_path.exists() {
        return Ok(());
    }

    let needle = format!("]({page_path})");
    let content = fs::read_to_string(index_path)?;

    let filtered: Vec<&str> = content
        .lines()
        .filter(|line| !line.contains(&needle))
        .collect();

    let mut output = filtered.join("\n");
    if !output.ends_with('\n') {
        output.push('\n');
    }

    fs::write(index_path, output)
}

/// Return a rough token estimate for index.md (file_size / 4).
/// Returns 0 if the file does not exist.
pub fn count_index_tokens(root: &Path) -> std::io::Result<usize> {
    let index_path = root.join("index.md");
    match fs::metadata(&index_path) {
        Ok(meta) => Ok(meta.len() as usize / crate::model_catalog::BYTES_PER_TOKEN),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// Extract a one-line summary from a wiki page's body (after frontmatter).
///
/// Takes the first non-empty, non-heading line and truncates to `max_len` chars.
pub fn extract_summary(content: &str, max_len: usize) -> String {
    // Skip frontmatter
    let body = if content.trim_start().starts_with("---") {
        let after_open = &content.trim_start()[3..];
        match after_open.find("\n---") {
            Some(idx) => &after_open[idx + 4..],
            None => content,
        }
    } else {
        content
    };

    // Find first non-empty, non-heading line
    let summary = body
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");

    if summary.len() <= max_len {
        summary.to_string()
    } else {
        // Truncate at word boundary
        match summary[..max_len].rfind(' ') {
            Some(idx) => format!("{}...", &summary[..idx]),
            None => format!("{}...", &summary[..max_len]),
        }
    }
}

/// Extract title and summary from a wiki page in a single frontmatter parse.
///
/// Uses the full YAML parser first, falling back to a lightweight line-scan
/// for the title when the strict parser rejects slightly malformed YAML.
/// Summary falls back to the first body line (truncated to `max_summary_len`).
pub fn extract_title_and_summary(
    content: &str,
    max_summary_len: usize,
) -> Option<(String, String)> {
    // Try the canonical parser first — one parse for both fields
    if let Ok((fm, _body)) = crate::validate::parse_frontmatter(content)
        && !fm.title.trim().is_empty()
    {
        let summary = fm
            .summary
            .unwrap_or_else(|| extract_summary(content, max_summary_len));
        return Some((fm.title, summary));
    }

    // Fallback: lightweight line-scan for title, body heuristic for summary
    let title = extract_title_line_scan(content)?;
    let summary = extract_summary(content, max_summary_len);
    Some((title, summary))
}

/// Lightweight line-scan fallback for title extraction.
fn extract_title_line_scan(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_open = &trimmed[3..];
    let end_idx = after_open.find("\n---")?;
    for line in after_open[..end_idx].lines() {
        if let Some(rest) = line.trim().strip_prefix("title:") {
            let title = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_index_entries_standard() {
        let content = "# Index\n\n- [Alpha](wiki/alpha.md) — First entry\n- [Beta](wiki/beta.md) — Second entry\n";
        let entries = parse_index_entries(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "Alpha");
        assert_eq!(entries[0].path, PathBuf::from("wiki/alpha.md"));
        assert_eq!(entries[0].summary, "First entry");
        assert_eq!(entries[1].title, "Beta");
        assert_eq!(entries[1].path, PathBuf::from("wiki/beta.md"));
        assert_eq!(entries[1].summary, "Second entry");
    }

    #[test]
    fn parse_index_entries_empty() {
        let content = "# Index\n\nNo wiki pages yet.\n";
        let entries = parse_index_entries(content);
        assert!(entries.is_empty());
    }

    #[test]
    fn rebuild_index_empty_wiki() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("wiki")).unwrap();

        let result = rebuild_index(dir.path()).unwrap();
        assert!(result.contains("No wiki pages yet"));
    }

    #[test]
    fn rebuild_index_with_pages() {
        let dir = TempDir::new().unwrap();
        let wiki_dir = dir.path().join("wiki");
        fs::create_dir_all(&wiki_dir).unwrap();

        let page_content = "---\ntitle: My Test Page\ntags:\n  - concept\ncreated: 2024-01-01T00:00:00Z\nlast_updated: 2024-01-01T00:00:00Z\n---\n\nBody text here.\n";
        fs::write(wiki_dir.join("my-test-page.md"), page_content).unwrap();

        let result = rebuild_index(dir.path()).unwrap();
        assert!(result.contains("My Test Page"), "got: {result}");
        assert!(result.contains("wiki/my-test-page.md"), "got: {result}");
    }

    #[test]
    fn count_tokens_empty() {
        let dir = TempDir::new().unwrap();
        let count = count_index_tokens(dir.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_tokens_nonzero() {
        let dir = TempDir::new().unwrap();
        let content = "x".repeat(400);
        fs::write(dir.path().join("index.md"), &content).unwrap();
        let count = count_index_tokens(dir.path()).unwrap();
        assert_eq!(count, 100);
    }

    #[test]
    fn update_index_entry_appends_new() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index.md");
        fs::write(
            &index_path,
            "# Index\n\n- [Alpha](wiki/alpha.md) -- first\n",
        )
        .unwrap();

        update_index_entry(&index_path, "wiki/beta.md", "Beta", "second entry").unwrap();

        let content = fs::read_to_string(&index_path).unwrap();
        assert!(
            content.contains("- [Beta](wiki/beta.md) -- second entry"),
            "got: {content}"
        );
        // Original entry must still be present
        assert!(
            content.contains("- [Alpha](wiki/alpha.md) -- first"),
            "got: {content}"
        );
    }

    #[test]
    fn update_index_entry_replaces_existing() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index.md");
        fs::write(
            &index_path,
            "# Index\n\n- [OldTitle](wiki/page.md) -- old summary\n",
        )
        .unwrap();

        update_index_entry(&index_path, "wiki/page.md", "NewTitle", "new summary").unwrap();

        let content = fs::read_to_string(&index_path).unwrap();
        assert!(
            content.contains("- [NewTitle](wiki/page.md) -- new summary"),
            "got: {content}"
        );
        // Old entry must be gone
        assert!(
            !content.contains("OldTitle"),
            "old title should be replaced; got: {content}"
        );
        // Only one entry for this path
        assert_eq!(
            content.matches("wiki/page.md").count(),
            1,
            "duplicate entries found; got: {content}"
        );
    }

    #[test]
    fn rebuild_index_prefers_frontmatter_summary() {
        let dir = TempDir::new().unwrap();
        let wiki_dir = dir.path().join("wiki");
        fs::create_dir_all(&wiki_dir).unwrap();

        let page = "---\ntitle: Auth Tokens\nsummary: How bearer tokens work in OAuth2 flows.\ntags:\n  - concept\ncreated: 2024-01-01T00:00:00Z\nlast_updated: 2024-01-01T00:00:00Z\n---\n\nA very long first paragraph that would normally be truncated if used as the summary because it keeps going and going well past 120 characters without stopping at any reasonable point.\n";
        fs::write(wiki_dir.join("auth-tokens.md"), page).unwrap();

        let result = rebuild_index(dir.path()).unwrap();
        assert!(
            result.contains("How bearer tokens work in OAuth2 flows."),
            "should use frontmatter summary, got: {result}"
        );
        assert!(
            !result.contains("very long first paragraph"),
            "should NOT fall back to body heuristic, got: {result}"
        );
    }

    #[test]
    fn rebuild_index_falls_back_to_body_without_summary() {
        let dir = TempDir::new().unwrap();
        let wiki_dir = dir.path().join("wiki");
        fs::create_dir_all(&wiki_dir).unwrap();

        // No summary field in frontmatter
        let page = "---\ntitle: Legacy Page\ntags:\n  - entity\ncreated: 2024-01-01T00:00:00Z\nlast_updated: 2024-01-01T00:00:00Z\n---\n\nThis is the body text.\n";
        fs::write(wiki_dir.join("legacy.md"), page).unwrap();

        let result = rebuild_index(dir.path()).unwrap();
        assert!(
            result.contains("This is the body text."),
            "should fall back to body heuristic, got: {result}"
        );
    }

    #[test]
    fn remove_index_entry_deletes_line() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index.md");
        fs::write(
            &index_path,
            "# Index\n\n- [Alpha](wiki/alpha.md) -- first\n- [Beta](wiki/beta.md) -- second\n",
        )
        .unwrap();

        remove_index_entry(&index_path, "wiki/alpha.md").unwrap();

        let content = fs::read_to_string(&index_path).unwrap();
        assert!(
            !content.contains("wiki/alpha.md"),
            "alpha should be removed; got: {content}"
        );
        assert!(
            content.contains("- [Beta](wiki/beta.md) -- second"),
            "beta should remain; got: {content}"
        );
    }
}
