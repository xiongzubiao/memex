use crate::error::{MemexError, Result};
use crate::types::PageFrontmatter;
use std::path::Path;

/// Parse YAML frontmatter from a wiki page string.
/// Returns (frontmatter, body) or error if frontmatter is missing/invalid.
pub fn parse_frontmatter(content: &str) -> Result<(PageFrontmatter, String)> {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return Err(MemexError::ValidationFailure {
            details: "Page missing YAML frontmatter (must start with ---)".to_string(),
        });
    }
    let after_first = &trimmed[3..];
    let end = after_first
        .find("---")
        .ok_or_else(|| MemexError::ValidationFailure {
            details: "Unclosed frontmatter (missing closing ---)".to_string(),
        })?;
    let yaml_str = &after_first[..end];
    let body = after_first[end + 3..].trim().to_string();
    // Try parsing YAML as-is first
    let fm: PageFrontmatter = match serde_yaml::from_str(yaml_str) {
        Ok(fm) => fm,
        Err(_) => {
            // Common LLM issue: unquoted colons in title (e.g., "title: Go: Deep Equal")
            // Fix by quoting the title value
            let fixed = fix_yaml_title_colons(yaml_str);
            serde_yaml::from_str(&fixed).map_err(|e| MemexError::ValidationFailure {
                details: format!("Invalid frontmatter YAML: {e}"),
            })?
        }
    };
    Ok((fm, body))
}

/// Validate a complete wiki page (frontmatter + body).
pub fn validate_page(content: &str) -> Result<PageFrontmatter> {
    let (fm, _body) = parse_frontmatter(content)?;
    if fm.title.trim().is_empty() {
        return Err(MemexError::ValidationFailure {
            details: "Page title is empty".to_string(),
        });
    }
    Ok(fm)
}

/// Extract all [[wiki links]] from content.
pub fn extract_wiki_links(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' && chars.peek() == Some(&'[') {
            chars.next(); // consume second [
            let mut link = String::new();
            loop {
                match chars.next() {
                    None => break,
                    Some(']') => {
                        if chars.peek() == Some(&']') {
                            chars.next();
                            if !link.is_empty() {
                                links.push(link);
                            }
                            break;
                        } else {
                            link.push(']');
                        }
                    }
                    Some(c2) => link.push(c2),
                }
            }
        }
    }
    links
}

/// Fix YAML title lines that have unquoted colons (common LLM output issue).
/// e.g., "title: Go: Deep Equal" → "title: \"Go: Deep Equal\""
fn fix_yaml_title_colons(yaml: &str) -> String {
    yaml.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("title:") {
                let value = trimmed.strip_prefix("title:").unwrap().trim();
                // If value contains a colon and isn't already quoted, quote it
                if value.contains(':') && !value.starts_with('"') && !value.starts_with('\'') {
                    return format!("title: \"{}\"", value.replace('"', "\\\""));
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Validate wiki links: check that targets exist as .md files in wiki/.
/// Returns list of dangling links (warnings, not errors).
pub fn find_dangling_links(content: &str, wiki_dir: &Path) -> Vec<String> {
    extract_wiki_links(content)
        .into_iter()
        .filter(|link| {
            // Reject links with path traversal or directory separators.
            !link.contains("..")
                && !link.contains('/')
                && !wiki_dir.join(format!("{link}.md")).exists()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const VALID_PAGE: &str = "---\ntitle: Test Page\ntags:\n  - entity\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources:\n  - sources/documents/abc.md\n---\n\nPage body here.\n";

    #[test]
    fn parse_valid_frontmatter() {
        let (fm, body) = parse_frontmatter(VALID_PAGE).unwrap();
        assert_eq!(fm.title, "Test Page");
        assert_eq!(fm.tags, vec!["entity"]);
        assert!(body.contains("Page body"));
    }

    #[test]
    fn parse_missing_frontmatter() {
        let err = parse_frontmatter("No frontmatter here").unwrap_err();
        assert!(format!("{err}").contains("missing YAML frontmatter"));
    }

    #[test]
    fn parse_unclosed_frontmatter() {
        let err = parse_frontmatter("---\ntitle: Oops\n").unwrap_err();
        assert!(format!("{err}").contains("Unclosed frontmatter"));
    }

    #[test]
    fn parse_invalid_yaml() {
        let err = parse_frontmatter("---\n: invalid: yaml:\n---\nbody").unwrap_err();
        assert!(format!("{err}").contains("Invalid frontmatter YAML"));
    }

    #[test]
    fn validate_page_rejects_empty_title() {
        let page = "---\ntitle: \"\"\ntags:\n  - entity\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\nbody";
        let err = validate_page(page).unwrap_err();
        assert!(format!("{err}").contains("title is empty"));
    }

    #[test]
    fn extract_wiki_links_finds_links() {
        let content = "See [[caching-strategies]] and [[auth-patterns]] for details.";
        let links = extract_wiki_links(content);
        assert_eq!(links, vec!["caching-strategies", "auth-patterns"]);
    }

    #[test]
    fn extract_wiki_links_no_links() {
        assert!(extract_wiki_links("No links here.").is_empty());
    }

    #[test]
    fn extract_wiki_links_ignores_single_brackets() {
        assert!(extract_wiki_links("array[0] and [markdown](link)").is_empty());
    }

    #[test]
    fn find_dangling_links_detects_missing() {
        let dir = TempDir::new().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("caching.md"), "exists").unwrap();
        let dangling = find_dangling_links("See [[caching]] and [[nonexistent]]", &wiki);
        assert_eq!(dangling, vec!["nonexistent"]);
    }

    #[test]
    fn find_dangling_links_all_valid() {
        let dir = TempDir::new().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("foo.md"), "exists").unwrap();
        assert!(find_dangling_links("See [[foo]]", &wiki).is_empty());
    }

    #[test]
    fn parse_frontmatter_with_colon_in_title() {
        let page = "---\ntitle: Go: Deep Equal Comparison\ntags:\n  - entity\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\nBody.\n";
        let (fm, _) = parse_frontmatter(page).unwrap();
        assert_eq!(fm.title, "Go: Deep Equal Comparison");
    }

    #[test]
    fn fix_yaml_colons_quotes_title() {
        let yaml = "title: Go: Deep Equal\ntags:\n  - entity";
        let fixed = fix_yaml_title_colons(yaml);
        assert!(fixed.contains("\"Go: Deep Equal\""));
    }
}
