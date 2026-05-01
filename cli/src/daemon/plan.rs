//! Plan JSON schema. Wire format streamed between skill and daemon
//! via stdin/stdout. Skill stores it in its own temp file; the daemon
//! is stateless on plan content.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub version: u32,
    pub source: PlanSource,
    pub created_at: String,
    pub proposals: Vec<Proposal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanSource {
    pub id: String,
    pub identifier: String,
    pub content_hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Proposal {
    pub index: usize,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub body: String,
    pub merge_target_slug: Option<String>,
    pub merge_target_hash: Option<String>,
    pub merge_diff: Option<String>,
    #[serde(default)]
    pub dropped: bool,
    #[serde(default)]
    pub committed: bool,
    pub original_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Proposal {
    /// Construct a "new page" proposal from a freshly-extracted page.
    /// Merge cases (success or failure) layer field overrides on top.
    pub fn new_for_page(index: usize, page: crate::daemon::queue::ExtractedPage) -> Self {
        let slug = page.slug;
        Self {
            index,
            slug: slug.clone(),
            title: page.title,
            tags: page.tags,
            body: page.body,
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: slug,
            error: None,
        }
    }
}

impl Plan {
    /// Structural validation. Catches user-tampered plans without trying
    /// to detect every form of tampering — apply enforces the shape that
    /// the rest of the logic depends on.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported version: {} (expected 1)",
                self.version
            ));
        }
        if !is_hex64(&self.source.content_hash) {
            return Err(format!(
                "source.content_hash must be 64 lowercase hex chars (got {} chars)",
                self.source.content_hash.len()
            ));
        }
        for p in &self.proposals {
            if p.original_slug.is_empty() {
                return Err(format!("proposal {}: original_slug is empty", p.index));
            }
            if p.slug.is_empty() {
                return Err(format!("proposal {}: slug is empty", p.index));
            }
            if !is_kebab_case_slug(&p.slug) {
                return Err(format!(
                    "proposal {}: slug '{}' is not kebab-case",
                    p.index, p.slug
                ));
            }
            if let Some(s) = &p.merge_target_slug
                && s.is_empty()
            {
                return Err(format!("proposal {}: merge_target_slug is empty", p.index));
            }
            if let Some(h) = &p.merge_target_hash
                && !is_hex64(h)
            {
                return Err(format!(
                    "proposal {}: merge_target_hash must be 64 lowercase hex chars",
                    p.index
                ));
            }
            // Coupling rule: target_slug == None implies target_hash == None.
            if p.merge_target_slug.is_none() && p.merge_target_hash.is_some() {
                return Err(format!(
                    "proposal {}: merge_target_hash set without merge_target_slug",
                    p.index
                ));
            }
        }
        // Index must be 0-based contiguous.
        let mut indices: Vec<usize> = self.proposals.iter().map(|p| p.index).collect();
        indices.sort();
        for (expected, got) in indices.iter().enumerate() {
            if expected != *got {
                return Err(format!(
                    "proposals[].index must be 0-based contiguous; saw {got} where {expected} expected"
                ));
            }
        }
        // Effective-slug uniqueness across non-dropped proposals.
        let mut by_slug: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for p in self.proposals.iter().filter(|p| !p.dropped) {
            if let Some(prev) = by_slug.insert(p.slug.as_str(), p.index) {
                return Err(format!(
                    "slug collision: '{}' appears in proposals {} and {}",
                    p.slug, prev, p.index
                ));
            }
        }
        Ok(())
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn is_kebab_case_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_minimal_plan() {
        let plan = Plan {
            version: 1,
            source: PlanSource {
                id: "src-abc123".into(),
                identifier: "https://x/p".into(),
                content_hash: "a".repeat(64),
                size_bytes: 100,
            },
            created_at: "2026-04-30T19:42:00Z".into(),
            proposals: vec![],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn proposal_round_trip_with_merge_fields() {
        let p = Proposal {
            index: 0,
            slug: "mmai".into(),
            title: "MMAI".into(),
            tags: vec!["ai".into()],
            body: "body".into(),
            merge_target_slug: Some("mmai".into()),
            merge_target_hash: Some("f".repeat(64)),
            merge_diff: Some("--- a\n+++ b\n".into()),
            dropped: false,
            committed: false,
            original_slug: "mmai".into(),
            error: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        assert!(json.contains(r#""merge_target_slug":"mmai""#));
        assert!(
            !json.contains(r#""error":"#),
            "error: None must be skipped via skip_serializing_if"
        );
    }

    #[test]
    fn proposal_deserialize_omits_error_when_absent() {
        let json = r#"{
            "index":0,"slug":"x","title":"X","tags":[],"body":"b",
            "merge_target_slug":null,"merge_target_hash":null,"merge_diff":null,
            "dropped":false,"committed":false,"original_slug":"x"
        }"#;
        let p: Proposal = serde_json::from_str(json).unwrap();
        assert!(p.error.is_none());
    }

    fn valid_plan() -> Plan {
        Plan {
            version: 1,
            source: PlanSource {
                id: "src-abc".into(),
                identifier: "https://x".into(),
                content_hash: "a".repeat(64),
                size_bytes: 1,
            },
            created_at: "2026-04-30T00:00:00Z".into(),
            proposals: vec![Proposal {
                index: 0,
                slug: "mmai".into(),
                title: "MMAI".into(),
                tags: vec![],
                body: "b".into(),
                merge_target_slug: None,
                merge_target_hash: None,
                merge_diff: None,
                dropped: false,
                committed: false,
                original_slug: "mmai".into(),
                error: None,
            }],
        }
    }

    #[test]
    fn validate_accepts_valid_plan() {
        valid_plan().validate().unwrap();
    }

    #[test]
    fn validate_rejects_unknown_version() {
        let mut p = valid_plan();
        p.version = 2;
        assert!(p.validate().unwrap_err().contains("version"));
    }

    #[test]
    fn validate_rejects_short_content_hash() {
        let mut p = valid_plan();
        p.source.content_hash = "abc".into();
        assert!(p.validate().unwrap_err().contains("content_hash"));
    }

    #[test]
    fn validate_rejects_empty_slug() {
        let mut p = valid_plan();
        p.proposals[0].slug = String::new();
        assert!(p.validate().unwrap_err().contains("slug"));
    }

    #[test]
    fn validate_rejects_non_kebab_slug() {
        let mut p = valid_plan();
        p.proposals[0].slug = "MMAI".into();
        assert!(p.validate().unwrap_err().contains("kebab"));
    }

    #[test]
    fn validate_rejects_slug_collision_among_non_dropped() {
        let mut p = valid_plan();
        p.proposals.push(Proposal {
            index: 1,
            slug: "mmai".into(),
            title: "Dup".into(),
            tags: vec![],
            body: "b".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "mmai".into(),
            error: None,
        });
        let err = p.validate().unwrap_err();
        assert!(err.contains("slug collision"), "got: {err}");
    }

    #[test]
    fn validate_allows_slug_collision_when_one_dropped() {
        let mut p = valid_plan();
        p.proposals.push(Proposal {
            index: 1,
            slug: "mmai".into(),
            title: "Dup".into(),
            tags: vec![],
            body: "b".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: true,
            committed: false,
            original_slug: "mmai".into(),
            error: None,
        });
        p.validate().unwrap();
    }

    #[test]
    fn validate_rejects_non_contiguous_index() {
        let mut p = valid_plan();
        p.proposals[0].index = 5;
        assert!(p.validate().unwrap_err().contains("index"));
    }

    #[test]
    fn validate_rejects_orphan_hash_without_slug() {
        let mut p = valid_plan();
        p.proposals[0].merge_target_slug = None;
        p.proposals[0].merge_target_hash = Some("a".repeat(64));
        assert!(p.validate().unwrap_err().contains("merge_target"));
    }

    #[test]
    fn validate_allows_slug_with_null_hash() {
        // MERGE-dry-run failure state: slug populated, hash null.
        let mut p = valid_plan();
        p.proposals[0].merge_target_slug = Some("mmai".into());
        p.proposals[0].merge_target_hash = None;
        p.validate().unwrap();
    }
}
