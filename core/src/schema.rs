pub const SCHEMA_VERSION: u32 = 1;

pub const DEFAULT_SCHEMA: &str = r#"---
version: 1
---

# Memex Wiki Schema

## Page Naming
- Use lowercase-kebab-case (e.g., `caching-strategies.md`)
- Source summaries: `{source-name}.md` (e.g., `notes-md.md`)
- Brainstorms: `{date}-{topic}.md` (e.g., `2026-04-05-api-gateway.md`)

## Required Frontmatter
```yaml
title: Human-readable title
tags:
  - entity
created: ISO-8601 timestamp
last_updated: ISO-8601 timestamp
sources:
  - sources/documents/abc123-notes.md
```

## Tags
Free-form tags describing page content. Common tags:
- **entity**: A specific thing (technology, pattern, tool)
- **concept**: An abstract idea or principle

Reserved tags (set automatically):
- **brainstorm**: Results of a brainstorming session
- **contradiction**: Conflicting claims from different sources

## Cross-References
- Use `[[wiki links]]` to reference other pages
- Link when pages share a meaningful relationship

## When to Create vs Update
- Create: new topic not covered by existing pages
- Update: new information about an existing topic
- Contradiction: when new info conflicts with existing page
"#;

/// Parse the schema version from schema.md content.
pub fn parse_schema_version(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    let yaml_block = &after_first[..end];
    for line in yaml_block.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("version:") {
            return value.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_schema_has_correct_version() {
        assert_eq!(parse_schema_version(DEFAULT_SCHEMA), Some(SCHEMA_VERSION));
    }

    #[test]
    fn parse_version_from_valid_schema() {
        let content = "---\nversion: 3\n---\n# Schema\n";
        assert_eq!(parse_schema_version(content), Some(3));
    }

    #[test]
    fn parse_version_missing_frontmatter() {
        assert_eq!(parse_schema_version("# No frontmatter"), None);
    }

    #[test]
    fn parse_version_missing_version_field() {
        let content = "---\ntitle: foo\n---\n";
        assert_eq!(parse_schema_version(content), None);
    }

    #[test]
    fn parse_version_invalid_number() {
        let content = "---\nversion: abc\n---\n";
        assert_eq!(parse_schema_version(content), None);
    }
}
