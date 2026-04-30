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
    #[serde(default)]
    pub collections: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub sources: Vec<String>,
}

/// Borrow-friendly serialization mirror of `PageFrontmatter`. Same field
/// order, types, and skip rules, so `serde_yaml::to_string(&PageFrontmatterRef)`
/// produces identical output to the owned form — callers building many
/// frontmatters per ingest can avoid cloning shared arrays.
#[derive(Debug, Serialize)]
pub struct PageFrontmatterRef<'a> {
    pub title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<&'a str>,
    pub tags: &'a [String],
    pub collections: &'a [String],
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub sources: &'a [String],
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
    /// Raw file's body sha256 doesn't match the hash encoded in its
    /// path: external body edit to a `raw/<hh>/<rest>` file invalidates
    /// the content-addressed name. `lint --fix` repairs by renaming the
    /// file to its new body hash, dropping old chunks/embeddings, and
    /// re-embedding at the new hash. `target` carries the recomputed
    /// body hash so the fix path doesn't re-read the file.
    RawHashMismatch,
}

/// A document in the content-addressable store (wiki or raw).
#[derive(Debug, Clone)]
pub struct Document {
    pub id: i64,
    pub doc_type: String,
    pub path: String,
    pub title: String,
    pub hash: String,
    pub tags: String,
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
            collections: vec!["default".to_string()],
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            sources: vec!["sources/documents/abc.md".to_string()],
        };
        let yaml = serde_yaml::to_string(&fm).unwrap();
        let back: PageFrontmatter = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.title, "Test");
        assert_eq!(back.tags, vec!["entity"]);
    }

    /// Locks in the YAML shape that `cli/src/daemon/handler.rs` depends on
    /// when wrapping serialized output in `---\n{yaml}---\n\n{body}`.
    #[test]
    fn frontmatter_serialized_shape_is_stable() {
        let fm = PageFrontmatter {
            title: "T".to_string(),
            summary: None,
            tags: vec![],
            collections: vec![],
            created_at: "2026-04-24T00:00:00Z".parse().unwrap(),
            updated_at: "2026-04-24T00:00:00Z".parse().unwrap(),
            sources: vec![],
        };
        let yaml = serde_yaml::to_string(&fm).unwrap();
        assert!(
            !yaml.contains("summary:"),
            "summary must be omitted when None: {yaml}"
        );
        assert!(yaml.ends_with('\n'), "must end with newline: {yaml:?}");
        assert!(yaml.contains("tags: []"), "empty tags render as []: {yaml}");
    }

    #[test]
    fn frontmatter_tags_default_to_empty() {
        let yaml = "title: No Tags\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n";
        let fm: PageFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert!(fm.tags.is_empty());
        assert!(fm.collections.is_empty());
    }
}
