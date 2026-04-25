pub mod daemon;

use std::path::PathBuf;

pub fn memex_root() -> PathBuf {
    if let Ok(root) = std::env::var("MEMEX_ROOT") {
        return PathBuf::from(root);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".memex")
}

/// Normalize a title to a kebab-case slug for wiki page filenames.
pub fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    slug.split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
