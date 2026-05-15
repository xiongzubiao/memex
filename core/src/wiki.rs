use std::path::{Path, PathBuf};

pub fn wiki_path_for_slug(wiki_dir: &Path, slug: &str) -> PathBuf {
    wiki_dir.join(format!("{slug}.md"))
}

/// Compose the canonical `---\n{yaml}---\n\n{body}` markdown a wiki
/// page is stored as. Caller is responsible for assembling the
/// `sources` list (typically: read prior frontmatter, append the new
/// source ref if absent) and for picking `created_at` (preserved from
/// prior page if any, else `now_dt`). `title` has its `\n\r` collapsed
/// to spaces so the YAML scalar stays single-line; serde_yaml escapes
/// the rest.
pub fn compose_wiki_markdown(
    title: &str,
    body: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    sources: &[String],
    collections: &[String],
    now_dt: chrono::DateTime<chrono::Utc>,
) -> Result<String, serde_yaml::Error> {
    let safe_title = title.replace(['\n', '\r'], " ");
    let yaml = serde_yaml::to_string(&crate::types::PageFrontmatterRef {
        title: &safe_title,
        summary: None,
        collections,
        created_at,
        updated_at: now_dt,
        sources,
    })?;
    Ok(format!("---\n{yaml}---\n\n{body}"))
}

pub fn normalize_slug(input: &str) -> String {
    let stripped = input.strip_suffix(".md").unwrap_or(input);
    let mut out = String::with_capacity(stripped.len());
    let mut prev_dash = true; // suppress leading dashes
    for ch in stripped.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn slug_to_path_appends_md() {
        let p = wiki_path_for_slug(&PathBuf::from("/m/wiki"), "auth-tokens");
        assert_eq!(p, PathBuf::from("/m/wiki/auth-tokens.md"));
    }

    #[test]
    fn normalize_slug_strips_md_extension() {
        assert_eq!(normalize_slug("auth-tokens.md"), "auth-tokens");
        assert_eq!(normalize_slug("auth-tokens"), "auth-tokens");
    }

    #[test]
    fn normalize_slug_kebabs_spaces_and_lowercases() {
        assert_eq!(normalize_slug("Auth Tokens"), "auth-tokens");
        assert_eq!(normalize_slug("REST  Patterns"), "rest-patterns");
    }

    #[test]
    fn normalize_slug_drops_non_ascii() {
        assert_eq!(normalize_slug("Café Notes"), "caf-notes");
    }

    #[test]
    fn compose_wiki_markdown_collapses_newlines_in_title() {
        let now = chrono::Utc::now();
        let out = compose_wiki_markdown(
            "Multi\nline\rtitle",
            "body text",
            now,
            &["session-1".into()],
            &[],
            now,
        )
        .unwrap();
        // Title's newlines collapsed to spaces in the YAML; body
        // appended verbatim after the closing `---`.
        assert!(out.contains("title: Multi line title"));
        assert!(out.ends_with("body text"));
        assert!(out.starts_with("---\n"));
    }
}
