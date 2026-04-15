use crate::error::{MemexError, Result};
use crate::types::{PageAction, ProposedPage};
use std::path::PathBuf;
use tracing::warn;

/// Parse LLM output that proposes wiki pages.
pub fn parse_llm_wiki_output(output: &str) -> Result<Vec<ProposedPage>> {
    let mut pages = Vec::new();
    let mut remaining = output;

    while let Some(page_start) = remaining.find("<<< PAGE:") {
        remaining = &remaining[page_start..];

        // Extract path
        let path_start = "<<< PAGE:".len();
        let path_end = remaining
            .find(">>>")
            .ok_or_else(|| MemexError::ValidationFailure {
                details: "Unclosed <<< PAGE: tag".to_string(),
            })?;
        let path_str = remaining[path_start..path_end].trim();
        let path = PathBuf::from(path_str);
        remaining = &remaining[path_end + 3..];

        // Extract action (optional, defaults to Create)
        let action = if let Some(action_start) = remaining.find("<<< ACTION:") {
            // Only parse if ACTION comes before next PAGE or END PAGE
            let next_page = remaining.find("<<< PAGE:");
            if next_page.is_none() || action_start < next_page.unwrap() {
                let after = &remaining[action_start + "<<< ACTION:".len()..];
                let action_end =
                    after
                        .find(">>>")
                        .ok_or_else(|| MemexError::ValidationFailure {
                            details: "Unclosed <<< ACTION: tag".to_string(),
                        })?;
                let action_str = after[..action_end].trim().to_lowercase();
                remaining = &after[action_end + 3..];
                match action_str.as_str() {
                    "update" => PageAction::Update,
                    _ => PageAction::Create,
                }
            } else {
                PageAction::Create
            }
        } else {
            PageAction::Create
        };

        // Extract content until <<< END PAGE >>>, or next <<< PAGE:, or EOF
        let end_marker = "<<< END PAGE >>>";
        let end_pos = remaining.find(end_marker);
        let next_page_pos = remaining.find("<<< PAGE:");

        let (content_end, skip_len) = if let Some(end) = end_pos {
            // Prefer <<< END PAGE >>> if it comes before the next <<< PAGE:
            if let Some(next) = next_page_pos
                && next < end
            {
                // Next page starts before end marker (malformed), use next page as boundary
                warn!(
                    page = path_str,
                    "missing <<< END PAGE >>>, using next page boundary"
                );
                (next, next)
            } else {
                (end, end + end_marker.len())
            }
        } else if let Some(next) = next_page_pos {
            // No end marker, but there's another page. Use it as boundary.
            warn!(
                page = path_str,
                "missing <<< END PAGE >>>, using next page boundary"
            );
            (next, next)
        } else {
            // No end marker, no next page. Use everything remaining.
            warn!(
                page = path_str,
                "missing <<< END PAGE >>>, using end of output"
            );
            (remaining.len(), remaining.len())
        };

        let content = remaining[..content_end].trim().to_string();
        remaining = &remaining[skip_len..];

        pages.push(ProposedPage {
            path,
            action,
            content,
        });
    }

    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_page() {
        let output = r#"
<<< PAGE: wiki/caching.md >>>
<<< ACTION: create >>>
---
title: Caching Strategies
tags:
  - entity
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/abc-notes.md
---

Caching improves performance by storing frequently accessed data.

See also [[rate-limiting]].
<<< END PAGE >>>
"#;
        let pages = parse_llm_wiki_output(output).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].path.to_string_lossy(), "wiki/caching.md");
        assert_eq!(pages[0].action, PageAction::Create);
        assert!(pages[0].content.contains("Caching improves"));
        assert!(pages[0].content.contains("[[rate-limiting]]"));
    }

    #[test]
    fn parse_multiple_pages() {
        let output = r#"
<<< PAGE: wiki/caching.md >>>
<<< ACTION: create >>>
---
title: Caching
tags:
  - entity
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources: []
---

Content about caching.
<<< END PAGE >>>

<<< PAGE: wiki/notes-md.md >>>
<<< ACTION: create >>>
---
title: Notes Summary
tags:
  - source-summary
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/abc-notes.md
---

Summary of the notes document.
<<< END PAGE >>>
"#;
        let pages = parse_llm_wiki_output(output).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].path.to_string_lossy(), "wiki/caching.md");
        assert_eq!(pages[1].path.to_string_lossy(), "wiki/notes-md.md");
    }

    #[test]
    fn parse_update_action() {
        let output = "<<< PAGE: wiki/existing.md >>>\n<<< ACTION: update >>>\n---\ntitle: Updated\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\nNew content.\n<<< END PAGE >>>";
        let pages = parse_llm_wiki_output(output).unwrap();
        assert_eq!(pages[0].action, PageAction::Update);
    }

    #[test]
    fn parse_empty_output() {
        let pages = parse_llm_wiki_output("No pages proposed.").unwrap();
        assert!(pages.is_empty());
    }

    #[test]
    fn parse_missing_end_marker_uses_eof() {
        let output = "<<< PAGE: wiki/broken.md >>>\n<<< ACTION: create >>>\n---\ntitle: Broken\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\ncontent without end marker";
        let pages = parse_llm_wiki_output(output).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].content.contains("content without end marker"));
    }

    #[test]
    fn parse_missing_end_marker_uses_next_page() {
        let output = "<<< PAGE: wiki/first.md >>>\n<<< ACTION: create >>>\n---\ntitle: First\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\nFirst content\n<<< PAGE: wiki/second.md >>>\n<<< ACTION: create >>>\n---\ntitle: Second\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\nSecond content\n<<< END PAGE >>>";
        let pages = parse_llm_wiki_output(output).unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages[0].content.contains("First content"));
        assert!(pages[1].content.contains("Second content"));
    }
}
