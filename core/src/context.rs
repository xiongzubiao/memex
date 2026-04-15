use crate::error::Result;
use crate::search::{self, Bm25Search, WikiSearch};
use crate::types::WikiPage;
use crate::{Memex, validate};
use tracing::warn;

const CONTEXT_SYSTEM_PROMPT: &str = "You are a knowledgeable assistant helping to select \
    relevant context from a personal wiki knowledge base.";

impl Memex {
    /// Return raw wiki page contents relevant to a task, respecting a token budget.
    ///
    /// Uses the same fusion pipeline as `query()` (following qmd):
    /// 1. BM25 probe → seed ranked lists
    /// 2. Strong signal → skip expansion
    /// 3. LLM expansion → each lex term is a separate ranked list
    /// 4. RRF fusion → minScore filter
    /// 5. Fallback: LLM reads index.md
    /// 6. Read selected pages respecting max_tokens budget
    pub async fn context_for(&self, task: &str, max_tokens: usize) -> Result<Vec<WikiPage>> {
        // BM25 probe
        let initial_fts = self.search.search(task, 20, None).await.unwrap_or_default();
        let has_strong_signal = Bm25Search::is_strong_signal(&initial_fts);

        let mut ranked_lists: Vec<Vec<search::SearchResult>> = Vec::new();
        if !initial_fts.is_empty() {
            ranked_lists.push(initial_fts);
        }

        // Expand if no strong signal
        if !has_strong_signal && let Ok(terms) = self.expand_query(task).await {
            for term in &terms {
                if let Ok(hits) = self.search.search(term, 20, None).await
                    && !hits.is_empty()
                {
                    ranked_lists.push(hits);
                }
            }
        }

        // RRF fusion + minScore filter
        let fused = search::reciprocal_rank_fusion(&ranked_lists, 20);
        let selected_paths: Vec<std::path::PathBuf> = fused
            .iter()
            .filter(|r| r.score >= crate::query::MIN_SCORE)
            .map(|r| r.path.clone())
            .collect();

        // Fallback: LLM-based selection via index
        let selected_paths = if selected_paths.is_empty() {
            let index_content = match self.read_index() {
                Ok(c) => c,
                Err(_) => return Ok(vec![]),
            };

            if crate::index::is_empty_index(&index_content) {
                return Ok(vec![]);
            }

            match crate::query::select_relevant_pages(
                self.provider.as_ref(),
                self.model(),
                &index_content,
                task,
                "Task",
                CONTEXT_SYSTEM_PROMPT,
            )
            .await
            {
                Ok(paths) => paths,
                Err(e) => {
                    warn!(error = %e, "context_for LLM call failed, returning empty");
                    return Ok(vec![]);
                }
            }
        } else {
            selected_paths
        };

        if selected_paths.is_empty() {
            return Ok(vec![]);
        }

        // Step 4: Read selected pages respecting token budget
        let mut pages = Vec::new();
        let mut tokens_used = 0usize;

        for rel_path in &selected_paths {
            if tokens_used >= max_tokens {
                break;
            }

            let abs_path = self.root.join(rel_path);
            let content = match std::fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let token_estimate = content.len() / crate::model_catalog::BYTES_PER_TOKEN;
            if tokens_used + token_estimate > max_tokens && !pages.is_empty() {
                // Adding this page would exceed budget; skip unless it's the first page
                break;
            }

            // Parse frontmatter
            let (frontmatter, body) = match validate::parse_frontmatter(&content) {
                Ok(pair) => pair,
                Err(_) => continue,
            };

            pages.push(WikiPage {
                path: rel_path.clone(),
                frontmatter,
                body,
            });

            tokens_used += token_estimate;
        }

        Ok(pages)
    }
}

// Tests for parse_page_paths live in query.rs (the canonical home for that function).
