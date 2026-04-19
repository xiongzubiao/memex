//! Shared JSON-reply parsers used by every agent worker. The agent prompt
//! is identical across providers; so is the JSON output contract.

use crate::daemon::queue::{ExpandReply, SynthReply};
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

fn strip_code_fences(text: &str) -> &str {
    text.trim()
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
