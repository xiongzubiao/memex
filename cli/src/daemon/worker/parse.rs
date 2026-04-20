//! Shared JSON-reply parsers used by every agent worker. The agent prompt
//! is identical across providers; so is the JSON output contract.

use crate::daemon::queue::{ExpandReply, ExtractedPage, IngestReply, MergeReply, SynthReply};
use serde::Deserialize;

/// Parse a synthesis reply. Returns `Err(raw_text)` if the payload doesn't
/// match `{answer, citations}` — callers surface as `WorkerError::AgentError`.
pub fn parse_reply(text: &str) -> Result<SynthReply, String> {
    #[derive(Deserialize)]
    struct Raw {
        answer: String,
        #[serde(default)]
        citations: Vec<String>,
    }
    let cleaned = strip_code_fences(text);
    match serde_json::from_str::<Raw>(cleaned) {
        Ok(r) => Ok(SynthReply {
            answer: r.answer,
            citations: r.citations,
        }),
        Err(_) => Err(text.to_string()),
    }
}

/// Parse an expansion reply. Returns `Err(raw_text)` if the payload doesn't
/// match `{lex, vec, hyde}` — callers fall back to un-expanded retrieval.
pub fn parse_expand(text: &str) -> Result<ExpandReply, String> {
    #[derive(Deserialize)]
    struct Raw {
        lex: String,
        vec: String,
        hyde: String,
    }
    let cleaned = strip_code_fences(text);
    match serde_json::from_str::<Raw>(cleaned) {
        Ok(r) => Ok(ExpandReply {
            lex: r.lex,
            vec: r.vec,
            hyde: r.hyde,
        }),
        Err(_) => Err(text.to_string()),
    }
}

/// Parse an ingest extraction reply. Tries JSON first (safe from YAML alias
/// expansion attacks), then YAML as fallback.
pub fn parse_ingest(text: &str) -> Result<IngestReply, String> {
    let cleaned = strip_code_fences(text);
    if let Ok(pages) = serde_json::from_str::<Vec<ExtractedPage>>(cleaned) {
        return Ok(IngestReply { pages });
    }
    if let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned) {
        return Ok(IngestReply { pages });
    }
    Err(text.to_string())
}

/// Parse a merge reply. Same format as ingest: JSON first, YAML fallback.
pub fn parse_merge(text: &str) -> Result<MergeReply, String> {
    let cleaned = strip_code_fences(text);
    if let Ok(pages) = serde_json::from_str::<Vec<ExtractedPage>>(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply { merged_pages: pages });
    }
    if let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply { merged_pages: pages });
    }
    Err(text.to_string())
}

fn strip_code_fences(text: &str) -> &str {
    text.trim()
        .trim_start_matches("```yaml")
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_round_trips() {
        let r = parse_reply(r#"{"answer":"hi","citations":["a"]}"#).unwrap();
        assert_eq!(r.answer, "hi");
        assert_eq!(r.citations, vec!["a"]);
    }

    #[test]
    fn parse_reply_code_fence() {
        let r = parse_reply("```json\n{\"answer\":\"hi\",\"citations\":[]}\n```").unwrap();
        assert_eq!(r.answer, "hi");
        assert!(r.citations.is_empty());
    }

    #[test]
    fn parse_reply_fallback() {
        assert_eq!(parse_reply("plain").unwrap_err(), "plain");
    }

    #[test]
    fn parse_expand_round_trips() {
        let r = parse_expand(r#"{"lex":"x","vec":"y","hyde":"z"}"#).unwrap();
        assert_eq!(r.lex, "x");
        assert_eq!(r.vec, "y");
        assert_eq!(r.hyde, "z");
    }

    #[test]
    fn parse_expand_fallback() {
        assert_eq!(parse_expand("not json").unwrap_err(), "not json");
    }
}
