use std::path::{Path, PathBuf};

pub fn wiki_path_for_slug(wiki_dir: &Path, slug: &str) -> PathBuf {
    wiki_dir.join(format!("{slug}.md"))
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
    while out.ends_with('-') { out.pop(); }
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
}
