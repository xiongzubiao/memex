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
    pub created: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
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

/// A raw source to ingest.
#[derive(Debug, Clone)]
pub enum Source {
    File { path: PathBuf },
    Url { url: String },
    Directory { path: PathBuf },
}

/// Report from an ingest operation.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    pub pages_created: Vec<PathBuf>,
    pub pages_updated: Vec<PathBuf>,
    pub contradictions: Vec<String>,
    /// Non-fatal warnings (e.g. individual batch/file failures in a multi-item ingest).
    pub warnings: Vec<String>,
}

impl IngestReport {
    /// Merge another report into this one, consuming all its entries.
    pub fn merge(&mut self, other: IngestReport) {
        self.pages_created.extend(other.pages_created);
        self.pages_updated.extend(other.pages_updated);
        self.contradictions.extend(other.contradictions);
        self.warnings.extend(other.warnings);
    }
}

/// Analysis of a source before synthesis (building block for interactive ingest).
#[derive(Debug, Clone, Default)]
pub struct SourceAnalysis {
    pub takeaways: Vec<String>,
    pub suggested_emphasis: Vec<String>,
    pub image_count: usize,
}

/// Result from a query operation.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub answer: String,
    pub citations: Vec<Citation>,
    pub suggested_pages: Vec<SuggestedPage>,
}

/// Citation in a query result.
#[derive(Debug, Clone)]
pub struct Citation {
    pub page: PathBuf,
    pub title: String,
}

/// A page suggested by query (requires user confirmation).
#[derive(Debug, Clone)]
pub struct SuggestedPage {
    pub path: PathBuf,
    pub content: String,
    pub rationale: String,
}

/// Report from a lint operation.
#[derive(Debug, Clone, Default)]
pub struct LintReport {
    pub issues: Vec<LintIssue>,
    pub suggested_questions: Vec<String>,
    pub suggested_sources: Vec<String>,
}

/// A single lint issue.
#[derive(Debug, Clone)]
pub struct LintIssue {
    pub kind: LintIssueKind,
    pub description: String,
    pub affected_pages: Vec<PathBuf>,
    pub proposed_fix: Option<ProposedFix>,
}

/// Categories of lint issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LintIssueKind {
    Stale,
    Contradiction,
    Orphan,
    MissingLink,
    DuplicateCoverage,
    IncompletePage,
}

/// A proposed fix for a lint issue.
#[derive(Debug, Clone)]
pub struct ProposedFix {
    pub description: String,
    pub operations: Vec<WikiOperation>,
}

/// An operation on the wiki.
#[derive(Debug, Clone)]
pub enum WikiOperation {
    CreatePage { path: PathBuf, content: String },
    UpdatePage { path: PathBuf, content: String },
    DeletePage { path: PathBuf },
}

/// Metadata sidecar for a stored source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceMeta {
    pub original_path: String,
    pub format: SourceFormat,
    pub hash: String,
    pub ingested_at: DateTime<Utc>,
}

/// Detected source format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceFormat {
    Markdown,
    Text,
    Code,
    Pdf,
    Image,
    Html,
    Json,
    Jsonl,
    Toml,
    Zip,
    Tgz,
    Unknown,
}

/// Index entry (one line in index.md).
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub title: String,
    pub path: PathBuf,
    pub summary: String,
}

/// Memex configuration (from config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemexConfig {
    pub provider: ProviderConfig,
}

/// Provider config section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_key_env: Option<String>,
}

/// A proposed page from LLM output.
#[derive(Debug, Clone)]
pub struct ProposedPage {
    pub path: PathBuf,
    pub action: PageAction,
    pub content: String,
}

/// Action for a proposed page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAction {
    Create,
    Update,
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
            created: chrono::Utc::now(),
            last_updated: chrono::Utc::now(),
            sources: vec!["sources/documents/abc.md".to_string()],
        };
        let yaml = serde_yaml::to_string(&fm).unwrap();
        let back: PageFrontmatter = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.title, "Test");
        assert_eq!(back.tags, vec!["entity"]);
    }

    #[test]
    fn frontmatter_tags_default_to_empty() {
        let yaml = "title: No Tags\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n";
        let fm: PageFrontmatter = serde_yaml::from_str(yaml).unwrap();
        assert!(fm.tags.is_empty());
    }

    #[test]
    fn source_format_serializes() {
        let json = serde_json::to_string(&SourceFormat::Jsonl).unwrap();
        assert_eq!(json, "\"jsonl\"");
    }

    #[test]
    fn source_meta_roundtrip() {
        let meta = SourceMeta {
            original_path: "/tmp/notes.md".to_string(),
            format: SourceFormat::Markdown,
            hash: "abc123".to_string(),
            ingested_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SourceMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hash, "abc123");
    }

    #[test]
    fn ingest_report_default_is_empty() {
        let report = IngestReport::default();
        assert!(report.pages_created.is_empty());
    }
}
