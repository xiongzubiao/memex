use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Page frontmatter (YAML between --- delimiters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageFrontmatter {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub sources: Vec<String>,
}

/// A wiki page with parsed content.
#[derive(Debug, Clone)]
pub struct WikiPage {
    pub path: PathBuf,
    pub frontmatter: PageFrontmatter,
    pub body: String,
}

/// Index entry (one line in index.md).
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub title: String,
    pub path: PathBuf,
    pub summary: String,
}

/// Report from a lint operation.
#[derive(Debug, Clone, Default)]
pub struct LintReport {
    pub issues: Vec<LintIssue>,
}

/// A single lint issue.
#[derive(Debug, Clone)]
pub struct LintIssue {
    pub kind: LintIssueKind,
    pub page: String,
    pub target: String,
}

/// Categories of lint issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LintIssueKind {
    DanglingLink,
    MissingLink,
    StaleIndex,
    UntrackedFile,
    MissingFile,
    OutdatedEmbedding,
}

/// A document in the content-addressable store (wiki or source).
#[derive(Debug, Clone)]
pub struct Document {
    pub id: i64,
    pub collection: String,
    pub path: String,
    pub title: String,
    pub hash: String,
    pub docid: String,
    pub tags: String,
    pub summary: String,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_roundtrip() {
        let fm = PageFrontmatter {
            title: "Test".to_string(),
            summary: Some("A test page.".to_string()),
            tags: vec!["entity".to_string()],
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            sources: vec!["sources/documents/abc.md".to_string()],
        };
        let yaml = serde_yaml::to_string(&fm).unwrap();
        let back: PageFrontmatter = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.title, "Test");
        assert_eq!(back.tags, vec!["entity"]);
    }

    #[test]
    fn frontmatter_tags_default_to_empty() {
        let yaml = "title: No Tags\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n";
        let fm: PageFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert!(fm.tags.is_empty());
    }
}
