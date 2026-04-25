//! Shared JSON-reply parsers used by every agent worker. The agent prompt
//! is identical across providers; so is the JSON output contract.

use crate::daemon::queue::{ExpandReply, ExtractedPage, IngestReply, MergeReply, SynthReply};
use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone)]
pub struct ParseFailure {
    pub raw: String,
    pub reason: String,
}

impl ParseFailure {
    pub fn preview(&self, max_chars: usize) -> String {
        let mut out = String::new();
        for ch in self.raw.chars().take(max_chars) {
            if ch == '\n' || ch == '\r' {
                out.push(' ');
            } else {
                out.push(ch);
            }
        }
        out
    }
}

/// Parse an expansion reply. Returns `Err(raw_text)` if the payload doesn't
/// match `{lex, vec, hyde}` — callers fall back to un-expanded retrieval.
pub fn parse_expansion(text: &str) -> Result<ExpandReply, ParseFailure> {
    #[derive(Deserialize)]
    struct Raw {
        lex: String,
        vec: String,
        hyde: String,
    }
    let cleaned = strip_code_fences(text);
    match parse_json_with_recovery::<Raw>(cleaned) {
        Ok(r) => Ok(ExpandReply {
            lex: r.lex,
            vec: r.vec,
            hyde: r.hyde,
        }),
        Err(e) => Err(ParseFailure {
            raw: text.to_string(),
            reason: format!("expected JSON object {{lex,vec,hyde}}: {e}"),
        }),
    }
}

/// Parse a synthesis reply. Returns `Err(raw_text)` if the payload doesn't
/// match `{answer, citations}` — callers surface as `WorkerError::Backend`.
pub fn parse_synthesis(text: &str) -> Result<SynthReply, ParseFailure> {
    #[derive(Deserialize)]
    struct Raw {
        answer: String,
        #[serde(default)]
        citations: Vec<String>,
    }
    let cleaned = strip_code_fences(text);
    match parse_json_with_recovery::<Raw>(cleaned) {
        Ok(r) => Ok(SynthReply {
            answer: r.answer,
            citations: r.citations,
        }),
        Err(e) => Err(ParseFailure {
            raw: text.to_string(),
            reason: format!("expected JSON object {{answer,citations}}: {e}"),
        }),
    }
}

/// Parse an ingest extraction reply. Tries JSON first (safe from YAML alias
/// expansion attacks), then YAML as fallback.
pub fn parse_ingest(text: &str) -> Result<IngestReply, ParseFailure> {
    #[derive(Deserialize)]
    struct WrappedPages {
        pages: Vec<ExtractedPage>,
    }

    let cleaned = strip_code_fences(text);
    if let Ok(pages) = serde_json::from_str::<Vec<ExtractedPage>>(cleaned) {
        return Ok(IngestReply { pages });
    }
    if let Ok(w) = serde_json::from_str::<WrappedPages>(cleaned) {
        return Ok(IngestReply { pages: w.pages });
    }
    if let Ok(page) = serde_json::from_str::<ExtractedPage>(cleaned) {
        return Ok(IngestReply { pages: vec![page] });
    }
    if let Ok(pages) = parse_json_with_recovery::<Vec<ExtractedPage>>(cleaned) {
        return Ok(IngestReply { pages });
    }
    if let Ok(w) = parse_json_with_recovery::<WrappedPages>(cleaned) {
        return Ok(IngestReply { pages: w.pages });
    }
    if let Ok(page) = parse_json_with_recovery::<ExtractedPage>(cleaned) {
        return Ok(IngestReply { pages: vec![page] });
    }
    if let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned) {
        return Ok(IngestReply { pages });
    }
    if let Ok(page) = serde_yaml::from_str::<ExtractedPage>(cleaned) {
        return Ok(IngestReply { pages: vec![page] });
    }
    if let Some(idx) = cleaned.find("\n- slug:")
        && let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(&cleaned[idx + 1..])
    {
        return Ok(IngestReply { pages });
    }
    if cleaned.starts_with("- slug:")
        && let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned)
    {
        return Ok(IngestReply { pages });
    }
    if let Some(idx) = cleaned.find("\nslug:")
        && let Ok(page) = serde_yaml::from_str::<ExtractedPage>(&cleaned[idx + 1..])
    {
        return Ok(IngestReply { pages: vec![page] });
    }
    if let Some(pages) = parse_yaml_pages_by_slug_blocks(cleaned)
        && !pages.is_empty()
    {
        return Ok(IngestReply { pages });
    }
    let json_err = serde_json::from_str::<Vec<ExtractedPage>>(cleaned)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    let yaml_err = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    Err(ParseFailure {
        raw: text.to_string(),
        reason: format!(
            "expected list of pages as JSON/YAML; json_err={json_err}; yaml_err={yaml_err}"
        ),
    })
}

/// Parse a merge reply. Same format as ingest: JSON first, YAML fallback.
pub fn parse_merge(text: &str) -> Result<MergeReply, ParseFailure> {
    #[derive(Deserialize)]
    struct WrappedPages {
        pages: Vec<ExtractedPage>,
    }

    let cleaned = strip_code_fences(text);
    if let Ok(pages) = serde_json::from_str::<Vec<ExtractedPage>>(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: pages,
        });
    }
    if let Ok(w) = serde_json::from_str::<WrappedPages>(cleaned)
        && !w.pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: w.pages,
        });
    }
    if let Ok(page) = serde_json::from_str::<ExtractedPage>(cleaned) {
        return Ok(MergeReply {
            merged_pages: vec![page],
        });
    }
    if let Ok(pages) = parse_json_with_recovery::<Vec<ExtractedPage>>(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: pages,
        });
    }
    if let Ok(page) = parse_json_with_recovery::<ExtractedPage>(cleaned) {
        return Ok(MergeReply {
            merged_pages: vec![page],
        });
    }
    if let Ok(w) = parse_json_with_recovery::<WrappedPages>(cleaned)
        && !w.pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: w.pages,
        });
    }
    if let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: pages,
        });
    }
    if let Ok(page) = serde_yaml::from_str::<ExtractedPage>(cleaned) {
        return Ok(MergeReply {
            merged_pages: vec![page],
        });
    }
    if let Some(idx) = cleaned.find("\n- slug:")
        && let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(&cleaned[idx + 1..])
        && !pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: pages,
        });
    }
    if cleaned.starts_with("- slug:")
        && let Ok(pages) = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: pages,
        });
    }
    if let Some(idx) = cleaned.find("\nslug:")
        && let Ok(page) = serde_yaml::from_str::<ExtractedPage>(&cleaned[idx + 1..])
    {
        return Ok(MergeReply {
            merged_pages: vec![page],
        });
    }
    if let Some(pages) = parse_yaml_pages_by_slug_blocks(cleaned)
        && !pages.is_empty()
    {
        return Ok(MergeReply {
            merged_pages: pages,
        });
    }
    let json_err = serde_json::from_str::<Vec<ExtractedPage>>(cleaned)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    let yaml_err = serde_yaml::from_str::<Vec<ExtractedPage>>(cleaned)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    Err(ParseFailure {
        raw: text.to_string(),
        reason: format!(
            "expected non-empty list of merged pages as JSON/YAML; json_err={json_err}; yaml_err={yaml_err}"
        ),
    })
}

fn strip_code_fences(text: &str) -> &str {
    text.trim()
        .trim_start_matches("```yaml")
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

fn parse_json_with_recovery<T: DeserializeOwned>(text: &str) -> Result<T, serde_json::Error> {
    if let Ok(v) = serde_json::from_str::<T>(text) {
        return Ok(v);
    }

    // Recovery path for model outputs like:
    // "Some prose...\n{...json...}\nmore prose"
    let mut starts = Vec::new();
    for (i, ch) in text.char_indices() {
        if ch == '{' || ch == '[' {
            starts.push(i);
            if starts.len() >= 256 {
                break;
            }
        }
    }

    let mut last_err = serde_json::Error::io(std::io::Error::other(
        "no JSON object/array candidate found",
    ));
    for idx in starts {
        let slice = &text[idx..];
        let mut iter = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        match iter.next() {
            Some(Ok(v)) => match serde_json::from_value::<T>(v) {
                Ok(parsed) => return Ok(parsed),
                Err(e) => last_err = e,
            },
            Some(Err(e)) => last_err = e,
            None => {}
        }
    }
    Err(last_err)
}

fn parse_yaml_pages_by_slug_blocks(text: &str) -> Option<Vec<ExtractedPage>> {
    let mut blocks: Vec<(usize, Vec<String>)> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_indent: usize = 0;

    for line in text.lines() {
        let indent = line.len().saturating_sub(line.trim_start().len());
        let trimmed = line.trim_start();
        if trimmed.starts_with("- slug:") {
            if !current.is_empty() {
                blocks.push((current_indent, std::mem::take(&mut current)));
            }
            current_indent = indent;
            current.push(trimmed.trim_start_matches("- ").to_string());
            continue;
        }
        if !current.is_empty() {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        blocks.push((current_indent, current));
    }
    if blocks.is_empty() {
        return None;
    }

    let mut pages = Vec::new();
    for (indent, lines) in blocks {
        let mut dedented: Vec<String> = Vec::with_capacity(lines.len());
        let child_dedent = indent.saturating_add(2);
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                dedented.push(line.clone());
                continue;
            }
            dedented.push(dedent_line(line, child_dedent));
        }
        let block = dedented.join("\n");
        if let Ok(page) = serde_yaml::from_str::<ExtractedPage>(&block) {
            pages.push(page);
            continue;
        }
        let normalized = drop_duplicate_top_level_keys(&block);
        if let Ok(page) = serde_yaml::from_str::<ExtractedPage>(&normalized) {
            pages.push(page);
        }
    }
    if pages.is_empty() { None } else { Some(pages) }
}

fn dedent_line(line: &str, indent: usize) -> String {
    let mut removed = 0usize;
    let mut out = line;
    while removed < indent {
        if let Some(rest) = out.strip_prefix(' ') {
            out = rest;
            removed += 1;
        } else {
            break;
        }
    }
    out.to_string()
}

fn drop_duplicate_top_level_keys(block: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    seen.insert("slug");
    let mut out = Vec::new();
    for (i, line) in block.lines().enumerate() {
        if i == 0 {
            out.push(line.to_string());
            continue;
        }
        let is_top_level =
            !line.starts_with(' ') && line.contains(':') && !line.trim_start().starts_with('-');
        if is_top_level {
            let key = line
                .trim_start()
                .split_once(':')
                .map(|(k, _)| k.trim())
                .unwrap_or_default();
            if matches!(key, "slug" | "title" | "tags" | "body") {
                if seen.contains(key) {
                    continue;
                }
                seen.insert(key);
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_round_trips() {
        let r = parse_synthesis(r#"{"answer":"hi","citations":["a"]}"#).unwrap();
        assert_eq!(r.answer, "hi");
        assert_eq!(r.citations, vec!["a"]);
    }

    #[test]
    fn parse_reply_code_fence() {
        let r = parse_synthesis("```json\n{\"answer\":\"hi\",\"citations\":[]}\n```").unwrap();
        assert_eq!(r.answer, "hi");
        assert!(r.citations.is_empty());
    }

    #[test]
    fn parse_reply_fallback() {
        let e = parse_synthesis("plain").unwrap_err();
        assert_eq!(e.raw, "plain");
    }

    #[test]
    fn parse_expand_round_trips() {
        let r = parse_expansion(r#"{"lex":"x","vec":"y","hyde":"z"}"#).unwrap();
        assert_eq!(r.lex, "x");
        assert_eq!(r.vec, "y");
        assert_eq!(r.hyde, "z");
    }

    #[test]
    fn parse_expand_fallback() {
        let e = parse_expansion("not json").unwrap_err();
        assert_eq!(e.raw, "not json");
    }

    #[test]
    fn parse_reply_recovers_json_from_prose() {
        let r =
            parse_synthesis("Answer:\n{\"answer\":\"ok\",\"citations\":[\"p1\"]}\nThanks").unwrap();
        assert_eq!(r.answer, "ok");
        assert_eq!(r.citations, vec!["p1"]);
    }

    #[test]
    fn parse_expand_recovers_json_from_prose() {
        let r = parse_expansion("Using workflow...\n{\"lex\":\"a\",\"vec\":\"b\",\"hyde\":\"c\"}")
            .unwrap();
        assert_eq!(r.lex, "a");
        assert_eq!(r.vec, "b");
        assert_eq!(r.hyde, "c");
    }

    #[test]
    fn parse_ingest_accepts_single_page_object() {
        let r = parse_ingest(r#"{"slug":"s","title":"t","tags":[],"body":"b"}"#).unwrap();
        assert_eq!(r.pages.len(), 1);
        assert_eq!(r.pages[0].slug, "s");
    }

    #[test]
    fn parse_ingest_accepts_wrapped_pages_object() {
        let r = parse_ingest(r#"{"pages":[{"slug":"s","title":"t","tags":["x"],"body":"b"}]}"#)
            .unwrap();
        assert_eq!(r.pages.len(), 1);
        assert_eq!(r.pages[0].slug, "s");
    }

    #[test]
    fn parse_merge_recovers_yaml_after_prose() {
        let txt = "note\n- slug: s\n  title: t\n  tags: []\n  body: b\n";
        let r = parse_merge(txt).unwrap();
        assert_eq!(r.merged_pages.len(), 1);
        assert_eq!(r.merged_pages[0].slug, "s");
    }

    #[test]
    fn parse_merge_accepts_wrapped_pages_object() {
        let r =
            parse_merge(r#"{"pages":[{"slug":"s","title":"t","tags":["x"],"body":"b"}]}"#).unwrap();
        assert_eq!(r.merged_pages.len(), 1);
        assert_eq!(r.merged_pages[0].slug, "s");
    }

    #[test]
    fn parse_expand_replays_log_sample_with_prose_prefix() {
        let raw = "Using the superpowers workflow to make sure I follow the task format precisely.{\"lex\":\"camping plans\",\"vec\":\"What day is Melanie scheduled to go camping?\",\"hyde\":\"2026-07-12 — Melanie plans to go camping next weekend [planned]\"}";
        let r = parse_expansion(raw).unwrap();
        assert_eq!(r.lex, "camping plans");
    }

    #[test]
    fn parse_merge_replays_log_sample_with_bad_nested_list_indentation() {
        let raw = "- slug: john\n  title: \"John\"\n  tags: [\"person\"]\n  body: |\n    John text\n\n  - slug: maria\n    title: \"Maria\"\n    tags: [\"person\"]\n    body: |\n      Maria text";
        let r = parse_merge(raw).unwrap();
        assert_eq!(r.merged_pages.len(), 2);
        assert_eq!(r.merged_pages[0].slug, "john");
        assert_eq!(r.merged_pages[1].slug, "maria");
    }

    #[test]
    fn parse_ingest_replays_log_sample_with_duplicate_tags_noise() {
        let raw = "- slug: joanna\n  title: Joanna\n  tags: [\"person\"]\n  body: |\n    Joanna text\n  tags: [\"person\"]\n- slug: nate\n  title: Nate\n  tags: [\"person\"]\n  body: |\n    Nate text";
        let r = parse_ingest(raw).unwrap();
        assert!(!r.pages.is_empty());
        assert!(
            r.pages
                .iter()
                .any(|p| p.slug == "joanna" || p.slug == "nate")
        );
    }

    #[test]
    fn parse_failure_preview_collapses_newlines() {
        let e = ParseFailure {
            raw: "line1\nline2\r\nline3".to_string(),
            reason: "x".to_string(),
        };
        assert_eq!(e.preview(100), "line1 line2  line3");
    }
}
