use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{MemexError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RawFrontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converter: Option<String>,
}

/// True for `http://` and `https://` source paths. Used to distinguish a
/// URL ingest from a local-file ingest in raw frontmatter (`source_kind`)
/// and downstream displays.
pub fn is_url(source_path: &str) -> bool {
    source_path.starts_with("http://") || source_path.starts_with("https://")
}

pub fn raw_path_for_hash(raw_dir: &Path, body_hash: &str) -> PathBuf {
    debug_assert!(body_hash.len() >= 3, "body hash too short");
    raw_dir.join(&body_hash[..2]).join(&body_hash[2..])
}

pub fn parse_raw_frontmatter(file: &str) -> Result<(RawFrontmatter, &str)> {
    let (yaml, body) = crate::storage::split_frontmatter(file).ok_or_else(|| {
        MemexError::Other(anyhow::anyhow!(
            "raw file missing leading or closing frontmatter fence"
        ))
    })?;
    let fm: RawFrontmatter = serde_yaml::from_str(yaml)
        .map_err(|e| MemexError::Other(anyhow::anyhow!("invalid raw frontmatter YAML: {e}")))?;
    Ok((fm, body))
}

pub fn assemble_raw_file(fm: &RawFrontmatter, body: &str) -> String {
    let yaml = serde_yaml::to_string(fm).unwrap_or_default();
    format!("---\n{yaml}---\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn path_for_hash_uses_two_char_fanout() {
        let h = "abcd0123456789abcd0123456789abcd0123456789abcd0123456789abcd0123";
        assert_eq!(
            raw_path_for_hash(&PathBuf::from("/m/raw"), h),
            PathBuf::from(
                "/m/raw/ab/cd0123456789abcd0123456789abcd0123456789abcd0123456789abcd0123"
            ),
        );
    }

    #[test]
    fn parse_raw_frontmatter_extracts_source_and_title() {
        let file = "---\nsource: https://x/p\nsource_kind: url\ningested_at: 2026-04-26T10:00:00Z\nconverter: markitdown\ntitle: P Title\n---\n\nbody bytes...";
        let (fm, body) = parse_raw_frontmatter(file).unwrap();
        assert_eq!(fm.source.as_deref(), Some("https://x/p"));
        assert_eq!(fm.title.as_deref(), Some("P Title"));
        assert_eq!(body, "body bytes...");
    }

    #[test]
    fn parse_raw_frontmatter_no_frontmatter_errors() {
        let err = parse_raw_frontmatter("body only\n").unwrap_err();
        assert!(err.to_string().contains("frontmatter"));
    }

    #[test]
    fn assemble_raw_file_serializes_yaml_then_body() {
        let fm = RawFrontmatter {
            source: Some("https://x".into()),
            source_kind: Some("url".into()),
            ingested_at: Some("2026-04-26T10:00:00Z".into()),
            converter: Some("markitdown".into()),
            title: Some("T".into()),
        };
        let s = assemble_raw_file(&fm, "body bytes");
        assert!(s.starts_with("---\n"));
        assert!(s.contains("source: https://x"));
        assert!(s.contains("title: T"));
        assert!(s.ends_with("body bytes"));
    }
}
