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

    #[test]
    fn extract_summary_basic() {
        let content = "---\ntitle: Test
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\n---\n\nThis is the body text.\n";
        let summary = extract_summary(content, 120);
        assert_eq!(summary, "This is the body text.");
    }

    #[test]
    fn extract_summary_truncates() {
        let content = "---\ntitle: T
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\n---\n\nWord1 word2 word3 word4 word5\n";
        let summary = extract_summary(content, 15);
        assert!(summary.len() <= 20, "got: {summary}");
    }

    #[test]
    fn extract_title_and_summary_from_frontmatter() {
        let content = "---\ntitle: Auth Tokens\nsummary: How bearer tokens work.
created_at: 2024-01-01T00:00:00Z\nupdated_at: 2024-01-01T00:00:00Z\n---\n\nBody text here.\n";
        let (title, summary) = extract_title_and_summary(content, 120).unwrap();
        assert_eq!(title, "Auth Tokens");
        assert_eq!(summary, "How bearer tokens work.");
    }

    #[test]
    fn extract_title_and_summary_fallback() {
        let content = "---\ntitle: Legacy Page
created_at: 2024-01-01T00:00:00Z\nupdated_at: 2024-01-01T00:00:00Z\n---\n\nThis is the body text.\n";
        let (title, summary) = extract_title_and_summary(content, 120).unwrap();
        assert_eq!(title, "Legacy Page");
        assert_eq!(summary, "This is the body text.");
    }
}
