use crate::error::{MemexError, Result};
use crate::search::{self, Bm25Search, WikiSearch};
use crate::types::{Citation, QueryResult};
use crate::{Memex, index, log, validate};
use std::path::PathBuf;
use tracing::{error, info};

const QUERY_SYSTEM_PROMPT: &str = "You are a knowledgeable assistant helping to retrieve \
    and synthesize information from a personal wiki knowledge base.";

const NO_KNOWLEDGE_FOUND: &str = "No relevant knowledge found in memex.";

/// Minimum score for results after RRF fusion (following qmd).
/// Applied to `1/rank` scores (not raw RRF scores), same as qmd's skipRerank path.
/// Rank 1 = 1.0, rank 2 = 0.5, rank 3 = 0.33. Threshold 0.3 passes top ~3 results.
pub(crate) const MIN_SCORE: f32 = 0.3;

impl Memex {
    /// Query the memex for an answer to a question.
    ///
    /// Fusion pipeline (following qmd's hybridQuery):
    /// 1. BM25 probe (free, instant)
    /// 2. Strong signal check (>= 0.85, gap >= 0.15) — skip expansion if strong
    /// 3. LLM expands query into lex terms, each searched separately
    /// 4. RRF fusion of all ranked lists (initial probe + expansions)
    /// 5. Post-fusion minScore filter
    /// 6. Fallback: if no results, LLM reads index.md and selects pages
    /// 7. LLM synthesizes answer with citations
    pub async fn query(&self, question: &str) -> Result<QueryResult> {
        // Step 1: BM25 probe
        let initial_fts = self.search.search(question, 20, None).await?;
        let top_score = initial_fts.first().map(|r| r.score).unwrap_or(0.0);
        info!(hits = initial_fts.len(), top_score, "BM25 probe");

        // Step 2: Strong signal check — skip expensive LLM expansion
        let has_strong_signal = Bm25Search::is_strong_signal(&initial_fts);
        if has_strong_signal {
            info!(top_score, "strong signal, skipping expansion");
        }

        // Step 3: Expand query (or skip if strong signal)
        let mut ranked_lists: Vec<Vec<search::SearchResult>> = Vec::new();

        // Seed with initial BM25 probe results (always included, following qmd)
        if !initial_fts.is_empty() {
            ranked_lists.push(initial_fts);
        }

        if !has_strong_signal {
            // LLM expansion → each lex term is a separate ranked list
            match self.expand_query(question).await {
                Ok(terms) => {
                    info!(
                        term_count = terms.len(),
                        "query expansion produced lex terms"
                    );
                    for term in &terms {
                        if let Ok(hits) = self.search.search(term, 20, None).await
                            && !hits.is_empty()
                        {
                            ranked_lists.push(hits);
                        }
                    }
                }
                Err(_) => {
                    info!("query expansion failed");
                }
            }
        }

        // Step 4: RRF fusion
        let fused = search::reciprocal_rank_fusion(&ranked_lists, 10);

        // Step 5: Post-fusion minScore filter
        let selected_paths: Vec<PathBuf> = fused
            .iter()
            .filter(|r| r.score >= MIN_SCORE)
            .map(|r| r.path.clone())
            .collect();

        if !selected_paths.is_empty() {
            info!(
                hits = selected_paths.len(),
                top_score = fused.first().map(|r| r.score).unwrap_or(0.0),
                "RRF fusion results"
            );
            for r in &fused {
                if r.score >= MIN_SCORE {
                    info!(path = %r.path.display(), score = r.score, "selected page");
                }
            }
        }

        // Step 6: Fallback — if fusion produced nothing, LLM reads full index
        let selected_paths = if selected_paths.is_empty() {
            info!("no results after fusion, falling back to LLM index selection");
            let index_content = match self.read_index() {
                Ok(c) if !crate::index::is_empty_index(&c) => c,
                _ => {
                    return Ok(QueryResult {
                        answer: NO_KNOWLEDGE_FOUND.to_string(),
                        citations: vec![],
                        suggested_pages: vec![],
                    });
                }
            };
            let paths = select_relevant_pages(
                self.provider.as_ref(),
                self.model(),
                &index_content,
                question,
                "Question",
                QUERY_SYSTEM_PROMPT,
            )
            .await?;
            info!(pages = paths.len(), "LLM selected pages from index");
            paths
        } else {
            selected_paths
        };

        if selected_paths.is_empty() {
            return Ok(QueryResult {
                answer: NO_KNOWLEDGE_FOUND.to_string(),
                citations: vec![],
                suggested_pages: vec![],
            });
        }

        // Step 4: Read selected page contents
        let mut page_contents = Vec::new();
        for rel_path in &selected_paths {
            let abs_path = self.root().join(rel_path);
            if let Ok(content) = std::fs::read_to_string(&abs_path) {
                page_contents.push((rel_path.clone(), content));
            }
        }

        if page_contents.is_empty() {
            return Ok(QueryResult {
                answer: NO_KNOWLEDGE_FOUND.to_string(),
                citations: vec![],
                suggested_pages: vec![],
            });
        }

        // Build context string for synthesis
        let mut context = String::new();
        for (path, content) in &page_contents {
            context.push_str(&format!("=== {} ===\n{}\n\n", path.display(), content));
        }

        // Step 5: Ask LLM to synthesize answer
        info!(
            pages = page_contents.len(),
            context_bytes = context.len(),
            "synthesizing answer via LLM"
        );
        let synthesis_prompt = format!(
            "Based on these wiki pages from a personal knowledge base, answer the question.\n\n\
            {context}\n\
            Question: {question}\n\n\
            Provide a clear, concise answer with citations to relevant pages using [Page Title] notation."
        );

        let answer = self
            .provider
            .chat(
                Some(QUERY_SYSTEM_PROMPT),
                &synthesis_prompt,
                self.model(),
                0.2,
            )
            .await
            .map_err(|e| {
                error!(error = %e, "query LLM call failed");
                MemexError::LlmCallFailed {
                    details: e.to_string(),
                }
            })?;

        // Build citations from selected pages, looking up titles in index
        let index_content = self.read_index().unwrap_or_default();
        let index_entries = index::parse_index_entries(&index_content);
        let citations = build_citations(&page_contents, &index_entries);

        // Step 6: Log the query
        let pages_count = page_contents.len();
        let _ = log::append_log(
            self.root(),
            "query",
            &format!(
                "\"{}\"",
                if question.len() > 80 {
                    &question[..80]
                } else {
                    question
                }
            ),
            &format!("{pages_count} pages referenced"),
        );

        Ok(QueryResult {
            answer,
            citations,
            suggested_pages: vec![],
        })
    }

    /// Expand a query into lex search terms using the LLM (following qmd).
    ///
    /// Returns individual search terms (one per line from the LLM).
    /// Each term is searched separately as its own ranked list for RRF fusion.
    pub(crate) async fn expand_query(&self, query: &str) -> Result<Vec<String>> {
        // Check cache first
        let cache_key = format!("expand:{query}");
        if let Some(cached) = self.search.cache_get(&cache_key)? {
            let terms: Vec<String> = cached.lines().map(|l| l.to_string()).collect();
            return Ok(terms);
        }

        let prompt = format!(
            "Expand this search query into 10-15 alternative search terms.\n\
             Include synonyms, related technical terms, and specific technologies.\n\
             Return one term per line, nothing else.\n\n\
             Query: {query}"
        );

        let response = self
            .provider
            .chat(None, &prompt, self.model(), 0.3)
            .await
            .map_err(|e| MemexError::LlmCallFailed {
                details: e.to_string(),
            })?;

        let terms: Vec<String> = response
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let _ = self.search.cache_put(&cache_key, &terms.join("\n"));
        Ok(terms)
    }
}

/// Ask the LLM to select relevant wiki pages for a given topic.
/// Shared by `query()` and `context_for()`.
pub(crate) async fn select_relevant_pages(
    provider: &dyn crate::LlmProvider,
    model: &str,
    index_content: &str,
    topic: &str,
    label: &str,
    system_prompt: &str,
) -> std::result::Result<Vec<PathBuf>, crate::error::MemexError> {
    let selection_prompt = format!(
        "Here is a wiki index:\n\n{index_content}\n\n{label}: {topic}\n\n\
        Which wiki pages are most relevant? List paths, one per line (e.g. wiki/page.md). \
        Only list paths that exist in the index above."
    );

    let response = provider
        .chat(Some(system_prompt), &selection_prompt, model, 0.0)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "page selection LLM call failed");
            crate::error::MemexError::LlmCallFailed {
                details: e.to_string(),
            }
        })?;

    Ok(parse_page_paths(&response))
}

/// Parse lines that look like wiki page paths (starting with "wiki/" and ending with ".md").
/// Rejects paths containing traversal components (`..`, absolute paths) to prevent
/// a malicious LLM response from reading files outside the wiki directory.
pub(crate) fn parse_page_paths(response: &str) -> Vec<PathBuf> {
    response
        .lines()
        .map(|line| line.trim())
        .filter(|line| {
            line.starts_with("wiki/")
                && line.ends_with(".md")
                && !line.contains("..")
                && !line.contains("//")
        })
        .map(PathBuf::from)
        .collect()
}

/// Build citations from page contents and index entries.
fn build_citations(
    page_contents: &[(PathBuf, String)],
    index_entries: &[crate::types::IndexEntry],
) -> Vec<Citation> {
    page_contents
        .iter()
        .map(|(path, content)| {
            // Try to get title from index first
            let title = index_entries
                .iter()
                .find(|e| &e.path == path)
                .map(|e| e.title.clone())
                .unwrap_or_else(|| {
                    // Fallback: try to parse frontmatter from the content
                    validate::parse_frontmatter(content)
                        .map(|(fm, _)| fm.title)
                        .unwrap_or_else(|_| {
                            // Last resort: use filename stem
                            path.file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("Unknown")
                                .to_string()
                        })
                });
            Citation {
                page: path.clone(),
                title,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_page_paths_extracts_wiki_paths() {
        let response = "wiki/caching.md\nwiki/auth.md\nsome-other-line\n";
        let paths = parse_page_paths(response);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("wiki/caching.md"));
        assert_eq!(paths[1], PathBuf::from("wiki/auth.md"));
    }

    #[test]
    fn parse_page_paths_ignores_non_wiki() {
        let response = "docs/readme.md\nwiki/page.md\nwiki/no-extension\n";
        let paths = parse_page_paths(response);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], PathBuf::from("wiki/page.md"));
    }

    #[test]
    fn parse_page_paths_empty_response() {
        let paths = parse_page_paths("");
        assert!(paths.is_empty());
    }

    #[test]
    fn parse_page_paths_rejects_traversal() {
        let response = "wiki/../../etc/passwd.md\nwiki/../secret.md\nwiki/legit.md\n";
        let paths = parse_page_paths(response);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], PathBuf::from("wiki/legit.md"));
    }

    #[test]
    fn parse_page_paths_rejects_double_slash() {
        let response = "wiki//etc/passwd.md\nwiki/ok.md\n";
        let paths = parse_page_paths(response);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], PathBuf::from("wiki/ok.md"));
    }
}
