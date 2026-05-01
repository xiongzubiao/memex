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
        assert!(!json.contains(r#""error":"#), "error: None must be skipped via skip_serializing_if");
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
}
